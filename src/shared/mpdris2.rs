//! s2udio <-> mpDris2 bridge state (round 31).
//!
//! mpDris2 (the MPRIS bridge for MPD, run here through the `s2u-mpdris2`
//! shim) shows its own "Music Player Daemon" desktop notification on track
//! change via libnotify. The Settings panel toggles that popup; the value
//! is persisted in `state.ron` (a `UiSettings` field) and synced here to a
//! small state file that the shim consults on every notification, so the
//! toggle applies live — no mpDris2 service restart needed.
//!
//! The file lives beside the other mpDris2 bridge files in the cache dir
//! (default `~/.cache/s2udio/mpdris2-notify.json`, `S2U_CACHE_DIR` aware
//! on the shim side). Absent/unparsable means *enabled*: that matches
//! mpDris2's own default, so a shim that never sees the file (or an
//! unpatched mpDris2) behaves exactly like upstream.

use std::path::Path;

/// File name of the notification-toggle state (read by the shim).
pub const NOTIFY_STATE_FILE: &str = "mpdris2-notify.json";

/// The mpDris2 notification-toggle state path: `<cache_dir>/` when the
/// config sets one, else `~/.cache/s2udio/` (round 19 layout; no legacy
/// rmpc fallback — this file is new, absent = enabled either way).
pub fn notify_state_path(cache_dir: Option<&Path>) -> std::path::PathBuf {
    if let Some(dir) = cache_dir {
        return dir.join(NOTIFY_STATE_FILE);
    }
    crate::shared::paths::s2udio_cache_dir()
        .unwrap_or_else(|| {
            crate::config::utils::tilde_expand("~/.cache/s2udio").into_owned().into()
        })
        .join(NOTIFY_STATE_FILE)
}

/// Sync the mpDris2 notification toggle to the bridge state file
/// (`{"notify": <enabled>}`). Called on Settings Save and at startup so a
/// restarted s2udio re-applies the persisted preference. Failure is only
/// logged — the toggle in the UI/persisted state is authoritative and the
/// shim falls back to its enabled default when the file is missing.
pub fn write_notify_state(cache_dir: Option<&Path>, enabled: bool) {
    let path = notify_state_path(cache_dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // A tiny literal cannot fail to serialize; format! keeps the write
    // infallible too (no unwrap in a non-test path).
    let bytes = format!(r#"{{"notify":{enabled}}}"#).into_bytes();
    if let Err(err) = std::fs::write(&path, bytes) {
        log::error!(error:? = err; "Failed to write the mpDris2 notification state");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_file_round_trips_the_toggle() {
        let dir = std::env::temp_dir().join(format!("s2u-mpdris2-notify-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        write_notify_state(Some(&dir), false);
        let path = notify_state_path(Some(&dir));
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["notify"], serde_json::Value::Bool(false));

        write_notify_state(Some(&dir), true);
        let content = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["notify"], serde_json::Value::Bool(true));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn configured_cache_dir_wins_over_default() {
        let dir = std::env::temp_dir().join(format!("s2u-mpdris2-notify-path-{}", std::process::id()));
        let path = notify_state_path(Some(&dir));
        assert_eq!(path, dir.join(NOTIFY_STATE_FILE));
    }
}
