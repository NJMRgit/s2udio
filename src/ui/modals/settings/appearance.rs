use anyhow::Result;
use ratatui::style::Color;

use super::SettingsModal;
use crate::{
    config::{Config, scale_color, theme::UiConfig},
    ctx::Ctx,
    shared::macros::{status_info, status_warn},
};

/// A staged appearance change: not applied until the panel is closed with
/// Save. `Transparent` maps to `None` (no color / default background).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StagedColor {
    Unchanged,
    Transparent,
    Set(Color),
}

/// Which theme color an appearance row edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppearanceTarget {
    /// Content text color (the queue / tab-list text). Never touched by the
    /// blur watcher.
    Text,
    Borders,
    FocusBorder,
    Selection,
    Accent,
    Background,
    /// The UI chrome accent: `text_color`, which the accent derivation
    /// (borders, cava bars, selection, seekbar, controls) follows. The blur
    /// watcher owns it while a mode is active.
    Ui,
}

impl AppearanceTarget {
    pub(super) fn name(self) -> &'static str {
        match self {
            AppearanceTarget::Text => "text color",
            AppearanceTarget::Ui => "UI colors",
            AppearanceTarget::Borders => "border color",
            AppearanceTarget::FocusBorder => "focused border",
            AppearanceTarget::Selection => "selection highlight",
            AppearanceTarget::Accent => "highlighted item",
            AppearanceTarget::Background => "background color",
        }
    }

    /// Persistence order (matches the enum discriminant order, so
    /// `AppearanceTarget as usize` indices stay stable). `Ui` is appended at
    /// the end so legacy 6-value `state.ron` appearance arrays still align
    /// with the original targets.
    pub(crate) fn all() -> [AppearanceTarget; 7] {
        [
            AppearanceTarget::Text,
            AppearanceTarget::Borders,
            AppearanceTarget::FocusBorder,
            AppearanceTarget::Selection,
            AppearanceTarget::Accent,
            AppearanceTarget::Background,
            AppearanceTarget::Ui,
        ]
    }

    /// Display order in the settings panel (text color, UI colors, border
    /// color, …).
    pub(super) fn display_order() -> [AppearanceTarget; 7] {
        [
            AppearanceTarget::Text,
            AppearanceTarget::Ui,
            AppearanceTarget::Borders,
            AppearanceTarget::FocusBorder,
            AppearanceTarget::Selection,
            AppearanceTarget::Accent,
            AppearanceTarget::Background,
        ]
    }
}

/// Parse a 3- or 6-digit hex color (with or without '#').
pub(super) fn parse_hex_color(s: &str) -> Option<Color> {
    let s = s.trim().trim_start_matches('#');
    let expanded: String = match s.len() {
        3 => s.chars().flat_map(|c| std::iter::repeat(c).take(2)).collect(),
        6 => s.to_string(),
        _ => return None,
    };
    if !expanded.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(&expanded, 16).ok()?;
    Some(Color::Rgb(((v >> 16) & 0xff) as u8, ((v >> 8) & 0xff) as u8, (v & 0xff) as u8))
}

pub(super) fn color_hex(color: &Color) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        other => format!("{other:?}"),
    }
}

/// The current theme color of an appearance target. `blur_active` tells
/// whether the blur watcher currently owns `text_color`: while a mode is
/// active the persisted value must be the *configured* color, never the
/// transient mode accent (a save while a mode runs would otherwise freeze a
/// stale mode color into state.ron).
fn theme_color(theme: &UiConfig, target: AppearanceTarget, blur_active: bool) -> Option<Color> {
    match target {
        AppearanceTarget::Text => theme.list_text_color.or(theme.text_color),
        AppearanceTarget::Ui => {
            if blur_active {
                theme.list_text_color.or(theme.text_color)
            } else {
                theme.text_color
            }
        }
        AppearanceTarget::Borders => theme.borders_style.fg,
        AppearanceTarget::FocusBorder => theme.highlight_border_style.fg,
        AppearanceTarget::Selection => theme.current_item_style.bg,
        AppearanceTarget::Accent => theme.highlighted_item_style.fg,
        AppearanceTarget::Background => theme.background_color,
    }
}

/// The appearance values to persist to state.ron after a settings save:
/// each target's resolved color as a hex string, "" for transparent, and
/// `None` when the theme's value is not an RGB color (left to the theme on
/// restore). While a blur mode is scheduled the UI-accent target persists
/// the configured color (see `theme_color`).
pub(crate) fn persisted_appearance(config: &Config) -> Vec<Option<String>> {
    let blur_active = crate::core::blur::read_schedule_mode().is_some();
    persisted_appearance_with(config, blur_active)
}

