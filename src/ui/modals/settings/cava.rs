use std::process::Command;

use anyhow::Result;

use super::{
    model::{PwNode, StagedCava},
    SettingsModal,
};
use crate::ctx::Ctx;

/// Every capture target for the cava device row: pactl's source list
/// (sink monitors + mics + virtual sources) when available, falling back to
/// the wpctl sink list (offered as `X.monitor`) on systems without
/// pipewire-pulse. `wpctl status` alone is not enough: it only lists sinks
/// (and omits their monitors), while cava must capture a sink's *monitor*
/// to visualize what it plays.
pub(super) fn pipewire_sources() -> Vec<PwNode> {
    if let Some(mut nodes) = pipewire_sources_pactl() {
        nodes.sort_by(|a, b| (a.is_virtual, &a.name).cmp(&(b.is_virtual, &b.name)));
        return nodes;
    }
    pipewire_sinks_wpctl()
}

/// `pactl list sources`: lists *every* source — including the `X.monitor`
/// monitors of sinks, which are exactly the cava `source` values wanted.
/// `None` when pactl (pipewire-pulse) is not installed or the server is
/// unreachable.
pub(super) fn pipewire_sources_pactl() -> Option<Vec<PwNode>> {
    let out = Command::new("pactl").arg("list").arg("sources").output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(parse_pactl_sources(&String::from_utf8_lossy(&out.stdout)))
}

