use anyhow::{Context, Result, bail};
use bitflags::bitflags;
use bon::Builder;
use ratatui::style::Color as RColor;
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
pub trait ToConfigOr {
    fn to_config_or(
        &self,
        default_fg: Option<RColor>,
        default_bg: Option<RColor>,
    ) -> Result<ratatui::style::Style>;
}
pub(super) struct StringColor(pub Option<String>);
impl StringColor {
    pub fn to_color(&self) -> Result<Option<RColor>> {
        let fg: Option<ConfigColor> = self
            .0
            .as_ref()
            .map(|v| v.as_bytes().try_into())
            .transpose()?;
        Ok(fg.map(std::convert::Into::into))
    }
}
#[skip_serializing_none]
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq, Builder)]
pub struct StyleFile {
    pub fg: Option<String>,
    pub bg: Option<String>,
    pub modifiers: Option<Modifiers>,
}
impl std::fmt::Display for Modifiers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.contains(Modifiers::Bold) {
            write!(f, "b")?;
        }
        if self.contains(Modifiers::Dim) {
            write!(f, "d")?;
        }
        if self.contains(Modifiers::Italic) {
            write!(f, "i")?;
        }
        if self.contains(Modifiers::Underlined) {
            write!(f, "u")?;
        }
        if self.contains(Modifiers::Reversed) {
            write!(f, "r")?;
        }
        if self.contains(Modifiers::CrossedOut) {
            write!(f, "c")?;
        }
        Ok(())
    }
}
impl std::fmt::Display for StyleFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f, "Style({},{},{})", match self.fg { Some(ref fg) => fg.to_owned(), None =>
            "none".to_string(), }, match self.bg { Some(ref bg) => bg.to_owned(), None =>
            "none".to_string(), }, self.modifiers.as_ref().map_or_else(|| "none"
            .to_string(), ToString::to_string)
        )
    }
}
#[allow(clippy::similar_names)]
impl ToConfigOr for StyleFile {
    fn to_config_or(
        &self,
        default_fg: Option<RColor>,
        default_bg: Option<RColor>,
    ) -> Result<ratatui::style::Style> {
        let fg: Option<ConfigColor> = self
            .fg
            .as_ref()
            .map(|s| s.as_bytes().try_into())
            .transpose()?;
        let fg: Option<RColor> = fg.map(Into::into).or(default_fg);
        let bg: Option<ConfigColor> = self
            .bg
            .as_ref()
            .map(|s| s.as_bytes().try_into())
            .transpose()?;
        let bg: Option<RColor> = bg.map(Into::into).or(default_bg);
        let modifiers = self
            .modifiers
            .as_ref()
            .map_or(ratatui::style::Modifier::empty(), Into::into);
        let mut result = ratatui::style::Style::default();
        if let Some(fg) = fg {
            result = result.fg(fg);
        }
        if let Some(bg) = bg {
            result = result.bg(bg);
        }
        Ok(result.add_modifier(modifiers))
    }
}
#[allow(clippy::similar_names)]
impl ToConfigOr for Option<StyleFile> {
    fn to_config_or(
        &self,
        default_fg: Option<RColor>,
        default_bg: Option<RColor>,
    ) -> Result<ratatui::style::Style> {
        if let Some(val) = self {
            let fg: Option<ConfigColor> = val
                .fg
                .as_ref()
                .map(|s| s.as_bytes().try_into())
                .transpose()?;
            let fg: Option<RColor> = fg.map(Into::into).or(default_fg);
            let bg: Option<ConfigColor> = val
                .bg
                .as_ref()
                .map(|s| s.as_bytes().try_into())
                .transpose()?;
            let bg: Option<RColor> = bg.map(Into::into).or(default_bg);
            let modifiers = val
                .modifiers
                .as_ref()
                .map_or(ratatui::style::Modifier::empty(), Into::into);
            let mut result = ratatui::style::Style::default();
            if let Some(fg) = fg {
                result = result.fg(fg);
            }
            if let Some(bg) = bg {
                result = result.bg(bg);
            }
            Ok(result.add_modifier(modifiers))
        } else {
            let mut result = ratatui::style::Style::default();
            if let Some(fg) = default_fg {
                result = result.fg(fg);
            }
            if let Some(bg) = default_bg {
                result = result.bg(bg);
            }
            Ok(result)
        }
    }
}
impl TryFrom<&[u8]> for crate::config::ConfigColor {
    type Error = anyhow::Error;
    fn try_from(input: &[u8]) -> Result<Self, Self::Error> {
        match input {
            b"reset" => Ok(Self::Reset),
            b"default" => Ok(Self::Reset),
            b"black" => Ok(Self::Black),
            b"red" => Ok(Self::Red),
            b"green" => Ok(Self::Green),
            b"yellow" => Ok(Self::Yellow),
            b"blue" => Ok(Self::Blue),
            b"magenta" => Ok(Self::Magenta),
            b"cyan" => Ok(Self::Cyan),
            b"gray" => Ok(Self::Gray),
            b"dark_gray" => Ok(Self::DarkGray),
            b"light_red" => Ok(Self::LightRed),
            b"light_green" => Ok(Self::LightGreen),
            b"light_yellow" => Ok(Self::LightYellow),
            b"light_blue" => Ok(Self::LightBlue),
            b"light_magenta" => Ok(Self::LightMagenta),
            b"light_cyan" => Ok(Self::LightCyan),
            b"white" => Ok(Self::White),
            s if input.len() == 7 && input.first().is_some_and(|v| v == &b'#') => {
                let res = std::str::from_utf8(s.strip_prefix(b"#").context("")?)?;
                let res = u32::from_str_radix(res, 16).context("")?;
                Ok(Self::Hex(res))
            }
            s if s.starts_with(b"rgb(") => {
                let mut colors = std::str::from_utf8(
                        s
                            .strip_prefix(b"rgb(")
                            .context("")?
                            .strip_suffix(b")")
                            .context("")?,
                    )?
                    .splitn(3, ',');
                let r = colors
                    .next()
                    .with_context(|| {
                        format!("No red color present in {}", String::from_utf8_lossy(s))
                    })?
                    .trim()
                    .parse::<u8>()
                    .with_context(|| {
                        format!(
                            "Failed to parse RGB color: {}", String::from_utf8_lossy(s)
                        )
                    })?;
                let g = colors
                    .next()
                    .with_context(|| {
                        format!(
                            "No green color present in {}", String::from_utf8_lossy(s)
                        )
                    })?
                    .trim()
                    .parse::<u8>()
                    .with_context(|| {
                        format!(
                            "Failed to parse RGB color: {}", String::from_utf8_lossy(s)
                        )
                    })?;
                let b = colors
                    .next()
                    .with_context(|| {
                        format!(
                            "No blue color present in {}", String::from_utf8_lossy(s)
                        )
                    })?
                    .trim()
                    .parse::<u8>()
                    .with_context(|| {
                        format!(
                            "Failed to parse RGB color: {}", String::from_utf8_lossy(s)
                        )
                    })?;
                Ok(Self::Rgb(r, g, b))
            }
            s => {
                if let Ok(s) = std::str::from_utf8(s) {
                    if let Ok(v) = s.parse::<u8>() {
                        Ok(Self::Indexed(v))
                    } else {
                        bail!("Invalid color format '{s}'")
                    }
                } else {
                    bail!("Invalid color format '{s:?}'")
                }
            }
        }
    }
}
bitflags! {
    #[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)] pub struct
    Modifiers : u16 { const Bold = 0b0000_0000_0001; const Dim = 0b0000_0000_0010; const
    Italic = 0b0000_0000_0100; const Underlined = 0b0000_0000_1000; const Reversed =
    0b0000_0100_0000; const CrossedOut = 0b0001_0000_0000; }
}
impl From<Modifiers> for ratatui::style::Modifier {
    fn from(value: Modifiers) -> Self {
        (&value).into()
    }
}
impl From<&Modifiers> for ratatui::style::Modifier {
    fn from(value: &Modifiers) -> Self {
        Self::from_bits_retain(value.bits())
    }
}
#[derive(Debug, Serialize, Deserialize, Eq, PartialEq)]
pub enum ConfigColor {
    Reset,
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
    DarkGray,
    LightRed,
    LightGreen,
    LightYellow,
    LightBlue,
    LightMagenta,
    LightCyan,
    White,
    Hex(u32),
    Rgb(u8, u8, u8),
    Indexed(u8),
}
impl From<crate::config::ConfigColor> for RColor {
    fn from(value: crate::config::ConfigColor) -> Self {
        use crate::config::ConfigColor as CColor;
        match value {
            CColor::Reset => RColor::Reset,
            CColor::Black => RColor::Black,
            CColor::Red => RColor::Red,
            CColor::Green => RColor::Green,
            CColor::Yellow => RColor::Yellow,
            CColor::Blue => RColor::Blue,
            CColor::Magenta => RColor::Magenta,
            CColor::Cyan => RColor::Cyan,
            CColor::Gray => RColor::Gray,
            CColor::DarkGray => RColor::DarkGray,
            CColor::LightRed => RColor::LightRed,
            CColor::LightGreen => RColor::LightGreen,
            CColor::LightYellow => RColor::LightYellow,
            CColor::LightBlue => RColor::LightBlue,
            CColor::LightMagenta => RColor::LightMagenta,
            CColor::LightCyan => RColor::LightCyan,
            CColor::White => RColor::White,
            CColor::Rgb(r, g, b) => RColor::Rgb(r, g, b),
            CColor::Hex(v) => RColor::from_u32(v),
            CColor::Indexed(v) => RColor::Indexed(v),
        }
    }
}
