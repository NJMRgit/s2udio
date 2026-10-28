//! The matugen accent source: read the accent out of a matugen palette.
//!
//! matugen is a template engine — it does not keep "the current palette" in a
//! well-known file of its own, so this source discovers the palette from what
//! a matugen setup already leaves on disk, in this order:
//!
//! 1. `auto_theme.matugen_json`, when the user points at a file explicitly.
//! 2. The conventional JSON-template outputs a palette is often exported to:
//!    `~/.cache/matugen/colors.json` and `~/.config/matugen/colors.json`
//!    (`matugen image "$wall" --json hex > ~/.cache/matugen/colors.json`).
//! 3. matugen's own cache: with `caching = true` in matugen's `config.toml`
//!    it writes `~/.cache/matugen/images/<image>.<ext>.<sha256>.json`
//!    (rewritten whenever that image is rendered again), so the newest one
//!    is the palette the rest of the desktop was just themed with.
//! 4. The palette files the shells that drive matugen themselves keep:
//!    `~/.cache/dms/dms-colors.json` (DankMaterialShell / Dux shell) and
//!    `~/.local/state/caelestia/scheme.json` (Caelestia shell). Both are
//!    palette JSON written whenever their shell re-runs matugen, so their
//!    users need no s2udio-side setup either.
//! 5. Any `output_path` from the user's own `~/.config/matugen/config.toml`
//!    that ends in `.json` (a template that dumps the whole palette).
//!
//! Three document shapes are understood:
//!
//! ```json
//! // matugen 4.x `--json` dump (mode-nested)
//! { "colors": { "primary": { "default": { "color": "#c8bfff" },
//!                            "dark":   { "color": "#c8bfff" } } } }
//! // pre-4.0 flat dump (and single-colour template output)
//! { "colors": { "primary": "#c8bfff" } }
//! // matugen's cache file and DMS/Dux's `dms-colors.json` (mode first)
//! { "colors": { "dark": { "primary": "#c8bfffcc" }, "light": { … } } }
//! // Caelestia's `scheme.json` (British spelling, bare hex, no '#')
//! { "mode": "dark", "colours": { "primary": "c8bfff" } }
//! ```
//!
//! Colors may carry an alpha suffix (`#RRGGBBAA`, alpha last — matugen's
//! convention); it is ignored, the accent is the RGB part.
use std::path::{Path, PathBuf};

use ratatui::style::Color;
use serde_json::Value;

use crate::{
    config::{auto_theme::{AutoThemeConfig, MatugenMode}, utils::tilde_expand_path},
    shared::env::ENV,
    shared::paths::home_dir,
};

/// Read the accent from the first matugen palette that resolves.
pub(crate) fn read_accent(cfg: &AutoThemeConfig) -> Option<Color> {
    let (cache_root, config_root, state_root) = matugen_dirs();
    read_accent_in(
        cfg,
        cache_root.as_deref(),
        config_root.as_deref(),
        state_root.as_deref(),
    )
}

/// The XDG roots the discovery looks in (`$XDG_CACHE_HOME` and
/// `$XDG_CONFIG_HOME`, `$XDG_STATE_HOME`, with the usual `~/.cache` /
/// `~/.config` / `~/.local/state` fallbacks), without the app suffix.
fn matugen_dirs() -> (Option<PathBuf>, Option<PathBuf>, Option<PathBuf>) {
    let xdg = |var: &str, fallback: &str| {
        ENV.var_os(var)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| home_dir().map(|home| home.join(fallback)))
    };
    (
        xdg("XDG_CACHE_HOME", ".cache"),
        xdg("XDG_CONFIG_HOME", ".config"),
        xdg("XDG_STATE_HOME", ".local/state"),
    )
}

/// The discovery itself, with the XDG roots injected (deterministic in
/// tests).
fn read_accent_in(
    cfg: &AutoThemeConfig,
    cache_root: Option<&Path>,
    config_root: Option<&Path>,
    state_root: Option<&Path>,
) -> Option<Color> {
    for path in candidate_paths(cfg.matugen_json.as_deref(), cache_root, config_root, state_root)
    {
        let Ok(content) = std::fs::read_to_string(&path) else { continue };
        if let Some(color) = parse_palette(&content, &cfg.matugen_color, cfg.matugen_mode) {
            return Some(color);
        }
    }
    None
}

