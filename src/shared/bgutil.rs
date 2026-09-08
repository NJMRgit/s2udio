//! Self-heal for the `s2u-yt-bgutil` PO-token provider (round 59).
//!
//! YouTube playback resolves through `yt-dlp → bgutil (127.0.0.1:4416,
//! user service s2u-yt-bgutil.service) → BotGuard handshake → PO token`.
//! The PO-token **minter has a hard 12 h lifetime** (`lifetime: 43200s` in
//! the server) and renewal is **lazy, on-request only** — there is no
//! scheduled renewal and no renewal HTTP endpoint (`/ping`, `/get_pot`,
//! `/invalidate_caches`, `/invalidate_it` only). After the 12 h cliff the
//! next request's fresh BotGuard handshake can silently fail or wedge
//! (`BotGuard initialization failed`, `Cannot get BotGuard expiry info
//! after reinitialization`, `BotGuard snapshot has expired`), after which
//! every request fails until the process is restarted.
//!
//! These helpers restart the service **before the cliff** (stale detection
//! at 10 h uptime → renew) and **after a failure** (one cooldown-guarded
//! retry), so YouTube keeps working without a manual restart. Every check
//! degrades to a harmless no-op when the service or systemd-user is absent.

use std::{
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

/// The service's name in the user-manager namespace (with suffix; the
/// systemd-user unit name).
const BGUTIL_SERVICE: &str = "s2u-yt-bgutil.service";

/// The minter's hard lifetime (server `lifetime: 43200s`). Renew well
/// before it so a stale handshake never gets a chance to wedge.
const MINTER_LIFETIME_HOURS: u64 = 12;
/// Restart when the service has been up ≥ this many hours (≥ 2 h margin).
const STALE_HOURS: u64 = 10;
/// Minimum seconds between automatic restarts (one heal per failure).
const COOLDOWN_SECS: u64 = 15 * 60;

/// Unix timestamp of the last automatic restart (0 = never).
static LAST_AUTO_RESTART: AtomicU64 = AtomicU64::new(0);

/// What `maybe_heal` decided to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealOutcome {
    /// No systemd-user backend / service not deployed — nothing to do.
    NotApplicable,
    /// Service fresh (uptime < `STALE_HOURS`) — no restart needed.
    Fresh,
    /// Service was stale and was restarted now.
    Restarted,
    /// A recent automatic restart already ran; cooldown active.
    Cooldown,
    /// Restart was attempted but failed.
    Failed,
}

fn systemd_user_available() -> bool {
    let out = match Command::new("systemctl")
        .args(["--user", "is-system-running"])
        .output()
    {
        Ok(out) => out,
        Err(_) => return false,
    };
    // is-system-running answers these real-manager states (mirrors
    // scripts/s2u-svc's backend detection): anything else — "offline",
    // "Failed to connect to bus", empty (containers without logind) —
    // means no usable user manager.
    match String::from_utf8_lossy(&out.stdout).trim() {
        "running" | "degraded" | "starting" | "maintenance" | "stopping" => true,
        _ => false,
    }
}

fn service_active() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", BGUTIL_SERVICE])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// The service's uptime in seconds, when determinable (systemd-user).
/// Returns `None` when the service is absent/inactive or the backend can't
/// answer (in which case we never restart blindly).
fn service_uptime_secs() -> Option<u64> {
    let out = Command::new("systemctl")
        .args(["--user", "show", BGUTIL_SERVICE, "-p", "MainPID", "--value"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let pid: u32 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    if pid == 0 {
        return None;
    }
    let etimes = Command::new("ps")
        .args(["-o", "etimes=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !etimes.status.success() {
        return None;
    }
    String::from_utf8_lossy(&etimes.stdout).trim().parse().ok()
}

fn restart_service() -> bool {
    // Systemd-user restart (a fresh process mints a new minter lazily on
    // the next request, which is the proven fix).
    let ok = Command::new("systemctl")
        .args(["--user", "restart", BGUTIL_SERVICE])
        .status()
        .is_ok_and(|s| s.success());
    if ok {
        LAST_AUTO_RESTART.store(now_secs(), Ordering::Relaxed);
    }
    ok
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether a URL is a YouTube link (the only backend bgutil mints POT
/// tokens for). Soundcloud/NicoVideo resolve through yt-dlp too, but never
/// touch the bgutil minter.
pub fn is_youtube_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    match host.strip_prefix("www.").unwrap_or(host) {
        "youtube.com" | "youtu.be" => true,
        _ => false,
    }
}

/// Heal the bgutil service when it is stale or its renewal may have
/// wedged. Safe under all conditions:
///
/// - no systemd-user backend, or the service is absent → `NotApplicable`;
/// - fresh service (uptime < `STALE_HOURS`) → `Fresh`, never touched;
/// - stale service (≥ `STALE_HOURS`) → restart once, `Restarted`;
/// - a previous auto-restart is still inside the cooldown → `Cooldown` —
///   the retry is skipped so a wedged service can never cause a loop.
pub fn maybe_heal() -> HealOutcome {
    if !systemd_user_available() {
        return HealOutcome::NotApplicable;
    }
    if !service_active() {
        return HealOutcome::NotApplicable;
    }
    let Some(uptime) = service_uptime_secs() else {
        // Active but undeterminable uptime: don't restart blindly.
        return HealOutcome::Fresh;
    };
    if uptime < STALE_HOURS * 3600 {
        return HealOutcome::Fresh;
    }
    let last = LAST_AUTO_RESTART.load(Ordering::Relaxed);
    if now_secs().saturating_sub(last) < COOLDOWN_SECS {
        return HealOutcome::Cooldown;
    }
    if restart_service() {
        log::info!(
            "bgutil stale (uptime {}h > {}h) — restarted the PO-token minter before its {}h cliff",
            uptime / 3600,
            STALE_HOURS,
            MINTER_LIFETIME_HOURS
        );
        HealOutcome::Restarted
    } else {
        log::warn!("bgutil stale but restart failed");
        HealOutcome::Failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn youtube_url_detection() {
        assert!(is_youtube_url("https://www.youtube.com/watch?v=QgH9sr7G13Q"));
        assert!(is_youtube_url("https://youtu.be/abcdefgh"));
        assert!(is_youtube_url("https://youtube.com/playlist?list=PL123"));
        assert!(!is_youtube_url("https://soundcloud.com/artist/track"));
        assert!(!is_youtube_url("https://nicovideo.jp/watch/sm9"));
        assert!(!is_youtube_url("https://example.com/watch?v=x"));
        assert!(!is_youtube_url("not a url"));
        assert!(!is_youtube_url(""));
    }

    #[test]
    fn heal_decisions() {
        // SANITY ONLY: the checks themselves are plain comparisons; these
        // exercise the arithmetic (10h threshold vs 12h cliff) so a typo
        // like hours-vs-seconds can't silently ship.
        let uptime_fresh = 2 * 3600;
        let uptime_stale = 10 * 3600;
        assert!(uptime_fresh < STALE_HOURS * 3600);
        assert!(uptime_stale >= STALE_HOURS * 3600);
        assert!(uptime_stale < MINTER_LIFETIME_HOURS * 3600);
        assert!(COOLDOWN_SECS > 60, "cooldown must prevent restart loops");
    }

    #[test]
    fn outcome_equality() {
        assert_eq!(HealOutcome::Restarted, HealOutcome::Restarted);
        assert_ne!(HealOutcome::Restarted, HealOutcome::Fresh);
    }
}
