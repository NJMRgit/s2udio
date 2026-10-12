use std::time::Duration;

use crossbeam::channel::Sender;
use crossterm::event::Event;

use crate::shared::{
    events::AppEvent,
    mouse_event::{MouseEvent, MouseEventKind, MouseEventTracker},
    terminal::dnd::{DndOutcome, DndReceiver},
};

pub fn init(event_tx: Sender<AppEvent>) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("input".to_owned()).spawn(move || input_poll_task(&event_tx))
}

/// Read the system clipboard and feed it through the paste pipeline. Runs
/// on a background thread so a slow clipboard owner never blocks the UI;
/// the clipboard tool is wrapped in a timeout for the same reason.
///
/// Every paste gesture — middle click, Ctrl+V, Ctrl+Shift+V — goes through
/// this same function, so they always paste the same text: the clipboard
/// when it holds something the paste pipeline recognizes, the primary
/// selection only as a fallback. A middle click must not silently paste a
/// *different* selection than Ctrl+V: in practice the primary selection
/// holds whatever the user last selected elsewhere (a label, a file path),
/// while the clipboard holds what they actually copied — a middle click
/// pasting that file path instead of the copied web stream is the bug this
/// ordering exists to prevent.
fn read_clipboard_and_paste(event_tx: Sender<AppEvent>) {
    std::thread::Builder::new()
        .name("clipboard".to_owned())
        .spawn(move || {
            let text = read_clipboard_for_paste();
            if let Some(text) = text
                && !text.trim().is_empty()
            {
                let _ = event_tx.send(AppEvent::UserPaste(text));
            }
        })
        .ok();
}

/// The text a paste gesture pastes: the clipboard, with the primary
/// selection as a fallback when the clipboard holds nothing the paste
/// pipeline recognizes. The decision is a single order for every gesture
/// (see `read_clipboard_and_paste` for why), so the two gestures can never
/// disagree about what "the clipboard" holds.
fn read_clipboard_for_paste() -> Option<String> {
    choose_text(read_clipboard(false), read_clipboard(true))
}

/// The selection decision, split out from the shelling out so it can be
/// tested: the clipboard when it holds something pasteable (a copied link
/// lives there — Ctrl+C fills the clipboard, not the primary selection),
/// the primary selection when it does not, and the clipboard's text when
/// neither is pasteable (so the caller's emptiness check is the only thing
/// that decides).
fn choose_text(clipboard: Option<String>, primary: Option<String>) -> Option<String> {
    if is_pasteable(clipboard.as_deref()) {
        return clipboard;
    }
    if is_pasteable(primary.as_deref()) {
        return primary;
    }
    clipboard.or(primary)
}

/// Whether the text holds at least one item the paste pipeline recognizes.
/// `None` (no selection) is not pasteable.
fn is_pasteable(text: Option<&str>) -> bool {
    text.is_some_and(|t| !crate::ui::modals::paste::parse_paste(t).is_empty())
}

