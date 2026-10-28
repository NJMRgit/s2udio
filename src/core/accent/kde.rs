//! The KDE accent source: the Plasma accent color.
//!
//! Primary route is the **XDG desktop portal**: `org.freedesktop.appearance`
//! / `accent-color` on `org.freedesktop.portal.Settings.Read`. That is the
//! desktop-neutral setting KDE (and GNOME) publish, so it also works on a
//! non-KDE session whose portal exposes an accent.
//!
//! Fallback route is Plasma's `kdeglobals` INI: `[General] AccentColor`
//! (decimal `r,g,b`), then the active scheme's
//! `[Colors:Selection] BackgroundNormal`, then
//! `[Colors:Button] DecorationFocus`. Plasma writes that file whenever the
//! scheme or accent changes, so the fallback still follows the desktop when
//! the portal is missing, wedged, or not running.
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use ratatui::style::Color;

use crate::{config::auto_theme::AutoThemeConfig, config::utils::tilde_expand_path};

use super::PORTAL_TIMEOUT;

/// The portal namespace and key that carry the accent.
const APPEARANCE_NAMESPACE: &str = "org.freedesktop.appearance";
const ACCENT_COLOR_KEY: &str = "accent-color";

const PORTAL_BUS_NAME: &str = "org.freedesktop.portal.Desktop";
const PORTAL_OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";
const PORTAL_SETTINGS_INTERFACE: &str = "org.freedesktop.portal.Settings";

/// The Plasma accent: the portal first, then `kdeglobals`.
pub(crate) fn read_accent(cfg: &AutoThemeConfig) -> Option<Color> {
    read_portal_accent(PORTAL_TIMEOUT).or_else(|| read_kdeglobals(cfg))
}

/// The session-bus connection, kept for the life of the process: the watcher
/// resolves every second, and opening a fresh bus connection each time would
/// churn the portal for no reason. It is dropped again when a call fails at
/// the transport level (bus restart, portal gone), so the next tick
/// reconnects.
static PORTAL_CONNECTION: Mutex<Option<zbus::blocking::Connection>> = Mutex::new(None);

/// Read the accent from the desktop's settings portal. `None` when there is
/// no session bus, no portal, or the portal does not publish an accent.
fn read_portal_accent(timeout: Duration) -> Option<Color> {
    let mut connection = PORTAL_CONNECTION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if connection.is_none() {
        let built = zbus::blocking::connection::Builder::session()
            .ok()?
            .method_timeout(timeout)
            .build()
            .ok()?;
        *connection = Some(built);
    }
    let conn = connection.as_ref()?;
    match read_portal_accent_over(conn) {
        PortalReply::Accent(color) => Some(color),
        // The portal answered but has no accent: keep the connection.
        PortalReply::NoAccent => None,
        // Transport trouble: reconnect on the next resolve.
        PortalReply::Disconnected => {
            *connection = None;
            None
        }
    }
}

/// What one portal read produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortalReply {
    Accent(Color),
    /// The portal is there but has no accent for us (e.g. no such key).
    NoAccent,
    /// The connection/proxy failed: the cached connection is stale.
    Disconnected,
}

/// The portal read against an existing connection (split out so the reply
/// handling is testable and the connection lifetime is explicit).
fn read_portal_accent_over(connection: &zbus::blocking::Connection) -> PortalReply {
    let Ok(proxy) = zbus::blocking::Proxy::new(
        connection,
        PORTAL_BUS_NAME,
        PORTAL_OBJECT_PATH,
        PORTAL_SETTINGS_INTERFACE,
    ) else {
        return PortalReply::Disconnected;
    };
    let reply = match proxy.call_method("Read", &(APPEARANCE_NAMESPACE, ACCENT_COLOR_KEY)) {
        Ok(reply) => reply,
        // A method error is still an answer: this portal does not serve the
        // key. Any other error means the connection itself is unusable.
        Err(zbus::Error::MethodError(..)) => return PortalReply::NoAccent,
        Err(_) => return PortalReply::Disconnected,
    };
    let body = reply.body();
    let Ok(value): Result<zbus::zvariant::Value<'_>, _> = body.deserialize() else {
        return PortalReply::NoAccent;
    };
    parse_portal_accent(&value).map_or(PortalReply::NoAccent, PortalReply::Accent)
}

