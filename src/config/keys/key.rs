use std::{fmt::Display, str::FromStr};
use crossterm::event::{KeyCode, KeyEvent as CKeyEvent, KeyModifiers};
use serde_with::{DeserializeFromStr, SerializeDisplay};
use winnow::{
    Parser, Result,
    combinator::{alt, dispatch, empty, fail, opt, permutation, repeat, seq, trace},
    token::{any, literal},
};
#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub struct Key {
    pub key: KeyCode,
    pub modifiers: KeyModifiers,
}
impl From<CKeyEvent> for Key {
    fn from(value: CKeyEvent) -> Self {
        let should_insert_shift = matches!(
            value.code, KeyCode::Char(c) if c.is_uppercase()
        );
        let mut modifiers = value.modifiers;
        if should_insert_shift {
            modifiers.insert(KeyModifiers::SHIFT);
        }
        let key = if modifiers.contains(KeyModifiers::SHIFT) {
            if let KeyCode::Char(c) = value.code {
                KeyCode::Char(c.to_ascii_uppercase())
            } else {
                value.code
            }
        } else {
            value.code
        };
        Self { key, modifiers }
    }
}
#[derive(
    Debug,
    SerializeDisplay,
    DeserializeFromStr,
    PartialEq,
    Eq,
    Hash,
    Clone,
    derive_more::IntoIterator,
)]
pub struct KeySequence(pub Vec<Key>);
impl KeySequence {
    pub fn iter(&self) -> impl Iterator<Item = &Key> {
        let mut iter = self.0.iter();
        std::iter::from_fn(move || iter.next())
    }
    pub fn new() -> Self {
        Self(Vec::new())
    }
    pub fn char(mut self, c: char) -> Self {
        let key = if c.is_uppercase() {
            Key {
                key: KeyCode::Char(c.to_ascii_uppercase()),
                modifiers: KeyModifiers::SHIFT,
            }
        } else {
            Key {
                key: KeyCode::Char(c),
                modifiers: KeyModifiers::NONE,
            }
        };
        self.0.push(key);
        self
    }
    pub fn shift(mut self) -> Self {
        if let Some(last_key) = self.0.last_mut()
            && !matches!(last_key.key, KeyCode::Char(_))
        {
            if matches!(last_key.key, KeyCode::Tab) {
                last_key.key = KeyCode::BackTab;
            }
            last_key.modifiers |= KeyModifiers::SHIFT;
        }
        self
    }
    /// Add the CONTROL modifier to the last key.
    #[allow(dead_code)]
    pub fn ctrl(mut self) -> Self {
        if let Some(last_key) = self.0.last_mut() {
            last_key.modifiers |= KeyModifiers::CONTROL;
        }
        self
    }
    pub fn tab(mut self) -> Self {
        self.0
            .push(Key {
                key: KeyCode::Tab,
                modifiers: KeyModifiers::NONE,
            });
        self
    }
    pub fn cr(mut self) -> Self {
        self.0
            .push(Key {
                key: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
            });
        self
    }
    pub fn up(mut self) -> Self {
        self.0
            .push(Key {
                key: KeyCode::Up,
                modifiers: KeyModifiers::NONE,
            });
        self
    }
    pub fn down(mut self) -> Self {
        self.0
            .push(Key {
                key: KeyCode::Down,
                modifiers: KeyModifiers::NONE,
            });
        self
    }
    pub fn left(mut self) -> Self {
        self.0
            .push(Key {
                key: KeyCode::Left,
                modifiers: KeyModifiers::NONE,
            });
        self
    }
    pub fn right(mut self) -> Self {
        self.0
            .push(Key {
                key: KeyCode::Right,
                modifiers: KeyModifiers::NONE,
            });
        self
    }
    pub fn esc(mut self) -> Self {
        self.0
            .push(Key {
                key: KeyCode::Esc,
                modifiers: KeyModifiers::NONE,
            });
        self
    }
    pub fn page_up(mut self) -> Self {
        self.0
            .push(Key {
                key: KeyCode::PageUp,
                modifiers: KeyModifiers::NONE,
            });
        self
    }
    pub fn page_down(mut self) -> Self {
        self.0
            .push(Key {
                key: KeyCode::PageDown,
                modifiers: KeyModifiers::NONE,
            });
        self
    }
    pub fn delete(mut self) -> Self {
        self.0
            .push(Key {
                key: KeyCode::Delete,
                modifiers: KeyModifiers::NONE,
            });
        self
    }
}
impl From<Key> for KeySequence {
    fn from(key: Key) -> Self {
        Self(vec![key])
    }
}
impl From<Vec<Key>> for KeySequence {
    fn from(keys: Vec<Key>) -> Self {
        Self(keys)
    }
}
impl Display for KeySequence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.iter().try_for_each(|key| write!(f, "{key}"))
    }
}
impl FromStr for KeySequence {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let keys = parse_sequence.parse(s).map_err(|e| anyhow::format_err!("{e}"))?;
        Ok(Self(keys))
    }
}
impl Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let has_ctrl = self.modifiers.contains(KeyModifiers::CONTROL);
        let has_alt = self.modifiers.contains(KeyModifiers::ALT);
        let has_shift = self.modifiers.contains(KeyModifiers::SHIFT);
        let has_no_modifiers = !has_ctrl && !has_alt && !has_shift;
        if has_ctrl || has_alt
            || (has_shift && !matches!(self.key, KeyCode::Char(c) if c.is_alphabetic()))
        {
            write!(f, "<")?;
        }
        if has_ctrl {
            write!(f, "C-")?;
        }
        if has_alt {
            write!(f, "A-")?;
        }
        if has_shift && !matches!(self.key, KeyCode::Char(c) if c.is_alphabetic()) {
            write!(f, "S-")?;
        }
        match self.key {
            KeyCode::Backspace if has_no_modifiers => write!(f, "<BS>"),
            KeyCode::Backspace => write!(f, "BS"),
            KeyCode::Enter if has_no_modifiers => write!(f, "<CR>"),
            KeyCode::Enter => write!(f, "CR"),
            KeyCode::Left if has_no_modifiers => write!(f, "<Left>"),
            KeyCode::Left => write!(f, "Left"),
            KeyCode::Right if has_no_modifiers => write!(f, "<Right>"),
            KeyCode::Right => write!(f, "Right"),
            KeyCode::Up if has_no_modifiers => write!(f, "<Up>"),
            KeyCode::Up => write!(f, "Up"),
            KeyCode::Down if has_no_modifiers => write!(f, "<Down>"),
            KeyCode::Down => write!(f, "Down"),
            KeyCode::Home if has_no_modifiers => write!(f, "<Home>"),
            KeyCode::Home => write!(f, "Home"),
            KeyCode::End if has_no_modifiers => write!(f, "<End>"),
            KeyCode::End => write!(f, "End"),
            KeyCode::PageUp if has_no_modifiers => write!(f, "<PageUp>"),
            KeyCode::PageUp => write!(f, "PageUp"),
            KeyCode::PageDown if has_no_modifiers => write!(f, "<PageDown>"),
            KeyCode::PageDown => write!(f, "PageDown"),
            KeyCode::Tab if has_no_modifiers => write!(f, "<Tab>"),
            KeyCode::Tab => write!(f, "Tab"),
            KeyCode::BackTab if has_no_modifiers => write!(f, "<Tab>"),
            KeyCode::BackTab => write!(f, "Tab"),
            KeyCode::Delete if has_no_modifiers => write!(f, "<Del>"),
            KeyCode::Delete => write!(f, "Del"),
            KeyCode::Insert if has_no_modifiers => write!(f, "<Insert>"),
            KeyCode::Insert => write!(f, "Insert"),
            KeyCode::Esc if has_no_modifiers => write!(f, "<Esc>"),
            KeyCode::Esc => write!(f, "Esc"),
            KeyCode::F(num) if has_no_modifiers => write!(f, "<F{num}>"),
            KeyCode::F(num) => write!(f, "F{num}"),
            KeyCode::Char(' ') if has_no_modifiers => write!(f, "<Space>"),
            KeyCode::Char(' ') => write!(f, "Space"),
            KeyCode::Char(char) => write!(f, "{char}"),
            KeyCode::CapsLock
            | KeyCode::ScrollLock
            | KeyCode::NumLock
            | KeyCode::PrintScreen
            | KeyCode::Pause
            | KeyCode::Menu
            | KeyCode::KeypadBegin
            | KeyCode::Media(_)
            | KeyCode::Modifier(_)
            | KeyCode::Null => Ok(()),
        }?;
        if has_ctrl || has_alt
            || (has_shift && !matches!(self.key, KeyCode::Char(c) if c.is_alphabetic()))
        {
            write!(f, ">")?;
        }
        Ok(())
    }
}
fn parse_sequence(input: &mut &str) -> winnow::error::Result<Vec<Key>> {
    repeat(1.., parse_key).parse_next(input)
}
fn parse_key(input: &mut &str) -> winnow::error::Result<Key> {
    let ((modifiers, key),) = alt((
            trace(
                "with modifiers or special key",
                seq! {
                    _ : '<', | input : & mut & str | { let mut mods = parse_modifier
                    .parse_next(input) ?; match alt((parse_special_key, trace("char",
                    parse_char_key))).parse_next(input) { Ok((mods2, mut key)) => { mods
                    |= mods2; if mods.contains(KeyModifiers::SHIFT) && matches!(key,
                    KeyCode::Tab) { key = KeyCode::BackTab; } Ok((mods, key)) }, Err(err)
                    => { return Err(err); }, } }, _ : '>'
                },
            ),
            trace("single char key", parse_char_key.map(|v| (v,))),
        ))
        .parse_next(input)?;
    Ok(Key { key, modifiers })
}
fn parse_modifier(input: &mut &str) -> winnow::error::Result<KeyModifiers> {
    let mods = permutation((
            opt(literal("C-").value(KeyModifiers::CONTROL)),
            opt(literal("A-").value(KeyModifiers::ALT)),
            opt(literal("S-").value(KeyModifiers::SHIFT)),
        ))
        .parse_next(input)?;
    let mut modifiers = KeyModifiers::NONE;
    for modifier in [mods.0, mods.1, mods.2] {
        match modifier {
            Some(KeyModifiers::CONTROL) => modifiers |= KeyModifiers::CONTROL,
            Some(KeyModifiers::ALT) => modifiers |= KeyModifiers::ALT,
            Some(KeyModifiers::SHIFT) => modifiers |= KeyModifiers::SHIFT,
            _ => {}
        }
    }
    Ok(modifiers)
}
fn parse_char_key(input: &mut &str) -> Result<(KeyModifiers, KeyCode)> {
    let c = any.parse_next(input)?;
    if c.is_uppercase() {
        Ok((KeyModifiers::SHIFT, KeyCode::Char(c.to_ascii_uppercase())))
    } else {
        Ok((KeyModifiers::NONE, KeyCode::Char(c)))
    }
}
fn parse_special_key(
    input: &mut &str,
) -> winnow::error::Result<(KeyModifiers, KeyCode)> {
    let mut parser = alt((
        alt((
            "BS",
            "Backspace",
            "CR",
            "Enter",
            "Left",
            "Right",
            "Up",
            "Down",
            "Home",
            "End",
            "PageUp",
            "PageDown",
            "Tab",
        )),
        alt((
            "Del",
            "Insert",
            "Esc",
            "Space",
            "F10",
            "F11",
            "F12",
            "F1",
            "F2",
            "F3",
            "F4",
            "F5",
            "F6",
            "F7",
            "F8",
            "F9",
        )),
    ));
    let mut parser = dispatch! {
        parser; "BS" => empty.value(KeyCode::Backspace), "Backspace" => empty
        .value(KeyCode::Backspace), "CR" => empty.value(KeyCode::Enter), "Enter" => empty
        .value(KeyCode::Enter), "Left" => empty.value(KeyCode::Left), "Right" => empty
        .value(KeyCode::Right), "Up" => empty.value(KeyCode::Up), "Down" => empty
        .value(KeyCode::Down), "Home" => empty.value(KeyCode::Home), "End" => empty
        .value(KeyCode::End), "PageUp" => empty.value(KeyCode::PageUp), "PageDown" =>
        empty.value(KeyCode::PageDown), "Tab" => empty.value(KeyCode::Tab), "Del" =>
        empty.value(KeyCode::Delete), "Insert" => empty.value(KeyCode::Insert), "Esc" =>
        empty.value(KeyCode::Esc), "Space" => empty.value(KeyCode::Char(' ')), "F10" =>
        empty.value(KeyCode::F(10)), "F11" => empty.value(KeyCode::F(11)), "F12" => empty
        .value(KeyCode::F(12)), "F1" => empty.value(KeyCode::F(1)), "F2" => empty
        .value(KeyCode::F(2)), "F3" => empty.value(KeyCode::F(3)), "F4" => empty
        .value(KeyCode::F(4)), "F5" => empty.value(KeyCode::F(5)), "F6" => empty
        .value(KeyCode::F(6)), "F7" => empty.value(KeyCode::F(7)), "F8" => empty
        .value(KeyCode::F(8)), "F9" => empty.value(KeyCode::F(9)), "" => empty
        .value(KeyCode::Null), _ => fail,
    };
    parser.parse_next(input).map(|key| (KeyModifiers::NONE, key))
}