/// The palette files to try, in discovery order.
fn candidate_paths(
    explicit: Option<&str>,
    cache_root: Option<&Path>,
    config_root: Option<&Path>,
    state_root: Option<&Path>,
) -> Vec<PathBuf> {
    // An explicit path is the whole answer: the user said where the palette
    // is, so nothing else is guessed.
    if let Some(explicit) = explicit.map(str::trim).filter(|path| !path.is_empty()) {
        return vec![tilde_expand_path(Path::new(explicit))];
    }
    let matugen_cache = cache_root.map(|root| root.join("matugen"));
    let matugen_config = config_root.map(|root| root.join("matugen"));
    let mut paths = Vec::new();
    // A palette exported to the conventional JSON output
    // (`matugen image … --json hex > colors.json`).
    for root in [&matugen_cache, &matugen_config].into_iter().flatten() {
        paths.push(root.join("colors.json"));
    }
    // matugen's own cache (`caching = true`): the image rendered last.
    if let Some(root) = &matugen_cache
        && let Some(newest) = newest_cache_file(&root.join("images"))
    {
        paths.push(newest);
    }
    // The palette files the shells that drive matugen keep.
    if let Some(root) = cache_root {
        paths.push(root.join("dms").join("dms-colors.json")); // DankMaterialShell / Dux
    }
    if let Some(root) = state_root {
        paths.push(root.join("caelestia").join("scheme.json")); // Caelestia
    }
    // The user's own JSON palette templates.
    if let Some(root) = &matugen_config
        && let Ok(config) = std::fs::read_to_string(root.join("config.toml"))
    {
        paths.extend(template_json_outputs(&config, root));
    }
    paths
}

/// `output_path` / `output_paths` entries of a matugen `config.toml` that
/// point at JSON files (a template that dumps the whole palette). Relative
/// paths resolve against the config file's directory, as matugen does.
fn template_json_outputs(config: &str, config_dir: &Path) -> Vec<PathBuf> {
    let mut outputs = Vec::new();
    for line in config.lines() {
        let line = line.trim();
        if !(line.starts_with("output_path ") || line.starts_with("output_paths ")) {
            continue;
        }
        // Every quoted string on the line, comment part included is harmless:
        // a commented-out path simply does not exist and is skipped later.
        let mut rest = line;
        while let Some(quote_at) = rest.find(['\'', '"']) {
            let quote = rest.as_bytes()[quote_at] as char;
            let after = &rest[quote_at + 1..];
            let Some(end) = after.find(quote) else { break };
            let value = &after[..end];
            if value.ends_with(".json") && !value.is_empty() {
                let path = tilde_expand_path(Path::new(value));
                outputs.push(if path.is_absolute() { path } else { config_dir.join(path) });
            }
            rest = &after[end + 1..];
        }
    }
    outputs
}

/// The most recently written `*.json` in matugen's image cache.
fn newest_cache_file(dir: &Path) -> Option<PathBuf> {
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(std::ffi::OsStr::to_str) != Some("json") {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) else { continue };
        if newest.as_ref().is_none_or(|(best, _)| modified > *best) {
            newest = Some((modified, path));
        }
    }
    newest.map(|(_, path)| path)
}

/// Parse a matugen palette document and return the accent color.
pub(crate) fn parse_palette(content: &str, color_key: &str, mode: MatugenMode) -> Option<Color> {
    let json: Value = serde_json::from_str(content).ok()?;
    // `colors` is matugen's / DMS's spelling, `colours` Caelestia's.
    let colors = json.get("colors").or_else(|| json.get("colours"))?;
    // 4.x / pre-4.0 / single-colour shapes: colors.<key>.
    if let Some(entry) = colors.get(color_key)
        && let Some(hex) = entry_hex(entry, mode)
    {
        return parse_hex(&hex);
    }
    // matugen's cache file: colors.<variant>.<key>. The cache stores both
    // variants and no `default`, so `Default` picks the dark one — matugen's
    // own default mode.
    let variant = match mode {
        MatugenMode::Default => "dark",
        other => other.palette_key(),
    };
    let hex = colors.get(variant)?.get(color_key)?.as_str()?;
    parse_hex(hex)
}