/// The portal's reply: a variant holding a `(ddd)` triplet of 0..=1 doubles.
fn parse_portal_accent(value: &zbus::zvariant::Value<'_>) -> Option<Color> {
    let (r, g, b): (f64, f64, f64) = value.downcast_ref().ok()?;
    Some(Color::Rgb(unit_to_u8(r), unit_to_u8(g), unit_to_u8(b)))
}

/// A 0..=1 channel as a byte.
fn unit_to_u8(value: f64) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Read the accent out of Plasma's `kdeglobals`.
fn read_kdeglobals(cfg: &AutoThemeConfig) -> Option<Color> {
    let path = tilde_expand_path(Path::new(&cfg.kdeglobals));
    parse_kdeglobals(&std::fs::read_to_string(path).ok()?)
}

/// Parse the accent out of a `kdeglobals` document.
///
/// `[General] AccentColor` is the user's explicit accent; when it is absent
/// the scheme's own colors stand in (`[Colors:Selection] BackgroundNormal`,
/// then `[Colors:Button] DecorationFocus`). Values are decimal `r,g,b`
/// (occasionally with a fourth alpha component).
pub(crate) fn parse_kdeglobals(content: &str) -> Option<Color> {
    let mut section = String::new();
    let mut accent = None;
    let mut selection = None;
    let mut focus = None;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            // `[Colors:Header][Inactive]` is two groups on one line; the
            // first is the set that carries the colors.
            section = rest.split(']').next().unwrap_or_default().trim().to_owned();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        match (section.as_str(), key.trim()) {
            ("General", "AccentColor") => accent = parse_decimal_rgb(value),
            ("Colors:Selection", "BackgroundNormal") => selection = parse_decimal_rgb(value),
            ("Colors:Button", "DecorationFocus") => focus = parse_decimal_rgb(value),
            _ => {}
        }
    }
    accent.or(selection).or(focus)
}

/// Parse KDE's decimal `r,g,b` (or `r,g,b,a`) color notation.
fn parse_decimal_rgb(value: &str) -> Option<Color> {
    let value = value.trim().trim_matches('"');
    let mut parts = value.split(',').map(str::trim);
    let r: u8 = parts.next()?.parse().ok()?;
    let g: u8 = parts.next()?.parse().ok()?;
    let b: u8 = parts.next()?.parse().ok()?;
    // A fourth component is the alpha channel and carries no RGB meaning here.
    if let Some(alpha) = parts.next()
        && alpha.parse::<u8>().is_err()
    {
        return None;
    }
    Some(Color::Rgb(r, g, b))
}

#[cfg(test)]
mod tests {
    use zbus::zvariant::{Structure, Value};

    use super::*;

    const KDEGLOBALS: &str = r#"[General]
ColorScheme=BreezeDark
AccentColor=61,174,233

[Colors:View]
BackgroundNormal=20,22,24
ForegroundNormal=252,252,252

[Colors:Selection]
BackgroundNormal=61,174,233

[Colors:Button]
DecorationFocus=61,174,233
"#;

    #[test]
    fn kdeglobals_accent_color_wins() {
        assert_eq!(parse_kdeglobals(KDEGLOBALS), Some(Color::Rgb(61, 174, 233)));
    }

    #[test]
    fn a_missing_accent_falls_back_to_the_scheme() {
        let no_accent = KDEGLOBALS.replace("AccentColor=61,174,233\n", "");
        assert_eq!(
            parse_kdeglobals(&no_accent),
            Some(Color::Rgb(61, 174, 233)),
            "the selection background stands in for the accent"
        );
        let selection_only = "[Colors:Selection]\nBackgroundNormal=1,2,3\n";
        assert_eq!(parse_kdeglobals(selection_only), Some(Color::Rgb(1, 2, 3)));
        let focus_only = "[Colors:Button]\nDecorationFocus=4,5,6\n";
        assert_eq!(parse_kdeglobals(focus_only), Some(Color::Rgb(4, 5, 6)));
    }