/// The persisted values with an explicit blur-mode signal (deterministic in
/// tests).
pub(super) fn persisted_appearance_with(
    config: &Config,
    blur_active: bool,
) -> Vec<Option<String>> {
    AppearanceTarget::all()
        .iter()
        .map(|target| match theme_color(&config.theme, *target, blur_active) {
            Some(Color::Rgb(r, g, b)) => Some(format!("#{r:02x}{g:02x}{b:02x}")),
            Some(_) => None,
            None => Some(String::new()),
        })
        .collect()
}

/// Apply persisted appearance values to a config's theme: a hex string sets
/// the color, "" makes it transparent, `None` leaves the theme default.
///
/// When a blur mode is scheduled (`blur_mode_active`), the targets the blur
/// watcher derives from the mode's color (the UI accent / Borders /
/// FocusBorder / Selection) are skipped: the persisted values only reflect
/// the theme at the time of the last settings save and would otherwise
/// flash stale colors (e.g. a previous mode's accent) until the watcher
/// re-derives them on its first tick. The content text color (Text) and the
/// highlighted-item / background colors are not blur-managed and always
/// restore.
pub(crate) fn apply_persisted_appearance(
    config: &mut Config,
    values: &[Option<String>],
    blur_mode_active: bool,
) {
    for (target, value) in AppearanceTarget::all().iter().zip(values) {
        if blur_mode_active
            && matches!(
                target,
                AppearanceTarget::Ui
                    | AppearanceTarget::Borders
                    | AppearanceTarget::FocusBorder
                    | AppearanceTarget::Selection
            )
        {
            continue;
        }
        match value.as_deref() {
            Some(hex) if hex.is_empty() => {
                set_appearance_color(&mut config.theme, *target, None);
            }
            Some(hex) => {
                if let Some(color) = parse_hex_color(hex) {
                    set_appearance_color(&mut config.theme, *target, Some(color));
                }
            }
            None => {}
        }
    }
}

/// The current color of an appearance target in the theme (`None` =
/// transparent / unset, e.g. the background).
pub(super) fn appearance_color(theme: &UiConfig, target: AppearanceTarget) -> Option<Color> {
    match target {
        AppearanceTarget::Text => theme.list_text_color.or(theme.text_color),
        AppearanceTarget::Ui => theme.text_color,
        AppearanceTarget::Borders => theme.borders_style.fg,
        AppearanceTarget::FocusBorder => theme.highlight_border_style.fg,
        AppearanceTarget::Selection => theme.current_item_style.bg,
        AppearanceTarget::Accent => theme.highlighted_item_style.fg,
        AppearanceTarget::Background => theme.background_color,
    }
}

pub(crate) fn set_appearance_color(
    theme: &mut UiConfig,
    target: AppearanceTarget,
    color: Option<Color>,
) {
    match target {
        AppearanceTarget::Text => theme.list_text_color = color,
        AppearanceTarget::Ui => theme.text_color = color,
        AppearanceTarget::Borders => theme.borders_style.fg = color,
        AppearanceTarget::FocusBorder => theme.highlight_border_style.fg = color,
        AppearanceTarget::Selection => {
            theme.current_item_style.bg = color;
            theme.marked_item_style.bg = color.map(|c| scale_color(c, 1.3));
        }
        AppearanceTarget::Accent => theme.highlighted_item_style.fg = color,
        AppearanceTarget::Background => theme.background_color = color,
    }
}

impl SettingsModal {
    /// The color currently displayed for an appearance row: a staged value
    /// wins over the live theme value (`None` = transparent).
    pub(super) fn displayed_color(&self, ctx: &Ctx, target: AppearanceTarget) -> Option<Color> {
        match self.appearance_pending[target as usize] {
            StagedColor::Unchanged => appearance_color(&ctx.config.theme, target),
            StagedColor::Transparent => None,
            StagedColor::Set(color) => Some(color),
        }
    }

    pub(super) fn start_edit_color(&mut self, target: AppearanceTarget, ctx: &mut Ctx) {
        let _ = self.displayed_color(ctx, target);
        self.edit_buffer.clear();
        self.editing_color = Some(target);
        ctx.render().ok();
    }

    /// Commit the edit buffer: validate, stage the color and leave edit mode.
    /// `transparent` / `none` / `0` clears the color (e.g. a transparent
    /// background).
    pub(super) fn commit_edit_color(&mut self, ctx: &mut Ctx) -> Result<()> {
        let Some(target) = self.editing_color else { return Ok(()) };
        let value = self.edit_buffer.trim().to_ascii_lowercase();
        let staged = if matches!(value.as_str(), "transparent" | "none" | "0" | "") {
            Some(StagedColor::Transparent)
        } else {
            parse_hex_color(&value).map(StagedColor::Set)
        };
        match staged {
            Some(staged) => {
                self.appearance_pending[target as usize] = staged;
                self.editing_color = None;
                status_info!("{} staged — save when leaving settings", target.name());
                ctx.render()?;
            }
            None => {
                status_warn!(
                    "Invalid color '{}' (hex like 8f8f8f, or transparent)", self
                    .edit_buffer
                );
                ctx.render()?;
            }
        }
        Ok(())
    }
}