/// The hex string inside one `colors.<key>` entry, in any supported shape.
fn entry_hex(entry: &Value, mode: MatugenMode) -> Option<String> {
    // Pre-4.0 flat dump: "primary": "#RRGGBB".
    if let Some(hex) = entry.as_str() {
        return Some(hex.to_owned());
    }
    // A single rendered color: "primary": { "color": "#RRGGBB" }.
    if let Some(hex) = entry.get("color").and_then(Value::as_str) {
        return Some(hex.to_owned());
    }
    // 4.0+: "primary": { "default" | "dark" | "light": { "color": "#RRGGBB" } }.
    // The requested variant wins; a palette without it falls back to
    // `default` (the variant matugen itself rendered).
    let variant = entry
        .get(mode.palette_key())
        .or_else(|| entry.get(MatugenMode::Default.palette_key()))?;
    if let Some(hex) = variant.as_str() {
        return Some(hex.to_owned());
    }
    variant.get("color").and_then(Value::as_str).map(str::to_owned)
}

/// Parse `#RRGGBB` or `#RRGGBBAA` (alpha last) into an RGB color.
pub(crate) fn parse_hex(value: &str) -> Option<Color> {
    let hex = value.trim().trim_start_matches('#');
    if hex.len() != 6 && hex.len() != 8 {
        return None;
    }
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(hex.get(..6)?, 16).ok()?;
    Some(Color::Rgb(((v >> 16) & 0xff) as u8, ((v >> 8) & 0xff) as u8, (v & 0xff) as u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A matugen 4.x palette dump (mode-nested, `{ "color": ... }` entries).
    const V4: &str = r##"{
      "colors": {
        "primary": {
          "dark":    { "color": "#d0bcff" },
          "light":   { "color": "#6551a8" },
          "default": { "color": "#d0bcff" }
        },
        "secondary": {
          "dark":    { "color": "#ccc2dc" },
          "light":   { "color": "#625b71" },
          "default": { "color": "#ccc2dc" }
        },
        "source_color": {
          "dark":    { "color": "#6750a4" },
          "light":   { "color": "#6750a4" },
          "default": { "color": "#6750a4" }
        }
      },
      "mode": "dark",
      "scheme": "tonal-spot"
    }"##;

    /// A pre-4.0 dump (flat hex strings).
    const OLD: &str = r##"{
      "colors": { "primary": "#ff6673", "secondary": "#7f8c8d" },
      "palettes": {}
    }"##;

    /// matugen's `caching = true` file shape (mode first, #RRGGBBAA values).
    const CACHE: &str = r##"{
      "colors": {
        "light": { "primary": "#6551a8ff", "secondary": "#625b71ff" },
        "dark":  { "primary": "#d0bcffff", "secondary": "#ccc2dcff" }
      },
      "base16": { "light": { "base00": "#ffffffff" }, "dark": { "base00": "#000000ff" } }
    }"##;

    /// DankMaterialShell / Dux shell (`~/.cache/dms/dms-colors.json`).
    const DMS: &str = r##"{
      "colors": {
        "dark":  { "primary": "#a6c8ff", "secondary": "#8c94a8", "surface": "#111418" },
        "light": { "primary": "#3b608f", "secondary": "#59606e", "surface": "#f9f9ff" }
      },
      "mode": "dark"
    }"##;

    /// Caelestia shell (`~/.local/state/caelestia/scheme.json`): British
    /// spelling, bare hex without '#'.
    const CAELESTIA: &str = r##"{
      "name": "catppuccin-mocha",
      "flavour": "mocha",
      "mode": "dark",
      "colours": { "primary": "a6c8ff", "secondary": "8c94a8", "surface": "111418" }
    }"##;

    #[test]
    fn reads_the_default_variant_of_the_current_schema() {
        assert_eq!(
            parse_palette(V4, "primary", MatugenMode::Default),
            Some(Color::Rgb(0xd0, 0xbc, 0xff))
        );
    }

    #[test]
    fn reads_the_requested_variant() {
        assert_eq!(
            parse_palette(V4, "primary", MatugenMode::Light),
            Some(Color::Rgb(0x65, 0x51, 0xa8))
        );
        assert_eq!(
            parse_palette(V4, "primary", MatugenMode::Dark),
            Some(Color::Rgb(0xd0, 0xbc, 0xff))
        );
    }

    #[test]
    fn the_color_key_is_configurable() {
        assert_eq!(
            parse_palette(V4, "secondary", MatugenMode::Default),
            Some(Color::Rgb(0xcc, 0xc2, 0xdc))
        );
        assert_eq!(
            parse_palette(OLD, "primary", MatugenMode::Default),
            Some(Color::Rgb(0xff, 0x66, 0x73))
        );
    }

    #[test]
    fn reads_the_pre_4_flat_schema() {
        assert_eq!(
            parse_palette(OLD, "primary", MatugenMode::Default),
            Some(Color::Rgb(0xff, 0x66, 0x73))
        );
        // The old dump has no variants; any mode resolves to the flat value.
        assert_eq!(
            parse_palette(OLD, "secondary", MatugenMode::Light),
            Some(Color::Rgb(0x7f, 0x8c, 0x8d))
        );
    }

    #[test]
    fn reads_a_single_rendered_color_object() {
        let single = r##"{ "colors": { "primary": { "color": "#3daee9" } } }"##;
        assert_eq!(
            parse_palette(single, "primary", MatugenMode::Default),
            Some(Color::Rgb(0x3d, 0xae, 0xe9))
        );
    }

    #[test]
    fn alpha_suffixes_are_ignored() {
        // matugen's `format_hex_alpha` writes #RRGGBBAA (alpha last).
        let with_alpha = r##"{ "colors": { "primary": { "default": { "color": "#d0bcffcc" } } } }"##;
        assert_eq!(
            parse_palette(with_alpha, "primary", MatugenMode::Default),
            Some(Color::Rgb(0xd0, 0xbc, 0xff))
        );
        assert_eq!(parse_hex("#3dAEE9"), Some(Color::Rgb(0x3d, 0xae, 0xe9)));
    }

    #[test]
    fn a_missing_variant_falls_back_to_default() {
        let default_only =
            r##"{ "colors": { "primary": { "default": { "color": "#112233" } } } }"##;
        assert_eq!(
            parse_palette(default_only, "primary", MatugenMode::Light),
            Some(Color::Rgb(0x11, 0x22, 0x33))
        );
    }

    #[test]
    fn reads_matugens_own_cache_shape() {
        // No setup at all for a matugen user with `caching = true`.
        assert_eq!(
            parse_palette(CACHE, "primary", MatugenMode::Default),
            Some(Color::Rgb(0xd0, 0xbc, 0xff)),
            "Default follows matugen's dark default"
        );
        assert_eq!(
            parse_palette(CACHE, "primary", MatugenMode::Dark),
            Some(Color::Rgb(0xd0, 0xbc, 0xff))
        );
        assert_eq!(
            parse_palette(CACHE, "primary", MatugenMode::Light),
            Some(Color::Rgb(0x65, 0x51, 0xa8))
        );
        assert_eq!(
            parse_palette(CACHE, "secondary", MatugenMode::Default),
            Some(Color::Rgb(0xcc, 0xc2, 0xdc))
        );
    }

    #[test]
    fn malformed_or_missing_documents_yield_nothing() {
        assert_eq!(parse_palette("not json", "primary", MatugenMode::Default), None);
        assert_eq!(parse_palette("{}", "primary", MatugenMode::Default), None);
        assert_eq!(parse_palette(V4, "tertiary", MatugenMode::Default), None);
        assert_eq!(
            parse_palette(r##"{ "colors": { "primary": "blue" } }"##, "primary", MatugenMode::Default),
            None
        );
        assert_eq!(parse_hex("#zzzzzz"), None);
        assert_eq!(parse_hex("#12345"), None);
    }

    /// A throwaway matugen layout (`<root>/cache` + `<root>/config`).
    struct Layout(std::path::PathBuf);

    impl Layout {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("s2udio-matugen-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("cache/matugen/images")).expect("cache dir");
            std::fs::create_dir_all(root.join("config")).expect("config dir");
            std::fs::create_dir_all(root.join("state")).expect("state dir");
            Self(root)
        }

        fn cache(&self) -> PathBuf {
            self.0.join("cache")
        }

        fn config(&self) -> PathBuf {
            self.0.join("config")
        }

        fn state(&self) -> PathBuf {
            self.0.join("state")
        }

        fn write(&self, relative: &str, content: &str) -> PathBuf {
            let path = self.0.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("parent dir");
            }
            std::fs::write(&path, content).expect("write palette");
            path
        }
    }

    impl Drop for Layout {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn cfg() -> AutoThemeConfig {
        AutoThemeConfig {
            matugen_json: None,
            ..AutoThemeConfig::default()
        }
    }

    #[test]
    fn an_explicit_path_is_the_only_candidate() {
        let layout = Layout::new("explicit");
        layout.write("cache/matugen/colors.json", OLD);
        let explicit = layout.write(
            "explicit.json",
            r##"{ "colors": { "primary": "#112233" } }"##,
        );
        let mut cfg = cfg();
        cfg.matugen_json = Some(explicit.display().to_string());
        let (cache, config_root, state) = (layout.cache(), layout.config(), layout.state());
        assert_eq!(
            read_accent_in(&cfg, Some(&cache), Some(&config_root), Some(&state)),
            Some(Color::Rgb(0x11, 0x22, 0x33)),
            "the configured file wins over the conventional output"
        );
    }

    #[test]
    fn the_conventional_output_is_found_without_any_config() {
        let layout = Layout::new("conventional");
        layout.write("cache/matugen/colors.json", CACHE);
        let (cache, config_root, state) = (layout.cache(), layout.config(), layout.state());
        assert_eq!(
            read_accent_in(&cfg(), Some(&cache), Some(&config_root), Some(&state)),
            Some(Color::Rgb(0xd0, 0xbc, 0xff)),
            "no `matugen_json` needed for the usual redirect target"
        );
    }

    #[test]
    fn matugens_cache_is_used_when_no_json_output_exists() {
        let layout = Layout::new("cache");
        layout.write(
            "cache/matugen/images/wall.jpg.deadbeef.json",
            CACHE,
        );
        let (cache, config_root, state) = (layout.cache(), layout.config(), layout.state());
        assert_eq!(
            read_accent_in(&cfg(), Some(&cache), Some(&config_root), Some(&state)),
            Some(Color::Rgb(0xd0, 0xbc, 0xff)),
            "a `caching = true` setup needs no extra config either"
        );
    }

    #[test]
    fn the_newest_cache_entry_wins() {
        let layout = Layout::new("newest");
        let older = layout.write(
            "cache/matugen/images/old.jpg.aaaa.json",
            r##"{ "colors": { "dark": { "primary": "#000001ff" } } }"##,
        );
        let newer = layout.write(
            "cache/matugen/images/new.jpg.bbbb.json",
            r##"{ "colors": { "dark": { "primary": "#000002ff" } } }"##,
        );
        // Give the two files distinct mtimes.
        let now = std::time::SystemTime::now();
        let filetime = |path: &Path, offset: u64| {
            let _ = std::fs::File::options()
                .write(true)
                .open(path)
                .and_then(|file| {
                    file.set_modified(now + std::time::Duration::from_secs(offset))
                });
        };
        filetime(&older, 0);
        filetime(&newer, 60);
        assert_eq!(
            newest_cache_file(&layout.0.join("cache/matugen/images")),
            Some(newer),
            "the palette matugen wrote last is the current one"
        );
        let (cache, config_root, state) = (layout.cache(), layout.config(), layout.state());
        assert_eq!(
            read_accent_in(&cfg(), Some(&cache), Some(&config_root), Some(&state)),
            Some(Color::Rgb(0x00, 0x00, 0x02))
        );
    }

    #[test]
    fn reads_the_dankmaterialshell_palette_file() {
        assert_eq!(
            parse_palette(DMS, "primary", MatugenMode::Default),
            Some(Color::Rgb(0xa6, 0xc8, 0xff)),
            "DMS/Dux writes both variants; Default follows the dark one"
        );
        assert_eq!(
            parse_palette(DMS, "primary", MatugenMode::Light),
            Some(Color::Rgb(0x3b, 0x60, 0x8f))
        );
        assert_eq!(
            parse_palette(DMS, "secondary", MatugenMode::Default),
            Some(Color::Rgb(0x8c, 0x94, 0xa8))
        );
    }

    #[test]
    fn reads_the_caelestia_scheme_file() {
        assert_eq!(
            parse_palette(CAELESTIA, "primary", MatugenMode::Default),
            Some(Color::Rgb(0xa6, 0xc8, 0xff)),
            "Caelestia stores bare hex under `colours`"
        );
        assert_eq!(
            parse_palette(CAELESTIA, "secondary", MatugenMode::Light),
            Some(Color::Rgb(0x8c, 0x94, 0xa8)),
            "the scheme file holds one mode; any requested mode reads it"
        );
    }

    #[test]
    fn the_shell_palettes_are_discovered_without_any_setup() {
        let layout = Layout::new("shells");
        let (cache, config_root, state) = (layout.cache(), layout.config(), layout.state());
        // DMS / Dux.
        layout.write("cache/dms/dms-colors.json", DMS);
        assert_eq!(
            read_accent_in(&cfg(), Some(&cache), Some(&config_root), Some(&state)),
            Some(Color::Rgb(0xa6, 0xc8, 0xff)),
            "a DankMaterialShell user needs no s2udio configuration"
        );
        std::fs::remove_file(layout.0.join("cache/dms/dms-colors.json")).expect("cleanup");
        // Caelestia.
        layout.write("state/caelestia/scheme.json", CAELESTIA);
        assert_eq!(
            read_accent_in(&cfg(), Some(&cache), Some(&config_root), Some(&state)),
            Some(Color::Rgb(0xa6, 0xc8, 0xff)),
            "a Caelestia user needs no s2udio configuration either"
        );
    }

    #[test]
    fn a_json_palette_template_from_the_matugen_config_is_read() {
        let layout = Layout::new("template");
        let (cache, config_root, state) = (layout.cache(), layout.config(), layout.state());
        layout.write(
            "config/matugen/my-palette.json",
            r##"{ "colors": { "primary": { "default": { "color": "#123456" } } } }"##,
        );
        layout.write(
            "config/matugen/config.toml",
            "[templates.mypalette]\ninput_path = 'in.json'\noutput_path = 'my-palette.json'\n",
        );
        assert_eq!(
            read_accent_in(&cfg(), Some(&cache), Some(&config_root), Some(&state)),
            Some(Color::Rgb(0x12, 0x34, 0x56)),
            "a relative output_path resolves against the matugen config dir"
        );
    }

    #[test]
    fn the_conventional_output_beats_the_shell_palettes() {
        let layout = Layout::new("order");
        let (cache, config_root, state) = (layout.cache(), layout.config(), layout.state());
        layout.write("cache/dms/dms-colors.json", DMS);
        layout.write(
            "cache/matugen/colors.json",
            r##"{ "colors": { "primary": "#0a0b0c" } }"##,
        );
        assert_eq!(
            read_accent_in(&cfg(), Some(&cache), Some(&config_root), Some(&state)),
            Some(Color::Rgb(0x0a, 0x0b, 0x0c)),
            "an explicit matugen JSON output wins over a shell's copy"
        );
    }

    #[test]
    fn only_json_template_outputs_are_collected() {
        let config = r#"
[templates.term]
input_path = 'in.json'
output_path = '~/.config/kitty/colors.conf'

[templates.palette]
input_path = 'in.json'
output_path = "/tmp/palette.json"

[templates.many]
output_paths = ['a.json', "b.json", 'c.css']

# output_path = 'commented.json'
"#;
        let outputs = template_json_outputs(config, Path::new("/cfg"));
        assert_eq!(
            outputs,
            vec![
                PathBuf::from("/tmp/palette.json"),
                PathBuf::from("/cfg/a.json"),
                PathBuf::from("/cfg/b.json"),
            ],
            "JSON outputs only, relative paths against the config dir"
        );
    }

    #[test]
    fn nothing_on_disk_yields_no_accent() {
        let layout = Layout::new("empty");
        let (cache, config_root, state) = (layout.cache(), layout.config(), layout.state());
        assert_eq!(
            read_accent_in(&cfg(), Some(&cache), Some(&config_root), Some(&state)),
            None
        );
        // An unreadable / absent root is not an error either.
        assert_eq!(read_accent_in(&cfg(), None, None, None), None);
    }

    #[test]
    fn unknown_paths_yield_no_accent() {
        let mut cfg = cfg();
        cfg.matugen_json = Some("/nonexistent/s2udio-matugen.json".to_owned());
        assert_eq!(read_accent_in(&cfg, None, None, None), None);
    }
}
