use anyhow::Result;
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DurationFormat {
    parts: Vec<FormatPart>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum FormatPart {
    Literal(String),
    Days,
    DaysPadded,
    Hours,
    HoursPadded,
    Minutes,
    MinutesPadded,
    Seconds,
    SecondsPadded,
    TotalSeconds,
}
impl Default for DurationFormat {
    fn default() -> Self {
        Self::parse("%m:%S").expect("default duration format should be valid")
    }
}
impl DurationFormat {
    pub fn parse(template: &str) -> Result<Self> {
        let mut parts = Vec::new();
        let mut literal = String::new();
        let mut chars = template.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '%' {
                match chars.peek() {
                    Some('%') => {
                        chars.next();
                        literal.push('%');
                    }
                    Some('d') => {
                        chars.next();
                        if !literal.is_empty() {
                            parts
                                .push(FormatPart::Literal(std::mem::take(&mut literal)));
                        }
                        parts.push(FormatPart::Days);
                    }
                    Some('D') => {
                        chars.next();
                        if !literal.is_empty() {
                            parts
                                .push(FormatPart::Literal(std::mem::take(&mut literal)));
                        }
                        parts.push(FormatPart::DaysPadded);
                    }
                    Some('h') => {
                        chars.next();
                        if !literal.is_empty() {
                            parts
                                .push(FormatPart::Literal(std::mem::take(&mut literal)));
                        }
                        parts.push(FormatPart::Hours);
                    }
                    Some('H') => {
                        chars.next();
                        if !literal.is_empty() {
                            parts
                                .push(FormatPart::Literal(std::mem::take(&mut literal)));
                        }
                        parts.push(FormatPart::HoursPadded);
                    }
                    Some('m') => {
                        chars.next();
                        if !literal.is_empty() {
                            parts
                                .push(FormatPart::Literal(std::mem::take(&mut literal)));
                        }
                        parts.push(FormatPart::Minutes);
                    }
                    Some('M') => {
                        chars.next();
                        if !literal.is_empty() {
                            parts
                                .push(FormatPart::Literal(std::mem::take(&mut literal)));
                        }
                        parts.push(FormatPart::MinutesPadded);
                    }
                    Some('s') => {
                        chars.next();
                        if !literal.is_empty() {
                            parts
                                .push(FormatPart::Literal(std::mem::take(&mut literal)));
                        }
                        parts.push(FormatPart::Seconds);
                    }
                    Some('S') => {
                        chars.next();
                        if !literal.is_empty() {
                            parts
                                .push(FormatPart::Literal(std::mem::take(&mut literal)));
                        }
                        parts.push(FormatPart::SecondsPadded);
                    }
                    Some('t') => {
                        chars.next();
                        if !literal.is_empty() {
                            parts
                                .push(FormatPart::Literal(std::mem::take(&mut literal)));
                        }
                        parts.push(FormatPart::TotalSeconds);
                    }
                    Some(&ch) => {
                        anyhow::bail!(
                            "Invalid format token '%{ch}' in duration_format template. Valid tokens are: %d, %D, %h, %H, %m, %M, %s, %S, %t, %%"
                        );
                    }
                    None => {
                        anyhow::bail!("Trailing '%' at end of duration_format template");
                    }
                }
            } else {
                literal.push(c);
            }
        }
        if !literal.is_empty() {
            parts.push(FormatPart::Literal(literal));
        }
        Ok(Self { parts })
    }
    pub fn format(&self, total_seconds: u64) -> String {
        use std::fmt::Write;
        let mut result = String::with_capacity(16);
        let days = total_seconds / 86400;
        let hours = (total_seconds / 3600) % 24;
        let minutes = (total_seconds / 60) % 60;
        let seconds = total_seconds % 60;
        for part in &self.parts {
            match part {
                FormatPart::Literal(s) => result.push_str(s),
                FormatPart::Days => {
                    let _ = write!(result, "{days}");
                }
                FormatPart::DaysPadded => {
                    let _ = write!(result, "{days:02}");
                }
                FormatPart::Hours => {
                    let _ = write!(result, "{hours}");
                }
                FormatPart::HoursPadded => {
                    let _ = write!(result, "{hours:02}");
                }
                FormatPart::Minutes => {
                    let _ = write!(result, "{minutes}");
                }
                FormatPart::MinutesPadded => {
                    let _ = write!(result, "{minutes:02}");
                }
                FormatPart::Seconds => {
                    let _ = write!(result, "{seconds}");
                }
                FormatPart::SecondsPadded => {
                    let _ = write!(result, "{seconds:02}");
                }
                FormatPart::TotalSeconds => {
                    let _ = write!(result, "{total_seconds}");
                }
            }
        }
        result
    }
}
