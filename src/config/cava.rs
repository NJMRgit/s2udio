use std::{fmt::Write, path::PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use strum::Display;

use super::defaults;
use crate::shared::paths::legacy_config_dir;

#[derive(Debug, Default, Clone)]
pub struct Cava {
    pub framerate: u16,
    pub autosens: bool,
    pub sensitivity: u16,
    pub lower_cutoff_freq: Option<u16>,
    pub higher_cutoff_freq: Option<u32>,
    pub input: CavaInput,
    pub smoothing: CavaSmoothing,
    pub eq: Vec<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CavaFile {
    framerate: u16,
    pub autosens: bool,
    pub sensitivity: u16,
    lower_cutoff_freq: Option<u16>,
    higher_cutoff_freq: Option<u32>,
    input: CavaInputFile,
    smoothing: CavaSmoothingFile,
    eq: Vec<f64>,
}

impl Default for CavaFile {
    fn default() -> Self {
        Self {
            framerate: 60,
            autosens: true,
            sensitivity: 100,
            lower_cutoff_freq: None,
            higher_cutoff_freq: None,
            input: CavaInputFile::default(),
            smoothing: CavaSmoothingFile::default(),
            eq: vec![],
        }
    }
}

#[derive(Debug, Display, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[strum(serialize_all = "lowercase")]
pub enum CavaInputMethod {
    Fifo,
    Alsa,
    #[default]
    Pulse,
    Portaudio,
    Pipewire,
    Sndio,
    Oss,
    Jack,
    Shmem,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CavaSmoothingFile {
    #[serde(default)]
    monstercat: bool,
    #[serde(default)]
    waves: bool,
    #[serde(default = "defaults::u8::<77>")]
    noise_reduction: u8,
}

#[derive(Debug, Default, Clone)]
pub struct CavaSmoothing {
    pub monstercat: bool,
    pub waves: bool,
    pub noise_reduction: u8,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CavaInputFile {
    method: CavaInputMethod,
    source: String,
    #[serde(default)]
    sample_rate: Option<u32>,
    #[serde(default)]
    sample_bits: Option<u32>,
    #[serde(default)]
    channels: Option<u32>,
    #[serde(default)]
    autoconnect: Option<u32>,
    /// Round 29: a name for the PipeWire node cava creates (cava hardcodes
    /// `node.name = "cava"`; s2udio renames it via an LD_PRELOAD shim that
    /// injects `node.name`/`media.name` from `CAVA_NODE_NAME`). `None` (the
    /// default) leaves cava's own name alone.
    #[serde(default)]
    node_name: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct CavaInput {
    pub method: CavaInputMethod,
    pub source: String,
    pub sample_rate: Option<u32>,
    pub sample_bits: Option<u32>,
    pub channels: Option<u32>,
    pub autoconnect: Option<u32>,
    /// Round 29: the PipeWire node name for the cava stream, or `None` to
    /// keep cava's own ("cava").
    pub node_name: Option<String>,
}

impl From<CavaFile> for Cava {
    fn from(value: CavaFile) -> Self {
        Cava {
            framerate: value.framerate,
            autosens: value.autosens,
            sensitivity: value.sensitivity,
            lower_cutoff_freq: value.lower_cutoff_freq,
            higher_cutoff_freq: value.higher_cutoff_freq,
            input: CavaInput {
                method: value.input.method,
                source: value.input.source,
                sample_rate: value.input.sample_rate,
                sample_bits: value.input.sample_bits,
                channels: value.input.channels,
                autoconnect: value.input.autoconnect,
                node_name: value.input.node_name,
            },
            smoothing: CavaSmoothing {
                monstercat: value.smoothing.monstercat,
                waves: value.smoothing.waves,
                noise_reduction: value.smoothing.noise_reduction,
            },
            eq: value.eq,
        }
    }
}

impl Cava {
    pub fn to_cava_config_file(&self, bars: u16) -> Result<String> {
        let mut buf = String::new();

        writeln!(buf, "[general]")?;
        writeln!(buf, "framerate = {}", self.framerate)?;
        writeln!(buf, "bars = {bars}")?;
        writeln!(buf, "autosens = {}", i8::from(self.autosens))?;
        writeln!(buf, "sensitivity = {}", self.sensitivity)?;
        if let Some(val) = self.lower_cutoff_freq {
            writeln!(buf, "lower_cutoff_freq = {val}")?;
        }
        if let Some(val) = self.higher_cutoff_freq {
            writeln!(buf, "higher_cutoff_freq = {val}")?;
        }

        writeln!(buf, "[input]")?;
        writeln!(buf, "method = {}", self.input.method)?;
        writeln!(buf, "source = {}", self.input.source)?;
        if let Some(val) = self.input.sample_rate {
            writeln!(buf, "sample_rate = {val}")?;
        }
        if let Some(val) = self.input.sample_bits {
            writeln!(buf, "sample_bits = {val}")?;
        }
        if let Some(val) = self.input.channels {
            writeln!(buf, "channels = {val}")?;
        }
        if let Some(val) = self.input.autoconnect {
            writeln!(buf, "autoconnect = {val}")?;
        }

        writeln!(buf, "[output]")?;
        writeln!(buf, "method = raw")?;
        writeln!(buf, "channels = mono")?;
        writeln!(buf, "data_format = binary")?;
        writeln!(buf, "bit_format = 16bit")?;
        writeln!(buf, "reverse = 0")?;

        writeln!(buf, "[smoothing]")?;
        writeln!(buf, "noise_reduction = {}", self.smoothing.noise_reduction)?;
        writeln!(buf, "monstercat = {}", i8::from(self.smoothing.monstercat))?;
        writeln!(buf, "waves = {}", i8::from(self.smoothing.waves))?;

        writeln!(buf, "[eq]")?;
        for (i, val) in self.eq.iter().enumerate() {
            writeln!(buf, "{i} = {val}")?;
        }

        Ok(buf)
    }
}

/// Cava settings adjusted from the Settings panel, persisted to the
/// `cava.ron` sidecar (the main config file is never rewritten). Only the
/// fields the panel manages are stored; `None` leaves the configured value
/// untouched on merge.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CavaOverridesFile {
    pub framerate: Option<u16>,
    pub autosens: Option<bool>,
    pub sensitivity: Option<u16>,
    pub lower_cutoff_freq: Option<u16>,
    pub higher_cutoff_freq: Option<u32>,
    pub channels: Option<u32>,
    pub method: Option<CavaInputMethod>,
    pub source: Option<String>,
    pub noise_reduction: Option<u8>,
    pub monstercat: Option<bool>,
    pub waves: Option<bool>,
    /// Round 29: PipeWire node name for the cava stream. `Some(name)`
    /// renames the node; `Some("")` explicitly disables renaming; `None`
    /// leaves the configured value untouched (the usual sidecar semantic).
    pub node_name: Option<String>,
}

pub const CAVA_OVERRIDE_FILE: &str = "cava.ron";

impl CavaOverridesFile {
    /// The s2udio sidecar path (`~/.config/s2udio/cava.ron`, round 19 —
    /// s2udio's settings sidecar moved out of `~/.config/rmpc/`).
    pub fn path() -> Option<PathBuf> {
        crate::shared::paths::s2udio_config_dir().map(|dir| dir.join(CAVA_OVERRIDE_FILE))
    }

    /// The legacy pre-round-23 sidecar path (`~/.config/rmpc/cava.ron`).
    fn legacy_path() -> Option<PathBuf> {
        legacy_config_dir().map(|dir| dir.join(CAVA_OVERRIDE_FILE))
    }

    /// Read the sidecar file, if it exists and parses. Falls back to the
    /// legacy `~/.config/rmpc/cava.ron` when the s2udio file is absent.
    pub fn load() -> Option<Self> {
        let path = Self::path()
            .filter(|p| p.exists())
            .or_else(Self::legacy_path)
            .or_else(Self::path)?;
        let content = std::fs::read_to_string(&path).ok()?;
        match ron::de::from_str(&content) {
            Ok(overrides) => Some(overrides),
            Err(err) => {
                log::warn!(path:?; "Failed to parse cava overrides: {err}");
                None
            }
        }
    }

    /// Apply the overrides on top of the configured cava settings.
    pub fn apply_to(&self, cava: &mut Cava) {
        if let Some(v) = self.framerate {
            cava.framerate = v;
        }
        if let Some(v) = self.autosens {
            cava.autosens = v;
        }
        if let Some(v) = self.sensitivity {
            cava.sensitivity = v;
        }
        if let Some(v) = self.lower_cutoff_freq {
            cava.lower_cutoff_freq = Some(v);
        }
        if let Some(v) = self.higher_cutoff_freq {
            cava.higher_cutoff_freq = Some(v);
        }
        if let Some(v) = self.channels {
            cava.input.channels = Some(v);
        }
        if let Some(v) = &self.method {
            cava.input.method = v.clone();
        }
        if let Some(v) = &self.source {
            cava.input.source = v.clone();
        }
        if let Some(v) = self.noise_reduction {
            cava.smoothing.noise_reduction = v;
        }
        if let Some(v) = self.monstercat {
            cava.smoothing.monstercat = v;
        }
        if let Some(v) = self.waves {
            cava.smoothing.waves = v;
        }
        if let Some(v) = &self.node_name {
            // Some("") = explicit disable; Some(name) = rename the node.
            cava.input.node_name = (!v.is_empty()).then(|| v.clone());
        }
    }

    /// Persist the overrides to the sidecar file.
    pub fn save(&self) -> Result<()> {
        let Some(path) = Self::path() else {
            bail!("Could not determine config directory");
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = ron::ser::to_string_pretty(self, ron::ser::PrettyConfig::default())?;
        std::fs::write(&path, content)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round 29: the sidecar can set the cava node name (and explicitly
    /// disable it with an empty string); `None` leaves it untouched.
    #[test]
    fn node_name_override_sets_and_disables() {
        let mut cava = Cava::default();
        assert_eq!(cava.input.node_name, None);

        let set = CavaOverridesFile { node_name: Some("s2udio-cava".into()), ..Default::default() };
        set.apply_to(&mut cava);
        assert_eq!(cava.input.node_name.as_deref(), Some("s2udio-cava"));

        // Some("") explicitly disables the rename.
        let off = CavaOverridesFile { node_name: Some(String::new()), ..Default::default() };
        off.apply_to(&mut cava);
        assert_eq!(cava.input.node_name, None, "an empty node_name disables the rename");

        // None leaves the resolved value alone.
        cava.input.node_name = Some("keep-me".into());
        let untouched = CavaOverridesFile::default();
        untouched.apply_to(&mut cava);
        assert_eq!(cava.input.node_name.as_deref(), Some("keep-me"));
    }

    /// The main config's cava.input accepts `node_name` (round 29).
    #[test]
    fn main_config_input_parses_node_name() {
        let content = r#"(input: (method: Pipewire, source: "Media.monitor", node_name: Some("s2udio-cava")))"#;
        let parsed: CavaFile = ron::de::from_str(content).unwrap();
        let cava: Cava = parsed.into();
        assert_eq!(cava.input.node_name.as_deref(), Some("s2udio-cava"));
    }

    #[test]
    fn legacy_sidecar_with_removed_fields_still_parses() {
        // A cava.ron written before the sample rate / bit depth options were
        // removed must still load: the unknown fields are ignored and the
        // rest applies.
        let content = r#"(framerate: Some(90), autosens: Some(false), sensitivity: Some(175),
            lower_cutoff_freq: Some(20), higher_cutoff_freq: Some(15000),
            sample_rate: Some(48000), sample_bits: Some(24), channels: Some(2),
            method: Some(Fifo), source: Some("/tmp/mpd-cava.fifo"),
            noise_reduction: Some(10), monstercat: Some(false), waves: Some(false))"#;
        let parsed: CavaOverridesFile = ron::de::from_str(content).unwrap();
        assert_eq!(parsed.framerate, Some(90));
        assert_eq!(parsed.sensitivity, Some(175));
        assert_eq!(parsed.channels, Some(2));
    }

}
