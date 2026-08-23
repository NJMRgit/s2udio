//! Integration with the user's blur theme scheduler: watches
//! `~/.blur-schedule` (a shell snippet setting `_MODE=<mode>`) and applies
//! the active mode's accent color from `~/.local/bin/blsw` to the s2u theme.
//! The blsw script is only *read* — never executed — so s2u never triggers
//! the KWin side effects blsw applies.
use std::path::PathBuf;
use anyhow::Result;
use ratatui::style::Color;
use crate::{config::Config, shared::env::ENV};
/// `~` of the current user.
fn home() -> Option<PathBuf> {
    ENV.var_os("HOME").map(PathBuf::from)
}
fn schedule_path() -> Option<PathBuf> {
    home().map(|h| h.join(".blur-schedule"))
}
fn blsw_path() -> Option<PathBuf> {
    home().map(|h| h.join(".local/bin/blsw"))
}
/// The mode name currently in `~/.blur-schedule` (`_MODE=<mode>`).
pub fn read_schedule_mode() -> Option<String> {
    let content = std::fs::read_to_string(schedule_path()?).ok()?;
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
/// A greyish version of `color`: blended ~40% toward grey so the hue shows
/// through muted, then brightened enough to stay readable on a dark
/// terminal (dark theme colors like monster's deep red get lifted).
fn greyish(color: Color) -> Color {
    let Color::Rgb(r, g, b) = color else { return color };
    let (r, g, b) = (f64::from(r), f64::from(g), f64::from(b));
    let lum = 0.299 * r + 0.587 * g + 0.114 * b;
    let s = 0.4;
    let mut r = r * (1.0 - s) + lum * s;
    let mut g = g * (1.0 - s) + lum * s;
    let mut b = b * (1.0 - s) + lum * s;
    let max_ch = r.max(g).max(b);
    if max_ch > 0.0 && max_ch < 160.0 {
        let k = 160.0 / max_ch;
        r *= k;
        g *= k;
        b *= k;
    }
    Color::Rgb(r as u8, g as u8, b as u8)
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
/// actual theme hue), falling back to the logo color. The result is returned
/// greyish for s2udio.
pub fn read_mode_color(mode: &str) -> Option<Color> {
    let content = std::fs::read_to_string(blsw_path()?).ok()?;
    let upper = mode.to_uppercase();
    let dominant = find_var(&content, &format!("{upper}_OUTLINE_COLOR_ACTIVE"))
        .and_then(parse_rgb)
        .or_else(|| find_var(&content, &format!("{upper}_TINT")).and_then(parse_tint))
        .or_else(|| {
            find_var(&content, &format!("{upper}_FF_LOGO_COLOR")).and_then(parse_hex)
        })?;
    Some(greyish(dominant))
}
/// Apply the blur mode's accent as the theme text color (and re-derive the
/// border / focus / selection accents from it). Returns whether anything was
/// applied.
pub fn apply_mode_color(config: &mut Config, mode: &str) -> Result<bool> {
    let Some(color) = read_mode_color(mode) else {
        return Ok(false);
    };
    config.theme.text_color = Some(color);
    crate::config::derive_theme_accents(&mut config.theme);
    Ok(true)
}
