//! The blur-mode (STTM) accent source: watches `~/.blur-schedule` (a shell
//! snippet setting `_MODE=<mode>`) and reads the active mode's accent color
//! from `~/.local/bin/blsw`.
//!
//! The blsw script is only *read* — never executed — so s2udio never triggers
//! the KWin side effects blsw applies. Paths come from the `auto_theme` config
//! section, so a non-default install works without a rebuild.
use std::path::Path;

use ratatui::style::Color;

use crate::{config::auto_theme::AutoThemeConfig, config::utils::tilde_expand_path};

/// The mode name currently in the schedule file (`_MODE=<mode>`).
pub(crate) fn read_schedule_mode(cfg: &AutoThemeConfig) -> Option<String> {
    let path = tilde_expand_path(Path::new(&cfg.sttm_schedule));
    let content = std::fs::read_to_string(path).ok()?;
    parse_schedule_mode(&content)
}

/// Parse `_MODE=<mode>` out of the schedule snippet.
pub(crate) fn parse_schedule_mode(content: &str) -> Option<String> {
    let line = content.lines().find(|l| l.trim_start().starts_with("_MODE="))?;
    let mode = line.split_once('=')?.1.trim().trim_matches('"').to_lowercase();
    if mode.is_empty() { None } else { Some(mode) }
}

/// Extract `NAME=value` (quotes trimmed) from the blsw script.
fn find_var<'a>(content: &'a str, name: &str) -> Option<&'a str> {
    content
        .lines()
        .find(|l| l.trim_start().starts_with(&format!("{name}=")))
        .and_then(|l| l.split_once('=').map(|(_, v)| v.trim().trim_matches('"')))
        .filter(|v| !v.is_empty())
}

fn parse_hex(value: &str) -> Option<Color> {
    let hex = value.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(hex, 16).ok()?;
    Some(Color::Rgb(((v >> 16) & 0xff) as u8, ((v >> 8) & 0xff) as u8, (v & 0xff) as u8))
}

fn parse_rgb(value: &str) -> Option<Color> {
    let parts: Vec<&str> = value.split(',').map(str::trim).collect();
    if parts.len() != 3 {
        return None;
    }
    Some(
        Color::Rgb(
            parts[0].parse().ok()?,
            parts[1].parse().ok()?,
            parts[2].parse().ok()?,
        ),
    )
}

/// Parse a `#AARRGGBB` tint into its RGB color (the alpha prefix is the
/// effect opacity, not part of the color).
fn parse_tint(value: &str) -> Option<Color> {
    let hex = value.trim().trim_start_matches('#');
    if hex.len() != 8 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(&hex[2..], 16).ok()?;
    Some(Color::Rgb(((v >> 16) & 0xff) as u8, ((v >> 8) & 0xff) as u8, (v & 0xff) as u8))
}

/// The mode's dominant color from blsw: the KWin `TINT` / outline color (the
/// actual theme hue), falling back to the logo color.
pub(crate) fn read_mode_color(cfg: &AutoThemeConfig, mode: &str) -> Option<Color> {
    let path = tilde_expand_path(Path::new(&cfg.sttm_script));
    let content = std::fs::read_to_string(path).ok()?;
    mode_color(&content, mode)
}

/// The dominant color of `mode` in the blsw definitions.
fn mode_color(content: &str, mode: &str) -> Option<Color> {
    let upper = mode.to_uppercase();
    find_var(content, &format!("{upper}_OUTLINE_COLOR_ACTIVE"))
        .and_then(parse_rgb)
        .or_else(|| find_var(content, &format!("{upper}_TINT")).and_then(parse_tint))
        .or_else(|| find_var(content, &format!("{upper}_FF_LOGO_COLOR")).and_then(parse_hex))
}

/// The STTM accent: the scheduled mode's dominant color.
pub(crate) fn read_accent(cfg: &AutoThemeConfig) -> Option<Color> {
    let mode = read_schedule_mode(cfg)?;
    read_mode_color(cfg, &mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLSW_SAMPLE: &str = r##"FLWR_TINT="#50ff7f99"
FLWR_OUTLINE_COLOR_ACTIVE="255,127,153"
FLWR_FF_LOGO_COLOR="#FF6673"
NIGHT_TINT="#50005e8a"
NIGHT_OUTLINE_COLOR_ACTIVE="0,94,138"
NIGHT_FF_LOGO_COLOR="#07A761"
"##;

    #[test]
    fn schedule_mode_is_parsed() {
        assert_eq!(parse_schedule_mode("_MODE=monster\n"), Some("monster".to_owned()));
        assert_eq!(parse_schedule_mode("_MODE=\"Night\"\n"), Some("night".to_owned()));
        assert_eq!(parse_schedule_mode("_MODE=\n"), None);
        assert_eq!(parse_schedule_mode("nothing here"), None);
    }

    #[test]
    fn parse_hex_colors() {
        assert_eq!(parse_hex("#FF6673"), Some(Color::Rgb(0xff, 0x66, 0x73)));
        assert_eq!(parse_hex("ff6673"), Some(Color::Rgb(0xff, 0x66, 0x73)));
        assert_eq!(parse_hex("#ff66"), None);
    }

    #[test]
    fn parse_rgb_colors() {
        assert_eq!(parse_rgb("255,127,153"), Some(Color::Rgb(255, 127, 153)));
        assert_eq!(parse_rgb("1,2"), None);
    }

    #[test]
    fn finds_mode_color_in_blsw() {
        assert_eq!(find_var(BLSW_SAMPLE, "FLWR_FF_LOGO_COLOR"), Some("#FF6673"));
        assert_eq!(find_var(BLSW_SAMPLE, "FLWR_OUTLINE_COLOR_ACTIVE"), Some("255,127,153"));
        assert_eq!(find_var(BLSW_SAMPLE, "MISSING_X"), None);
    }

    #[test]
    fn parse_hex_from_blsw() {
        assert_eq!(
            parse_hex(find_var(BLSW_SAMPLE, "FLWR_FF_LOGO_COLOR").unwrap()),
            Some(Color::Rgb(0xff, 0x66, 0x73))
        );
    }

    #[test]
    fn parses_tint_rgba() {
        assert_eq!(parse_tint("#50ff7f99"), Some(Color::Rgb(0xff, 0x7f, 0x99)));
        assert_eq!(parse_tint("#50005e8a"), Some(Color::Rgb(0x00, 0x5e, 0x8a)));
    }

    #[test]
    fn outline_color_wins_over_tint_then_logo() {
        assert_eq!(
            mode_color(BLSW_SAMPLE, "flwr"),
            Some(Color::Rgb(255, 127, 153)),
            "the outline color is the mode's theme hue"
        );
        // A mode with only a tint falls back to it.
        let tint_only = "ONLY_TINT=\"#50640029\"\n";
        assert_eq!(mode_color(tint_only, "only"), Some(Color::Rgb(0x64, 0x00, 0x29)));
        // ...and one with only a logo color to that.
        let logo_only = "SOLO_FF_LOGO_COLOR=\"#07A761\"\n";
        assert_eq!(mode_color(logo_only, "solo"), Some(Color::Rgb(0x07, 0xa7, 0x61)));
        assert_eq!(mode_color(BLSW_SAMPLE, "absent"), None);
    }

    #[test]
    fn unknown_paths_yield_no_accent() {
        let cfg = AutoThemeConfig {
            sttm_schedule: "/nonexistent/s2udio-schedule".to_owned(),
            sttm_script: "/nonexistent/s2udio-blsw".to_owned(),
            ..AutoThemeConfig::default()
        };
        assert_eq!(read_accent(&cfg), None);
    }
}
