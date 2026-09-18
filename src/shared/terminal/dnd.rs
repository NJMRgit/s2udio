//! Kitty drag & drop protocol (OSC 72, kitty >= 0.47) — round 90.
//!
//! Dragging a file from a file manager into the terminal produces nothing
//! unless the program opts in, and kitty only delivers drops to programs that
//! announced they accept them:
//!
//! ```text
//! accept   (client -> terminal)  ESC ] 72 ; t=a ; <space separated MIME list> ESC \
//! stop     (client -> terminal)  ESC ] 72 ; t=A
//! move     (terminal -> client)  ESC ] 72 ; t=m:x=..:y=..:X=..:Y=..:o=.. ; <MIME list>
//! answer   (client -> terminal)  ESC ] 72 ; t=m:o=<1 copy|2 move|0 no> ; <MIME list>
//! drop     (terminal -> client)  ESC ] 72 ; t=M:... ; <full MIME list>
//! request  (client -> terminal)  ESC ] 72 ; t=r:x=<1-based index in the list>
//! data     (terminal -> client)  ESC ] 72 ; t=r:x=<idx>:m=<1|0> ; <base64 chunk>
//! error    (terminal -> client)  ESC ] 72 ; t=R:x=<idx> ; <POSIX error name[:desc]>
//! finish   (client -> terminal)  ESC ] 72 ; t=r:o=<1|2|0>
//! ```
//!
//! The bytes themselves are parsed by crossterm (patched, see
//! `vendor/crossterm/S2UDIO-PATCH.md`): an OSC string arrives as
//! `Event::Osc(body)` with the `ESC ]` prefix and the terminator already
//! stripped. [`DndReceiver`] drives the client side of the handshake from
//! those bodies: it answers a move, requests the `text/uri-list` on a drop,
//! assembles the base64 chunks and hands the decoded URI list to the paste
//! pipeline. Unknown `t=` values are ignored silently.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use base64::Engine as _;

use crate::shared::terminal::TERMINAL;

/// The OSC code kitty uses for this protocol (`kitty.fast_data_types.DND_CODE`).
pub const DND_CODE: &str = "72";
/// The protocol's own format: a list of URIs, one per line. This is what a
/// file manager hands over.
pub const URI_LIST_MIME: &str = "text/uri-list";
/// Mozilla's link format (`<url>\n<title>`, UTF-16): what a **link dragged out
/// of a Firefox/Zen page** offers. Such a drag does not offer `text/uri-list`
/// at all (measured: `text/x-moz-url`, `_NETSCAPE_URL`, `text/x-moz-url-data`,
/// `text/html`, `text/plain`, …), so a uri-list-only client declines the whole
/// drag and nothing happens.
pub const MOZ_URL_MIME: &str = "text/x-moz-url";
/// The same `<url>\n<title>` layout, UTF-8, used by KDE/GTK programs.
pub const NETSCAPE_URL_MIME: &str = "_NETSCAPE_URL";
/// Last resort: some sources offer nothing but plain text, which for a dragged
/// link is the URL itself.
pub const PLAIN_TEXT_MIME: &str = "text/plain";
/// The MIME types this client accepts, most preferred first.
///
/// Every entry needs a decoder (see [`uri_list_to_text`] / [`url_text_to_text`])
/// because the protocol hands over the data of exactly one of them: the one the
/// client requested. The list is also what the client announces to the terminal
/// and answers a drag offer with.
pub const ACCEPTED_MIMES: [&str; 4] =
    [URI_LIST_MIME, MOZ_URL_MIME, NETSCAPE_URL_MIME, PLAIN_TEXT_MIME];
/// Cap on the assembled payload, so a hostile or broken terminal cannot grow
/// memory without bound.
const MAX_PAYLOAD_LEN: usize = 1024 * 1024;
/// Requests allowed for one drop, spread over its candidate MIME types.
///
/// A fresh drop's OS transfer can answer the first request(s) with an **empty**
/// payload and deliver the data a moment later (measured on kitty 0.48.2: two
/// empty replies, then the payload on the third request of the same drag; a
/// file drag, which has a single candidate, got nothing at all). One request
/// per candidate is therefore not enough.
const MAX_DROP_ATTEMPTS: u32 = 6;
/// How long to wait before asking again after an empty reply: long enough for
/// the transfer to come up, short enough to stay invisible.
const RETRY_DELAY: Duration = Duration::from_millis(120);