/// Fetch the clipboard contents: Wayland (`wl-paste`), then X11
/// (`xclip` / `xsel`) as fallbacks. `primary` reads the primary selection
/// (middle-click); otherwise the clipboard proper (Ctrl+V).
fn read_clipboard(primary: bool) -> Option<String> {
    use std::process::{Command, Stdio};
    let run = |program: &str, args: &[&str]| {
        // `timeout` keeps a stalled clipboard owner from hanging the
        // helper forever (a dangling app holding the Wayland clipboard
        // never answers).
        Command::new("timeout")
            .args(["2", program])
            .args(args)
            .stdin(Stdio::null())
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let text = if primary {
        run("wl-paste", &["--primary", "--no-newline"])
    } else {
        run("wl-paste", &["--no-newline"])
    }
    .or_else(|| run("xclip", &["-selection", if primary { "primary" } else { "clipboard" }, "-o"]))
    .or_else(|| run("xsel", &[if primary { "--primary" } else { "--clipboard" }, "--output"]))?;
    if text.trim().is_empty() { None } else { Some(text) }
}

/// Hand one drag & drop outcome to the app: a pasted payload goes through the
/// ordinary paste pipeline, a drop the terminal had no data for is reported
/// (the gesture reached the app, so saying nothing looks like a dead feature),
/// and a handshake step needs nothing.
fn handle_dnd_outcome(outcome: DndOutcome, event_tx: &Sender<AppEvent>) {
    match outcome {
        DndOutcome::Paste(text) => {
            if let Err(err) = event_tx.send(AppEvent::UserPaste(text)) {
                log::error!(error:? = err; "Failed to send dropped text");
            }
        }
        DndOutcome::Empty => {
            let _ = event_tx.send(AppEvent::Status(
                "The dropped item carried no data".to_owned(),
                crate::shared::events::Level::Warn,
                Duration::from_secs(4),
            ));
        }
        DndOutcome::Handled => {}
    }
}

fn input_poll_task(event_tx: &Sender<AppEvent>) {
    // Sometimes in there are inputs left in the buffer(because of tmux maybe?)
    // before starting to read inputs (from reading terminal sequences), this
    // results in random stuff happening in the program. Simply drain them.
    drain_crossterm_events();

    let mut mouse_event_tracker = MouseEventTracker::default();
    // Round 90: the client side of kitty's drag & drop protocol (OSC 72).
    // Its escape codes arrive as `Event::Osc` — crossterm is patched to parse
    // OSC strings, see vendor/crossterm/S2UDIO-PATCH.md.
    let mut dnd = DndReceiver::new();
    loop {
        // Round 90.5: while a drop's data request is waiting to be retried the
        // poll has to wake early — the transport can answer the first request
        // of a fresh drop with nothing and deliver a moment later.
        let timeout = dnd.next_timeout().unwrap_or(Duration::from_millis(250));
        match crossterm::event::poll(timeout) {
            Ok(true) => match crossterm::event::read() {
                Ok(Event::Mouse(mouse)) => {
                    // Middle click pastes the same text as Ctrl+V: with mouse
                    // capture enabled the terminal no longer does its own
                    // middle-click paste, so read the clipboard and feed the
                    // same paste pipeline (the event is consumed here). The
                    // primary selection is only a fallback — see
                    // `read_clipboard_and_paste`.
                    if matches!(
                        mouse.kind,
                        crossterm::event::MouseEventKind::Down(
                            crossterm::event::MouseButton::Middle
                        )
                    ) {
                        read_clipboard_and_paste(event_tx.clone());
                        continue;
                    }
                    if let Some(ev) = mouse_event_tracker.track_and_get(mouse)
                        && let Err(err) = event_tx.send(AppEvent::UserMouseInput(ev))
                    {
                        log::error!(error:? = err; "Failed to send user mouse input");
                    }
                }
                Ok(Event::Key(key)) => {
                    // Ctrl+V pastes the clipboard (kitty does not remap it
                    // by default, and with raw mode the terminal sends the
                    // bare control code): read the clipboard and feed the
                    // paste pipeline. Ctrl+Shift+V is matched here too — it
                    // arrives as `Char('V')` with SHIFT (or as the bare
                    // `\x16` control code) — so the gesture has the same
                    // source as Ctrl+V instead of depending on whether the
                    // terminal happens to turn it into a bracketed paste.
                    let ctrl = key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL);
                    let is_ctrl_v = ctrl
                        && matches!(key.code, crossterm::event::KeyCode::Char('v' | 'V' | '\x16'));
                    if is_ctrl_v {
                        read_clipboard_and_paste(event_tx.clone());
                        continue;
                    }
                    if let Err(err) = event_tx.send(AppEvent::UserKeyInput(key)) {
                        log::error!(error:? = err; "Failed to send user input");
                    }
                }
                Ok(Event::Resize(columns, rows)) => {
                    if let Err(err) = event_tx.send(AppEvent::Resized { columns, rows }) {
                        log::error!(error:? = err; "Failed to render request after resize");
                    }
                }
                // Bracketed paste: terminal drag&dropped files/URLs (and
                // Ctrl+Shift+V-style terminal pastes) arrive here. Handed to
                // the UI, which offers the play/enqueue popup when the
                // content looks like audio/video. Middle-click and Ctrl+V are
                // handled above (mouse capture swallows the terminal's own
                // middle-click paste, and kitty sends Ctrl+V as a literal
                // control code).
                Ok(Event::Paste(text)) => {
                    if let Err(err) = event_tx.send(AppEvent::UserPaste(text)) {
                        log::error!(error:? = err; "Failed to send user paste");
                    }
                }
                // Round 90: a terminal-to-client OSC. Only OSC 72 (kitty's
                // drag & drop) is acted on; the handshake answers the drag
                // over the wire, and a completed drop is fed into the same
                // paste pipeline a pasted path or link takes.
                Ok(Event::Osc(body)) => {
                    // Round 90: log the body (truncated — a data chunk can be
                    // kilobytes of base64). The handshake is silent otherwise,
                    // which makes a drop that never arrives impossible to tell
                    // from one the terminal never sent.
                    let osc = String::from_utf8_lossy(&body)
                        .chars()
                        .take(160)
                        .collect::<String>();
                    log::debug!(osc = osc.as_str(); "Terminal OSC received");
                    handle_dnd_outcome(dnd.handle_body(&body), event_tx);
                }
                Ok(Event::FocusLost) => {
                    // The window lost focus: the pointer can be anywhere now,
                    // so clear the hover position (the 65535 leave convention
                    // covers most terminals; this is the rest).
                    let ev = MouseEvent {
                        x: u16::MAX,
                        y: u16::MAX,
                        kind: MouseEventKind::Moved,
                        modifiers: crossterm::event::KeyModifiers::NONE,
                    };
                    if let Err(err) = event_tx.send(AppEvent::UserMouseInput(ev)) {
                        log::error!(error:? = err; "Failed to send focus-lost mouse input");
                    }
                }
                // Focus regain needs no action (the next mouse move
                // refreshes the hover position).
                Ok(Event::FocusGained) => {}
                Err(err) => {
                    log::warn!(error:? = err; "Failed to read input event");
                }
            },
            Ok(_) => {}
            Err(e) => log::warn!(error:? = e; "Error when polling for event"),
        }
        // A drop whose data request is waiting for its retry (round 90.5).
        if let Some(outcome) = dnd.tick() {
            handle_dnd_outcome(outcome, event_tx);
        }
    }
}

