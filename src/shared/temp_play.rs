//! Temporary, queue-free playback of one stream or file through MPD.
//!
//! A temp play is `addid` + `playid`: MPD gets one queue row, starts it, and
//! the app hides that row from the Queue pane and drops it again when playback
//! moves on (`backend/mpd-playback`). The two commands are **one transaction**
//! here: when MPD refuses to open the media — a station answering `503`, an
//! unreachable host, a reset stream — the row `addid` just created is deleted
//! again *before* the failure is returned.
//!
//! The cleanup has to happen inside the request because only an `Ok` result
//! reaches the pane that records the temp id. On MPD 0.24 a `playid` that
//! cannot open the stream answers the command with an ACK while the row it
//! names stays in the playlist (measured 2026-10-24 against the user's MPD),
//! so a failed play used to leave an entry that belonged to nobody: the Queue
//! pane filters stream rows, so it could not be seen, and no cleanup path knew
//! its id, so it could not be removed. Retrying a station that was down added
//! one more dead row each time, and MPD later walked over them — the
//! 2026-10-24 report "web radio streams are not always reliably playing when
//! selected" (`docs/design/Sessions/2026-10-24.md`).

use anyhow::Result;

use crate::mpd::{errors::MpdError, mpd_client::MpdClient};

/// `addid` + `playid` as one transaction, returning the new queue id.
///
/// `label` is what the status line calls the media (`"CALMRADIO.COM - Solo
/// Piano"`, a file name): the failure reads `Could not play <label> — <reason>`
/// instead of MPD's internal `Cannot execute command: 'playid'. Detail: …`.
///
/// A failure leaves **no row** behind: the entry this call added is deleted
/// before the error is returned (best-effort — the `deleteid` can itself fail
/// when MPD already dropped the row, and that is not worth a second error).
pub(crate) fn add_and_play(
    client: &mut impl MpdClient,
    uri: &str,
    label: &str,
) -> Result<u32> {
    let id = client
        .add_id(uri, None)
        .map_err(|err| anyhow::anyhow!("Could not queue {label} — {}", failure_reason(&err)))?;
    if let Err(err) = client.play_id(id) {
        if let Err(delete) = client.delete_id(id) {
            log::debug!(
                error:? = delete, id, uri;
                "Could not drop the row of a temporary play MPD refused to start"
            );
        }
        return Err(anyhow::anyhow!("Could not play {label} — {}", failure_reason(&err)));
    }
    Ok(id)
}

/// The human half of an MPD failure.
///
/// A decoder failure carries the useful part after the URI it names —
/// `Failed to decode "http://…"; got HTTP status 503` — so only that reason is
/// kept; every other failure is passed through as MPD wrote it.
pub(crate) fn failure_reason(err: &MpdError) -> String {
    let detail = err.detail_or_display();
    match detail.split_once("\"; ") {
        Some((_, reason)) => reason.trim().to_owned(),
        None => detail,
    }
}

#[cfg(test)]
mod tests {
    use super::{add_and_play, failure_reason};
    use crate::mpd::errors::{ErrorCode, MpdError, MpdFailureResponse};
    use crate::tests::fixtures::mpd_client::client;

    fn decoder_message(uri: &str, reason: &str) -> String {
        format!("Failed to decode \"{uri}\"; {reason}")
    }

    fn decode_failure(uri: &str, reason: &str) -> MpdError {
        MpdError::Mpd(MpdFailureResponse {
            code: ErrorCode::UnknownCmd,
            command_list_index: 0,
            command: "playid".to_owned(),
            message: decoder_message(uri, reason),
        })
    }

    /// The status line gets the decoder's reason, not the whole MPD failure.
    #[test]
    fn a_decode_failure_reports_only_the_decoder_reason() {
        let err = decode_failure("http://streams.calmradio.com:1228/", "got HTTP status 503");
        assert_eq!(failure_reason(&err), "got HTTP status 503");
    }

    /// Any other failure is reported the way MPD wrote it — there is no URI to
    /// strip it at.
    #[test]
    fn a_plain_failure_is_passed_through() {
        let err = MpdError::Generic("playlist is full".to_owned());
        assert_eq!(failure_reason(&err), "GenericError: 'playlist is full'");
    }

    /// The failed play leaves the queue exactly as it found it: the row
    /// `addid` created is gone again, and the error names the media.
    #[test]
    fn a_refused_play_drops_the_row_it_added() {
        let mut client = client();
        let id = client.next_id;
        client.fail_play_with = Some(decoder_message(
            "http://station.example/stream",
            "got HTTP status 503",
        ));

        let err = add_and_play(&mut client, "http://station.example/stream", "Test FM")
            .expect_err("a refused play is an error");

        assert_eq!(err.to_string(), "Could not play Test FM — got HTTP status 503");
        assert!(client.queue.is_empty(), "the row of the refused play is gone");
        assert_eq!(client.deleted_ids, vec![id], "the row the addid created is deleted");
        assert_eq!(client.calls.get("play_id"), Some(&1), "playid was still attempted");
    }

    /// The happy path keeps the row and hands its id back.
    #[test]
    fn a_started_play_keeps_its_row() {
        let mut client = client();
        let expected = client.next_id;
        let id = add_and_play(&mut client, "http://station.example/stream", "Test FM")
            .expect("the play starts");
        assert_eq!(id, expected);
        assert_eq!(client.queue.len(), 1, "the row stays queued");
        assert!(client.deleted_ids.is_empty(), "nothing is dropped");
        assert_eq!(client.current_song_idx, Some(0), "the row is the playing one");
        let row = client.queue[0];
        assert_eq!(client.songs[row].file, "http://station.example/stream");
        assert_eq!(client.songs[row].id, expected);
    }
}