/// The accepted MIME types that are actually in *offered*, in this client's
/// order of preference — the list to answer a drag offer with.
fn accepted_subset(offered: &[String]) -> Vec<&'static str> {
    ACCEPTED_MIMES.iter().copied().filter(|accepted| offered.iter().any(|m| m == accepted)).collect()
}

/// One decoded OSC 72 event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DndEvent {
    /// `t=m`: the drag moved over the window (`left` when it left it, which
    /// kitty reports as `x=-1:y=-1`). `mimes` is the offered MIME list, which
    /// kitty sends on the first move and whenever it changes.
    Move {
        left: bool,
        mimes: Option<Vec<String>>,
    },
    /// `t=M`: the user dropped; the MIME list is mandatory.
    Dropped { mimes: Vec<String> },
    /// `t=r`: a chunk of requested data.
    Data {
        chunks: Vec<Vec<u8>>,
        index: Option<u32>,
        more: bool,
    },
    /// `t=R`: the terminal refused the data request (`ENOENT`, `EPERM`, …).
    Error { index: Option<u32> },
    /// Any other `t=` (the protocol's drag-source half, a malformed or
    /// unknown code): ignored silently by the receiver.
    Unknown,
}

impl DndEvent {
    /// One-line description for the debug log (`t=m left=false mimes=[…]`).
    /// The handshake is otherwise silent, which makes a broken drop
    /// impossible to tell from a terminal that never sent one.
    fn describe(&self) -> String {
        match self {
            DndEvent::Move { left, mimes } => format!("t=m left={left} mimes={mimes:?}"),
            DndEvent::Dropped { mimes } => format!("t=M mimes={mimes:?}"),
            DndEvent::Data { chunks, index, more } => format!(
                "t=r index={index:?} more={more} bytes={}",
                chunks.iter().map(Vec::len).sum::<usize>()
            ),
            DndEvent::Error { index } => format!("t=R index={index:?}"),
            DndEvent::Unknown => "unknown".to_owned(),
        }
    }
}

/// Decode the body of one `OSC 72 ; <metadata> ; <payload>` escape code.
/// `body` is everything after `"72;"`, with the terminating `ESC \` (or
/// `BEL`) already stripped. `None` when the sequence is not decodable at all
/// (for example a payload that is not valid UTF-8, or a `t=r` chunk that is
/// not valid base64).
pub fn parse_dnd_escape(body: &str) -> Option<DndEvent> {
    // `ESC ] 72 ; t=a ; text/uri-list`: `body` is `t=a ; text/uri-list`.
    let (metadata, payload) = match body.split_once(';') {
        Some((metadata, payload)) => (metadata, payload),
        // No payload at all (`t=A`, `t=q`): the metadata list may still be
        // there, but a trailing separator means `t=a;` — both forms reach the
        // same parse.
        None => (body.strip_suffix(';').unwrap_or(body), ""),
    };
    let fields: Vec<&str> = metadata.split(':').filter(|f| !f.is_empty()).collect();
    let Some((type_field, options)) = fields.split_first() else {
        return Some(DndEvent::Unknown);
    };
    // The event type is a `t=<char>` key/value pair, not a bare character:
    // `t=m:x=3:y=4:o=1` -> `m` with `x=3`, `y=4`, `o=1` as the options.
    let Some(event_type) = type_field.strip_prefix("t=") else {
        return None;
    };
    match event_type {
        "m" => {
            let mut left = false;
            for option in options {
                let Some((key, value)) = option.split_once('=') else {
                    continue;
                };
                // Only the leave-the-window event carries a negative cell.
                if key == "x" && value == "-1" {
                    left = true;
                }
            }
            let mimes = if payload.trim().is_empty() {
                None
            } else {
                Some(payload.split_whitespace().map(str::to_owned).collect())
            };
            Some(DndEvent::Move { left, mimes })
        }
        "M" => Some(DndEvent::Dropped {
            mimes: payload.split_whitespace().map(str::to_owned).collect(),
        }),
        "r" => {
            let mut index = None;
            let mut more = false;
            for option in options {
                let Some((key, value)) = option.split_once('=') else {
                    continue;
                };
                match key {
                    // 1-based index into the MIME list of the drop.
                    "x" => index = value.parse::<u32>().ok(),
                    // `m=1` marks a non-final chunk.
                    "m" => more = value == "1",
                    _ => {}
                }
            }
            let chunks = if payload.is_empty() {
                Vec::new()
            } else {
                vec![base64::engine::general_purpose::STANDARD
                    .decode(payload)
                    .ok()?]
            };
            Some(DndEvent::Data {
                chunks,
                index,
                more,
            })
        }
        "R" => {
            let mut index = None;
            for option in options {
                if let Some((key, value)) = option.split_once('=')
                    && key == "x"
                {
                    index = value.parse::<u32>().ok();
                }
            }
            Some(DndEvent::Error { index })
        }
        _ => Some(DndEvent::Unknown),
    }
}