fn drain_crossterm_events() {
    while crossterm::event::poll(Duration::from_millis(0)).unwrap_or(false) {
        let _ = crossterm::event::read();
    }
}

#[cfg(test)]
mod tests {
    use super::{choose_text, is_pasteable};

    // URLs/paths the paste pipeline recognizes, so "pasteable" is true.
    // A bare local path needs a real file on disk; a URL with an audio
    // extension classifies without touching the disk.
    const YT: &str = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";
    const AUDIO: &str = "https://example.com/song.mp3";
    const MAGNET: &str = "magnet:?xt=urn:btih:deadbeef";

    fn s(v: &str) -> Option<String> {
        Some(v.to_owned())
    }

    #[test]
    fn clipboard_wins_when_pasteable() {
        // The user's reported case: the clipboard holds the copied web
        // stream and the primary selection holds a file path — a paste
        // gesture must paste the clipboard, never the primary selection.
        assert_eq!(choose_text(s(YT), s(AUDIO)), s(YT));
        assert_eq!(choose_text(s(MAGNET), s(YT)), s(MAGNET));
    }

    #[test]
    fn falls_back_to_primary_when_clipboard_is_not_pasteable() {
        // Clipboard junk (a label, plain text, whitespace) or empty: the
        // primary selection is the fallback.
        assert_eq!(choose_text(s("this page"), s(YT)), s(YT));
        assert_eq!(choose_text(None, s(YT)), s(YT));
        assert_eq!(choose_text(s("  \n  "), s(YT)), s(YT));
    }

    #[test]
    fn neither_pasteable_keeps_clipboard_text() {
        // Both selections junk: the clipboard's text wins, because the
        // caller's emptiness check is the only gate; the paste pipeline
        // drops it anyway.
        assert_eq!(choose_text(s("this page"), s("another label")), s("this page"));
        assert_eq!(choose_text(None, None), None);
    }

    #[test]
    fn is_pasteable_recognizes_real_items() {
        assert!(is_pasteable(s(YT).as_deref()));
        assert!(is_pasteable(s(AUDIO).as_deref()));
        assert!(is_pasteable(s(MAGNET).as_deref()));
        assert!(!is_pasteable(s("this page").as_deref()));
        assert!(!is_pasteable(s("  ").as_deref()));
        assert!(!is_pasteable(None));
    }
}