    #[test]
    fn an_explicit_accent_beats_the_scheme_colors() {
        let doc = "[General]\nAccentColor=9,8,7\n\n[Colors:Selection]\nBackgroundNormal=61,174,233\n";
        assert_eq!(parse_kdeglobals(doc), Some(Color::Rgb(9, 8, 7)));
    }

    #[test]
    fn alpha_components_are_ignored() {
        let doc = "[General]\nAccentColor=61,174,233,255\n";
        assert_eq!(parse_kdeglobals(doc), Some(Color::Rgb(61, 174, 233)));
        let broken_alpha = "[General]\nAccentColor=61,174,233,oops\n";
        assert_eq!(parse_kdeglobals(broken_alpha), None);
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let doc = "# a comment\n; another\n\n[General]\n; AccentColor=0,0,0\nAccentColor=1,2,3\n";
        assert_eq!(parse_kdeglobals(doc), Some(Color::Rgb(1, 2, 3)));
    }

    #[test]
    fn two_group_headers_are_handled() {
        // `[Colors:Header][Inactive]` must not be read as one long section
        // name, nor merge into the section before it.
        let doc = "[Colors:Header][Inactive]\nBackgroundNormal=9,9,9\n\n[General]\nAccentColor=1,2,3\n";
        assert_eq!(parse_kdeglobals(doc), Some(Color::Rgb(1, 2, 3)));
    }

    #[test]
    fn malformed_documents_yield_nothing() {
        assert_eq!(parse_kdeglobals(""), None);
        assert_eq!(parse_kdeglobals("AccentColor=1,2,3\n"), None, "outside [General]");
        assert_eq!(parse_kdeglobals("[General]\nAccentColor=1,2\n"), None);
        assert_eq!(parse_kdeglobals("[General]\nAccentColor=300,0,0\n"), None);
        assert_eq!(parse_kdeglobals("[General]\nAccentColor=\n"), None);
    }

    fn portal_value(r: f64, g: f64, b: f64) -> Value<'static> {
        Value::Structure(Structure::from((r, g, b)))
    }

    #[test]
    fn portal_triplets_map_to_rgb() {
        // 61,174,233 is Plasma's Breeze Blue.
        assert_eq!(
            parse_portal_accent(&portal_value(0.2392, 0.6824, 0.9137)),
            Some(Color::Rgb(61, 174, 233))
        );
        // The wire form of a `v` reply nests the value one level deeper.
        let nested = Value::Value(Box::new(portal_value(1.0, 0.0, 0.5)));
        assert_eq!(parse_portal_accent(&nested), Some(Color::Rgb(255, 0, 128)));
    }

    #[test]
    fn portal_channels_are_clamped_and_rounded() {
        assert_eq!(unit_to_u8(0.0), 0);
        assert_eq!(unit_to_u8(1.0), 255);
        assert_eq!(unit_to_u8(0.5), 128);
        assert_eq!(unit_to_u8(-1.0), 0);
        assert_eq!(unit_to_u8(2.0), 255);
    }

    #[test]
    fn a_portal_reply_of_the_wrong_type_is_ignored() {
        assert_eq!(parse_portal_accent(&Value::from(1u32)), None);
        assert_eq!(parse_portal_accent(&Value::from("accent")), None);
    }

    #[test]
    fn unknown_paths_yield_no_accent_when_no_portal_answers() {
        // The test environment has no session bus / portal, so the portal
        // yields nothing and the (missing) file ends the chain.
        let cfg = AutoThemeConfig {
            kdeglobals: "/nonexistent/s2udio-kdeglobals".to_owned(),
            ..AutoThemeConfig::default()
        };
        assert_eq!(read_accent(&cfg), None);
    }
}