/// What handling one OSC 72 body told the input loop to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DndOutcome {
    /// Nothing to hand on: the event was part of the handshake (or noise).
    Handled,
    /// The dropped `text/uri-list` finished arriving and must go through the
    /// app's normal paste pipeline (`AppEvent::UserPaste`).
    Paste(String),
    /// The drop arrived and was answered, but the terminal had no data for any
    /// MIME type this client can decode (measured: a browser drag that answered
    /// every request with an empty payload). Nothing to paste — but the gesture
    /// reached the app, so the user is told instead of seeing nothing at all.
    Empty,
}

/// The client side of the protocol: the drop handshake, the chunked payload
/// assembly and the decoding of whatever the drop turned out to be. The escape
/// framing itself is crossterm's job.
#[derive(Default)]
pub struct DndReceiver {
    /// 1-based MIME index the pending data belongs to (as sent in the drop
    /// event) — the terminal validates it against its own list.
    requested: Option<u32>,
    /// MIME type that index names: decides which decoder the payload gets.
    requested_mime: Option<String>,
    /// MIME types of the current drop this client can decode, in preference
    /// order, rotated as empty replies come back. A terminal that cannot fetch
    /// the offered payload does not report an error — measured: both a browser
    /// link drag and a file drag answered requests with an **empty** payload —
    /// so the next candidate is tried before the drop is given up.
    candidates: VecDeque<(u32, &'static str)>,
    /// Requests still allowed for this drop (see [`MAX_DROP_ATTEMPTS`]).
    attempts_left: u32,
    /// When the next request may go out (see [`RETRY_DELAY`]).
    retry_at: Option<Instant>,
    /// Chunks received so far.
    chunks: Vec<Vec<u8>>,
    /// Accumulated size of `chunks`.
    len: usize,
    /// The offer that was already answered. kitty repeats the MIME list with
    /// **every** move event (up to hundreds per drag), so re-answering per
    /// event would flood the terminal with escape codes for one gesture; a
    /// repeated list means the offer is unchanged.
    answered_offer: Option<Vec<String>>,
}

impl DndReceiver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Handle one complete OSC body (`Event::Osc`), with the `ESC ]` and the
    /// terminator stripped.
    ///
    /// Returns [`DndOutcome::Paste`] only for the text that has just finished
    /// arriving; every other event is answered on the wire and handed back as
    /// [`DndOutcome::Handled`].
    pub fn handle_body(&mut self, body: &[u8]) -> DndOutcome {
        let Ok(body) = std::str::from_utf8(body) else {
            // Not one of ours (or not text): ignored silently.
            return DndOutcome::Handled;
        };
        // Only OSC 72 is ours; every other OSC belongs to another protocol.
        let Some(body) = body.strip_prefix(&format!("{DND_CODE};")) else {
            return DndOutcome::Handled;
        };
        match parse_dnd_escape(body) {
            Some(event) => self.act(event),
            // Undecodable: ignored, exactly like an unknown `t=`.
            None => DndOutcome::Handled,
        }
    }