/// Parse `pactl list sources`: each `Source #N` block's `Name:` (the cava
/// `source` value, e.g. `Media.monitor`), `Description:` (human-readable)
/// and the `node.virtual` property.
pub(super) fn parse_pactl_sources(stdout: &str) -> Vec<PwNode> {
    let mut result = Vec::new();
    for block in stdout.split("Source #").skip(1) {
        let mut name = None;
        let mut description = None;
        let mut is_virtual = false;
        for line in block.lines() {
            let line = line.trim();
            if let Some(v) = line.strip_prefix("Name:") {
                name = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("Description:") {
                description = Some(v.trim().to_string());
            } else if line.contains("node.virtual = \"true\"") {
                is_virtual = true;
            }
        }
        if let (Some(name), Some(description)) = (name, description) {
            result.push(PwNode {
                name,
                description,
                is_virtual,
            });
        }
    }
    result
}

/// Fallback when pactl is unavailable: the wpctl sink list, offered as
/// `X.monitor` capture sources (the form cava needs to visualize a sink —
/// a raw sink name makes cava use `target.object`, which PipeWire does not
/// feed on every setup).
pub(super) fn pipewire_sinks_wpctl() -> Vec<PwNode> {
    let mut result = Vec::new();
    let Ok(out) = Command::new("wpctl").arg("status").output() else {
        return result;
    };
    if !out.status.success() {
        return result;
    }
    for (id, description) in parse_wpctl_status(&String::from_utf8_lossy(&out.stdout)) {
        let Some((name, is_virtual)) = inspect_sink(id) else { continue };
        result.push(PwNode {
            name: format!("{name}.monitor"),
            description: format!("Monitor of {description}"),
            is_virtual,
        });
    }
    result
}

/// Parse `wpctl status`: the `Audio -> Sinks` section's `id. description`
/// lines.
pub(super) fn parse_wpctl_status(stdout: &str) -> Vec<(u32, String)> {
    let mut result = Vec::new();
    let mut section = String::new();
    for raw in stdout.lines() {
        let line = raw.trim();
        if line.starts_with("├─") || line.starts_with("└─") {
            let name = line
                .trim_start_matches(['├', '└'])
                .trim_start_matches('─')
                .trim()
                .trim_end_matches(':')
                .to_string();
            section = name;
            continue;
        }
        if section != "Sinks" {
            continue;
        }
        let Some(rest) = line.strip_prefix('│') else { continue };
        let rest = rest.trim();
        let Some(dot) = rest.find(". ") else { continue };
        let Ok(id) = rest[..dot].trim().parse::<u32>() else { continue };
        let description = rest[dot + 2..]
            .split(" [vol:")
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        if !description.is_empty() {
            result.push((id, description));
        }
    }
    result
}

pub(super) fn parse_wpctl_inspect(stdout: &str) -> Option<(String, bool)> {
    let mut name = None;
    let mut is_virtual = false;
    for raw in stdout.lines() {
        let line = raw.trim().trim_start_matches("* ").trim();
        if let Some(v) = line.strip_prefix("node.name = ") {
            name = Some(v.trim_matches('"').to_string());
        } else if let Some(v) = line.strip_prefix("node.virtual = ") {
            is_virtual = v.trim_matches('"') == "true";
        }
    }
    Some((name?, is_virtual))
}

pub(super) fn inspect_sink(id: u32) -> Option<(String, bool)> {
    let out = Command::new("wpctl").arg("inspect").arg(id.to_string()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_wpctl_inspect(&String::from_utf8_lossy(&out.stdout))
}

impl SettingsModal {
    /// Refresh the PipeWire capture-source list (with the "auto" entry
    /// first): sink monitors (`X.monitor`) plus mics/virtual sources.
    /// Round 30: cava is PipeWire-only, so the list is always shown.
    pub(super) fn refresh_nodes(&mut self) {
        let mut nodes = vec![PwNode {
            name: "auto".to_string(),
            description: "auto (default output)".to_string(),
            is_virtual: false,
        }];
        nodes.extend(pipewire_sources());
        self.nodes = nodes;
    }

    pub(super) fn source(&self) -> String {
        self.cava_pending.source.clone()
    }

    pub(super) fn fps(&self) -> u16 {
        self.cava_pending.framerate
    }

    pub(super) fn freq_min(&self) -> u16 {
        self.cava_pending.lower_cutoff_freq
    }

    pub(super) fn freq_max(&self) -> u32 {
        self.cava_pending.higher_cutoff_freq
    }

    pub(super) fn channels(&self) -> u32 {
        self.cava_pending.channels
    }

    pub(super) fn noise_reduction(&self) -> u8 {
        self.cava_pending.noise_reduction
    }

    pub(super) fn sensitivity(&self) -> u16 {
        self.cava_pending.sensitivity
    }

    pub(super) fn monstercat(&self) -> bool {
        self.cava_pending.monstercat
    }

    pub(super) fn waves(&self) -> bool {
        self.cava_pending.waves
    }

    /// The nodes currently offered to the device row. The "show virtual
    /// devices" toggle gates every virtual PipeWire node — the KDE
    /// split-sink monitors (`Media.monitor` etc.) and Easy Effects (sink
    /// monitor + processing source) — so off offers only real hardware
    /// capture points (plus "auto"); on offers everything. Non-virtual
    /// nodes are always offered.
    pub(super) fn visible_nodes(&self) -> Vec<PwNode> {
        self.nodes
            .iter()
            .filter(|n| self.ui_pending.show_virtual_devices || !n.is_virtual)
            .cloned()
            .collect()
    }

    /// Stage a cava change: the live config and the sidecar are untouched
    /// until the panel is closed with Save.
    pub(super) fn set_cava(
        &mut self,
        ctx: &mut Ctx,
        f: impl FnOnce(&mut StagedCava),
    ) -> Result<()> {
        f(&mut self.cava_pending);
        self.refresh_nodes();
        self.refresh_rows(ctx);
        ctx.render()?;
        Ok(())
    }

    /// Cycle the PipeWire monitor node by `delta` (-1 or +1).
    pub(super) fn cycle_device(&mut self, ctx: &mut Ctx, delta: i64) -> Result<()> {
        let nodes = self.visible_nodes();
        if nodes.is_empty() {
            return Ok(());
        }
        let current = self.source();
        let idx = nodes.iter().position(|n| n.name == current).unwrap_or(0);
        let next = (idx as i64 + delta).rem_euclid(nodes.len() as i64) as usize;
        let node = nodes[next].name.clone();
        self.set_cava(ctx, |c| c.source = node)
    }
}
