use std::{fmt::Write, path::PathBuf};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
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
    /// Round 30: cava is PipeWire-only; a legacy `method` field in older
    /// configs is an unknown field and is ignored by serde.
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
#[derive(Debug, Clone)]
pub struct CavaInput {
    /// The PipeWire capture source (`auto` or a sink/monitor/virtual source
    /// name). Round 30: cava is PipeWire-only, so there is no input method
    /// field anymore — the generated config always says `method = pipewire`.
    pub source: String,
    pub sample_rate: Option<u32>,
    pub sample_bits: Option<u32>,
    pub channels: Option<u32>,
    pub autoconnect: Option<u32>,
    /// Round 29: the PipeWire node name for the cava stream, or `None` to
    /// keep cava's own ("cava").
    pub node_name: Option<String>,
}
impl Default for CavaInput {
    fn default() -> Self {
        Self {
            source: "auto".to_string(),
            sample_rate: None,
            sample_bits: None,
            channels: None,
            autoconnect: None,
            node_name: None,
        }
    }
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
                source: sanitize_pipewire_source(&value.input.source),
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
/// Round 30: a cava `source` that names a fifo (an mpd.conf fifo path like
/// `/tmp/mpd-cava.fifo`) is meaningless for the PipeWire-only input; fall
/// back to the PipeWire default ("auto"). PipeWire node names never contain
/// a path separator, so any path-looking source is a leftover fifo.
fn sanitize_pipewire_source(source: &str) -> String {
    if source.is_empty() || source.ends_with(".fifo") || source.contains('/') {
        "auto".to_string()
    } else {
        source.to_string()
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
        writeln!(buf, "method = pipewire")?;
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
    /// Round 30: cava is PipeWire-only — a legacy `method` in an older
    /// `cava.ron` is an unknown field and is ignored by serde.
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
        if let Some(v) = &self.source {
            cava.input.source = sanitize_pipewire_source(v);
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
        let content = ron::ser::to_string_pretty(
            self,
            ron::ser::PrettyConfig::default(),
        )?;
        std::fs::write(&path, content)?;
        Ok(())
    }
}