    /// Apply one decoded event to the handshake state, answering on the wire
    /// where the protocol requires it.
    fn act(&mut self, event: DndEvent) -> DndOutcome {
        // kitty repeats the offer with **every** move event (hundreds per
        // drag): a repeat is dropped before it reaches the log, so the log
        // stays readable and one drag does not fill it.
        if let DndEvent::Move { left: false, mimes: Some(mimes) } = &event
            && self.answered_offer.as_ref() == Some(mimes)
        {
            return DndOutcome::Handled;
        }
        let described = event.describe();
        log::debug!(event = described.as_str(); "OSC 72 event");
        match event {
            DndEvent::Move { left, mimes } => {
                // The drag left the window: the next drag is a new offer, but
                // there is nothing to answer here.
                if left {
                    self.answered_offer = None;
                    return DndOutcome::Handled;
                }
                // A move without a list repeats the offer this drag already
                // carried (the terminal sends the list again when it changes).
                let Some(mimes) = mimes else {
                    return DndOutcome::Handled;
                };
                self.answered_offer = Some(mimes.clone());
                let accepted = accepted_subset(&mimes);
                if accepted.is_empty() {
                    // Nothing in the offer this client can use (a drag of rich
                    // text or a widget, say): decline explicitly, so the OS
                    // shows "not allowed" instead of accepting a drop whose
                    // data this client would have to throw away.
                    log::debug!(mimes:? = mimes; "Drag offer has no usable MIME type");
                    self.write(&format!("]{DND_CODE};t=m:o=0"));
                    return DndOutcome::Handled;
                }
                // Answer the offer: the terminal only knows the drop is wanted
                // once the client says so, and it needs to know which of the
                // offered MIME types the data should be handed over as.
                log::debug!(accepted:? = accepted; "Accepted the drag offer");
                self.write(&format!("]{DND_CODE};t=m:o=1;{}", accepted.join(" ")));
                DndOutcome::Handled
            }
            DndEvent::Dropped { mimes } => {
                self.answered_offer = None;
                self.reset_transfer();
                self.attempts_left = MAX_DROP_ATTEMPTS;
                // Every accepted type this drop offers, most preferred first:
                // the index is 1-based into the drop's own MIME list.
                self.candidates = ACCEPTED_MIMES
                    .iter()
                    .filter_map(|accepted| {
                        mimes
                            .iter()
                            .position(|offered| offered == accepted)
                            .map(|index| (index as u32 + 1, *accepted))
                    })
                    .collect();
                if !self.request_next() {
                    // A drop that offers only types this client cannot decode:
                    // no request, the terminal cancels it.
                    log::debug!(mimes:? = mimes; "Drop offers no usable MIME type");
                }
                DndOutcome::Handled
            }
            DndEvent::Error { index } => {
                // The terminal refused the data (EPERM for a drag that started
                // in this same window, ENOENT for an out-of-range index, …).
                // Per the protocol an error terminates the drop, so there is
                // no fallback here — only a report, so a dead drop is visible
                // in the log.
                log::debug!(index:? = index; "Drop data request failed");
                self.reset_transfer();
                DndOutcome::Handled
            }
            DndEvent::Data {
                chunks,
                index,
                more,
            } => {
                if self.requested.is_none() || index.is_some() && index != self.requested {
                    // Data for a MIME type this client never asked for, or for
                    // a transfer that already ended.
                    return DndOutcome::Handled;
                }
                for chunk in chunks {
                    self.len += chunk.len();
                    self.chunks.push(chunk);
                }
                if self.len > MAX_PAYLOAD_LEN {
                    // A broken or hostile sender: stop the transfer instead of
                    // growing memory without bound.
                    log::warn!(len = self.len; "Dropped an oversized drop payload");
                    self.reset_transfer();
                    return DndOutcome::Handled;
                }
                if more {
                    return DndOutcome::Handled;
                }
                let payload = std::mem::take(&mut self.chunks);
                let raw = payload.concat();
                let mime = self.requested_mime.take();
                self.len = 0;
                self.requested = None;
                if raw.is_empty() {
                    // The terminal answered with no data: measured on a fresh
                    // drop this can be the transfer not being up yet, and a
                    // moment later the same drag delivers — so keep asking
                    // (rotating through the candidate types) until the attempts
                    // run out.
                    if self.attempts_left > 0 {
                        self.retry_at = Some(Instant::now() + RETRY_DELAY);
                        log::debug!(mime:? = mime, attempts_left = self.attempts_left; "Drop reply carried no data, asking again");
                        return DndOutcome::Handled;
                    }
                    return self.give_up();
                }
                // The final action: copy.
                self.write(&format!("]{DND_CODE};t=r:o=1"));
                let text = decode_drop(mime.as_deref(), &raw);
                log::debug!(
                    mime:? = mime, bytes = raw.len(), text = text.as_str();
                    "Dropped payload decoded"
                );
                self.candidates.clear();
                // Nothing usable in the payload (an unsupported URI scheme, a
                // dragged selection that is not a path or a link): no paste,
                // rather than an empty one.
                if text.is_empty() {
                    return DndOutcome::Handled;
                }
                DndOutcome::Paste(text)
            }
            DndEvent::Unknown => DndOutcome::Handled,
        }
    }

