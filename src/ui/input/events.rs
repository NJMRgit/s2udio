use crossterm::event::{KeyCode, KeyModifiers};

use crate::config::keys::Key;

#[derive(Debug, PartialEq)]
pub enum InputResultEvent {
    Push,
    Pop,
    Confirm,
    NoChange,
    Cancel,
    /// The cursor is already at the buffer's start when a `Back` (Left)
    /// movement arrives (round 63.1): the move cannot happen, so the pane
    /// may treat the key as a request to leave (search staged exit) or
    /// ignore it. Distinct from `CursorLeft`, which follows a `Back` that
    /// DID move the cursor.
    AtStart,
    /// A `Back` (Left) movement that moved the cursor to the previous
    /// grapheme (round 63.1): search bars count consecutive occasions to
    /// let the SECOND Left at the bar exit exactly like Esc #2 — the
    /// insert-mode text cursor must stay usable for the first press.
    /// Every other consumer ignores it exactly like `NoChange`.
    CursorLeft,
}

#[derive(Debug, Clone, Copy)]
pub enum InputEvent {
    Push(char),

    // Delete
    PopLeft,
    PopRight,
    PopWordLeft,
    PopWordRight,
    DeleteToStart,
    DeleteToEnd,

    // Movement
    Forward,
    Back,
    Start,
    End,
    ForwardWord,
    BackWord,
}

impl InputEvent {
    pub fn from_key_event(ev: Key) -> Option<Self> {
        match ev.key {
            // Movement
            KeyCode::Left if ev.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(InputEvent::BackWord)
            }
            KeyCode::Right if ev.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(InputEvent::ForwardWord)
            }
            KeyCode::Left => Some(InputEvent::Back),
            KeyCode::Right => Some(InputEvent::Forward),
            KeyCode::Char('b') if ev.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(InputEvent::Back)
            }
            KeyCode::Char('f') if ev.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(InputEvent::Forward)
            }
            KeyCode::Char('b') if ev.modifiers.contains(KeyModifiers::ALT) => {
                Some(InputEvent::BackWord)
            }
            KeyCode::Char('f') if ev.modifiers.contains(KeyModifiers::ALT) => {
                Some(InputEvent::ForwardWord)
            }
            KeyCode::Char('a') if ev.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(InputEvent::Start)
            }
            KeyCode::Char('e') if ev.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(InputEvent::End)
            }

            // Delete
            KeyCode::Char('h') if ev.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(InputEvent::PopLeft)
            }
            KeyCode::Char('d') if ev.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(InputEvent::PopRight)
            }
            KeyCode::Char('u') if ev.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(InputEvent::DeleteToStart)
            }
            KeyCode::Char('k') if ev.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(InputEvent::DeleteToEnd)
            }
            KeyCode::Char('w') if ev.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(InputEvent::PopWordLeft)
            }
            KeyCode::Backspace if ev.modifiers.contains(KeyModifiers::ALT) => {
                Some(InputEvent::PopWordLeft)
            }
            KeyCode::Char('d') if ev.modifiers.contains(KeyModifiers::ALT) => {
                Some(InputEvent::PopWordRight)
            }
            KeyCode::Backspace => Some(InputEvent::PopLeft),

            // Other
            KeyCode::Char(c) => Some(InputEvent::Push(c)),
            _ => None,
        }
    }
}