    /// Forget the transfer in progress (but keep the candidate list: a
    /// fallback request may still be pending).
    fn reset_transfer(&mut self) {
        self.requested = None;
        self.requested_mime = None;
        self.chunks.clear();
        self.len = 0;
        self.retry_at = None;
    }

    /// Ask the terminal for the next candidate type of the current drop,
    /// rotating the queue so an empty reply moves on to the next type (and
    /// comes back to this one on the next attempt). Returns whether a request
    /// was sent.
    fn request_next(&mut self) -> bool {
        self.reset_transfer();
        self.retry_at = None;
        if self.attempts_left == 0 {
            return false;
        }
        let Some((index, mime)) = self.candidates.pop_front() else {
            return false;
        };
        self.candidates.push_back((index, mime));
        self.attempts_left -= 1;
        self.requested = Some(index);
        self.requested_mime = Some(mime.to_owned());
        self.write(&format!("]{DND_CODE};t=r:x={index}"));
        true
    }

    /// Send the retry that is due, if any. Returns an outcome when the drop
    /// has been given up on.
    pub fn tick(&mut self) -> Option<DndOutcome> {
        if self.retry_at.is_some_and(|at| Instant::now() >= at)
            && !self.request_next()
        {
            return Some(self.give_up());
        }
        None
    }

    /// How long the input loop may sleep before [`DndReceiver::tick`] has work.
    pub fn next_timeout(&self) -> Option<Duration> {
        self.retry_at
            .map(|at| at.saturating_duration_since(Instant::now()))
    }

    /// The terminal had nothing for the drop: tell it the drop is over
    /// (operation 0 = cancelled) and report it to the caller.
    fn give_up(&mut self) -> DndOutcome {
        self.write(&format!("]{DND_CODE};t=r:o=0"));
        log::debug!("Drop delivered no data");
        self.reset_transfer();
        self.candidates.clear();
        DndOutcome::Empty
    }

    /// Write an escape code to the terminal (best effort: a failed write only
    /// means the drop is not answered).
    fn write(&self, sequence: &str) {
        match TERMINAL.write_escape(sequence) {
            Ok(()) => log::debug!(sequence; "Sent an OSC 72 escape code"),
            Err(err) => log::debug!(err:?; "Failed to send an OSC 72 escape code"),
        }
    }
}

/// Decode one complete drop payload into the text the paste pipeline expects.
///
/// The MIME type decides the layout: `text/uri-list` is a list of URIs (one
/// per line), the Mozilla/Netscape link formats are `<url>\n<title>`, and
/// `text/plain` is whatever the source thought was useful (for a dragged link:
/// the URL).
fn decode_drop(mime: Option<&str>, raw: &[u8]) -> String {
    match mime {
        Some(URI_LIST_MIME) => uri_list_to_text(&String::from_utf8_lossy(raw)),
        Some(MOZ_URL_MIME | NETSCAPE_URL_MIME) => url_text_to_text(raw),
        _ => String::from_utf8_lossy(raw).into_owned(),
    }
}

/// Turn a decoded `text/uri-list` payload into the text the paste pipeline
/// expects, one entry per line.
///
/// `file://` URIs become local paths: the scheme is stripped, `localhost` is
/// dropped, and the path is percent-decoded (kitty escapes spaces as `%20`).
/// `http(s)` links are kept verbatim, so a link dragged out of a browser takes
/// exactly the same path as a pasted one — round-90 regression 6 was that they
/// were dropped as "not a file". Comment lines (`#`) and anything with an
/// unknown scheme are skipped; the paste pipeline decides what is playable.
fn uri_list_to_text(payload: &str) -> String {
    payload.lines().filter_map(uri_entry).collect::<Vec<_>>().join("\n")
}

/// One entry of a URI list as the paste pipeline wants it: a local path for a
/// `file://` URI, the link verbatim for `http(s)`, and `None` for a comment,
/// an empty line or a scheme this client does not understand.
fn uri_entry(line: &str) -> Option<String> {
    let line = trim_nuls(line);
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    if line.starts_with("http://") || line.starts_with("https://") {
        return Some(line.to_owned());
    }
    let rest = line.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let bytes = percent_decode(rest.as_bytes());
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// `<url>\n<title>` — Mozilla's `text/x-moz-url` (UTF-16) and KDE/GTK's
/// `_NETSCAPE_URL` (UTF-8), the formats a link dragged out of a browser
/// arrives in. Only the first line is the URL; the title behind it is for a
/// human and the paste pipeline has no use for it.
fn url_text_to_text(raw: &[u8]) -> String {
    let text = decode_text_payload(raw);
    let Some(url) = first_line(&text) else {
        return String::new();
    };
    uri_entry(&url).unwrap_or(url)
}

/// Decode a payload that may be UTF-16 or UTF-8.
///
/// `text/x-moz-url` is UTF-16 from Firefox — decoded as UTF-8 every second
/// byte turns into a replacement character, so the pasted link would be
/// garbage. A byte order mark decides the endianness; without one, the
/// NUL-in-every-second-byte shape of UTF-16 ASCII text decides it.
fn decode_text_payload(raw: &[u8]) -> String {
    match raw {
        [0xFF, 0xFE, rest @ ..] => decode_utf16(rest, u16::from_le_bytes),
        [0xFE, 0xFF, rest @ ..] => decode_utf16(rest, u16::from_be_bytes),
        _ if looks_like_utf16(raw) => decode_utf16(raw, u16::from_le_bytes),
        _ => String::from_utf8_lossy(raw).into_owned(),
    }
}

/// The UTF-16 reading of *raw*: pairs of bytes, odd trailing byte dropped.
fn decode_utf16<F: Fn([u8; 2]) -> u16>(raw: &[u8], pair: F) -> String {
    String::from_utf16_lossy(
        &raw.chunks_exact(2).map(|c| pair([c[0], c[1]])).collect::<Vec<u16>>(),
    )
}

/// Whether *raw* looks like UTF-16LE text without a byte order mark: every
/// second byte of ASCII text encoded that way is NUL.
fn looks_like_utf16(raw: &[u8]) -> bool {
    let pairs = raw.len() / 2;
    pairs > 0 && raw.iter().skip(1).step_by(2).filter(|byte| **byte == 0).count() * 2 >= pairs
}

/// The first line with content, trimmed — `first_line` of `<url>\n<title>`.
fn first_line(text: &str) -> Option<String> {
    text.lines().map(trim_nuls).find(|line| !line.is_empty()).map(str::to_owned)
}

/// Trim whitespace and the NULs a padded UTF-16 payload can carry.
fn trim_nuls(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || c == '\0')
}

/// Percent-decode (UTF-8 aware), doubling as kitty's drag & drop escaping.
/// Malformed escapes are left exactly as they arrived.
fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut idx = 0;
    while idx < input.len() {
        if input[idx] == b'%' && idx + 2 < input.len() {
            let hex = |byte: u8| (byte as char).to_digit(16).map(|digit| digit as u8);
            if let (Some(high), Some(low)) = (hex(input[idx + 1]), hex(input[idx + 2])) {
                out.push(high * 16 + low);
                idx += 3;
                continue;
            }
        }
        out.push(input[idx]);
        idx += 1;
    }
    out
}
