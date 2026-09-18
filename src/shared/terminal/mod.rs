use std::sync::LazyLock;
use anyhow::Result;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture,
        EnableBracketedPaste, EnableFocusChange, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    },
};
use crate::{
    config::album_art::{ImageMethod, ImageMethodFile},
    shared::{
        env::ENV, terminal::{crossterm_backend::CrosstermLockingBackend, tty::Tty},
        tmux::IS_TMUX,
    },
};
mod crossterm_backend;
/// Kitty drag & drop (OSC 72) — round 90, second attempt. The OSC strings
/// ride on the patched crossterm parser (`Event::Osc`); see
/// `vendor/crossterm/S2UDIO-PATCH.md`.
pub mod dnd;
mod emulator;
mod features;
mod tty;
pub use emulator::Emulator;
pub use features::ImageBackend;
pub use tty::{TtyReader, TtyWriter};
/// The escape character, spelled out so the sequences this module builds are
/// readable (round 90).
const ESCAPE: char = '\u{1b}';

/// Build a complete escape code from its body (round 90): the leading `ESC`,
/// the body, and the `ESC \` string terminator.
///
/// The terminator is **mandatory**: an unterminated OSC makes the terminal
/// treat every following byte as part of the string and swallow the whole UI
/// until it happens to find one — the round-90 blocker that left a real kitty
/// showing nothing but the album art (the graphics transmissions carry their
/// own terminator, which is exactly why the art survived).
pub(crate) fn escape_code(body: &str) -> String {
    format!("{ESCAPE}{body}{ESCAPE}\\")
}
pub struct Terminal {
    tty: Tty,
    emulator: Emulator,
    kitty_keyboard_protocol: bool,
    kitty_graphics: LazyLock<bool>,
    sixel: LazyLock<bool>,
    ueberzug_x11: LazyLock<bool>,
    ueberzug_wayland: LazyLock<bool>,
    zellij: bool,
}
pub static TERMINAL: LazyLock<Terminal> = LazyLock::new(Terminal::init);
#[allow(dead_code)]
impl Terminal {
    pub fn init() -> Self {
        let zellij = ENV.var("ZELLIJ").is_ok_and(|v| !v.is_empty());
        let kitty_keyboard_protocol = features::detect_kitty_keyboard()
            .inspect_err(|err| {
                log::error!(err:?; "Failed to determine kitty keyboard protocol support")
            })
            .unwrap_or_default();
        let emulator = Emulator::detect()
            .inspect_err(|err| log::error!(err:?; "Failed to detect terminal emulator"))
            .unwrap_or_default();
        let sixel: LazyLock<_> = LazyLock::new(|| {
            features::detect_sixel()
                .inspect_err(|err| {
                    log::error!(err:?; "Failed to determine sixel support")
                })
                .unwrap_or_default()
        });
        let kitty_graphics: LazyLock<_> = LazyLock::new(|| {
            features::detect_kitty_graphics()
                .inspect_err(|err| {
                    log::error!(err:?; "Failed to determine kitty graphics support")
                })
                .unwrap_or_default()
        });
        let ueberzug_x11: LazyLock<bool> = LazyLock::new(features::detect_ueberzug_x11);
        let ueberzug_wayland: LazyLock<bool> = LazyLock::new(
            features::detect_ueberzug_wayland,
        );
        Terminal {
            tty: Tty::new(),
            emulator,
            kitty_keyboard_protocol,
            kitty_graphics,
            sixel,
            ueberzug_x11,
            ueberzug_wayland,
            zellij,
        }
    }
    pub fn reader(&self) -> TtyReader {
        self.tty.reader()
    }
    pub fn writer(&self) -> TtyWriter {
        self.tty.writer()
    }
    pub fn emulator(&self) -> Emulator {
        self.emulator
    }
    pub fn ueberzug_x11(&self) -> bool {
        *self.ueberzug_x11
    }
    pub fn ueberzug_wayland(&self) -> bool {
        *self.ueberzug_wayland
    }
    pub fn keyboard_protocol_kitty(&self) -> bool {
        self.kitty_keyboard_protocol
    }
    /// Whether the drag & drop protocol (OSC 72, kitty >= 0.47) can be used
    /// here (round 90): only kitty implements it, so no other emulator ever
    /// receives the announce escape code.
    ///
    /// The XTVERSION probe identifies `kitty`; a tmux client's response can
    /// come back empty on some setups, so the same environment marker the
    /// image backends rely on is accepted as a fallback.
    pub fn kitty_dnd_supported(&self) -> bool {
        // A terminal multiplexer is never a drop target: it cannot forward a
        // terminal-to-client escape code, and it inherits the environment of
        // the kitty it runs in (`KITTY_WINDOW_ID`), so the fallback below
        // must not fire there (round 90).
        if *IS_TMUX || self.zellij {
            return false;
        }
        let supported = self.emulator == Emulator::Kitty
            || ENV.var("KITTY_WINDOW_ID").is_ok_and(|value| !value.is_empty())
            // Test hook: lets the pty harness drive the whole client side
            // without a real kitty (round 90). Never set in normal use; the
            // announce is the only thing it changes.
            || ENV.var("S2UDIO_DND_FORCE").is_ok_and(|value| value == "1");
        log::debug!(emulator:? = self.emulator, supported; "Kitty drag & drop usable");
        supported
    }
    /// Write a complete escape code (the body only: `]72;t=a;text/uri-list`)
    /// to the terminal, terminated and flushed immediately — a drop answer
    /// that sits in a buffer is an answer the terminal never sees (round 90).
    pub fn write_escape(&self, sequence: &str) -> std::io::Result<()> {
        use std::io::Write as _;
        let mut writer = self.writer();
        write!(writer, "{}", escape_code(sequence))?;
        writer.flush()
    }
    pub fn zellij(&self) -> bool {
        self.zellij
    }
    pub fn resolve_image_backend(
        &self,
        requested_backend: ImageMethodFile,
    ) -> ImageMethod {
        let result = match requested_backend {
            ImageMethodFile::UeberzugWayland if self.ueberzug_wayland() => {
                ImageMethod::UeberzugWayland
            }
            ImageMethodFile::UeberzugWayland => {
                log::warn!(
                    "UeberzugWayland requested but not supported, falling back to Block"
                );
                ImageMethod::Block
            }
            ImageMethodFile::UeberzugX11 if self.ueberzug_x11() => {
                ImageMethod::UeberzugX11
            }
            ImageMethodFile::UeberzugX11 => {
                log::warn!(
                    "UeberzugX11 requested but not supported, falling back to Block"
                );
                ImageMethod::Block
            }
            ImageMethodFile::Iterm2 => ImageMethod::Iterm2,
            ImageMethodFile::Kitty if self.kitty_graphics_supported() => {
                ImageMethod::Kitty
            }
            ImageMethodFile::Kitty => {
                log::warn!(
                    "Kitty requested but the kitty graphics protocol is not usable here (emulator {:?}, \
                     probe {}), falling back to Block",
                    self.emulator, * self.kitty_graphics
                );
                ImageMethod::Block
            }
            ImageMethodFile::Sixel => ImageMethod::Sixel,
            ImageMethodFile::Block => ImageMethod::Block,
            ImageMethodFile::None => ImageMethod::None,
            ImageMethodFile::Auto if self.zellij => {
                log::debug!(
                    requested_backend:?; "Zellij detected, disabling image backend"
                );
                ImageMethod::None
            }
            ImageMethodFile::Auto => self.autodetect_image_backend().into(),
        };
        log::debug!(
            requested_backend:?, resolved_backend:? = result, tmux = * IS_TMUX;
            "Resolved image backend"
        );
        result
    }
    pub fn autodetect_image_backend(&self) -> ImageBackend {
        use ImageBackend as B;
        let mut all_backends = vec![B::Kitty, B::Iterm2, B::Sixel];
        match self.emulator {
            Emulator::Konsole => all_backends.clear(),
            Emulator::WezTerm => {
                all_backends.retain(|b| matches!(b, B::Iterm2 | B::Sixel))
            }
            Emulator::VSCode => all_backends.retain(|b| matches!(b, B::Iterm2)),
            Emulator::Tabby => all_backends.retain(|b| matches!(b, B::Iterm2)),
            Emulator::Iterm2 => all_backends.retain(|b| matches!(b, B::Iterm2)),
            _ => all_backends.retain(|b| !matches!(b, B::Iterm2)),
        }
        if !matches!(self.emulator, Emulator::Konsole) {
            all_backends.push(B::UeberzugWayland);
            all_backends.push(B::UeberzugX11);
        }
        for backend in all_backends {
            if self.is_backend_supported(backend) {
                return backend;
            }
        }
        return ImageBackend::Block;
    }
    fn is_backend_supported(&self, backend: ImageBackend) -> bool {
        match backend {
            ImageBackend::Kitty => self.kitty_graphics_supported(),
            ImageBackend::Iterm2 => true,
            ImageBackend::Sixel => *self.sixel,
            ImageBackend::UeberzugWayland => *self.ueberzug_wayland,
            ImageBackend::UeberzugX11 => *self.ueberzug_x11,
            ImageBackend::Block => true,
        }
    }
    /// Whether the kitty graphics protocol can be relied on to render
    /// rmpc's images on the attached terminal.
    ///
    /// The protocol query alone is not trustworthy: Konsole answers it
    /// with `OK` but its implementation cannot render rmpc's images (the
    /// unicode placeholders support is missing), and the emulator probe
    /// can miss (empty XTVERSION response) when a tmux client attaches
    /// mid-startup, leaving the emulator Unknown while the query still
    /// gets answered by the partial implementation. The identifiable
    /// kitty-capable emulators all report their name via XTVERSION, so
    /// an unidentified emulator that answers the query is treated as
    /// unsupported too — the app falls back to Block instead of painting
    /// placeholder garbage ("white lines").
    pub fn kitty_graphics_supported(&self) -> bool {
        let supported = kitty_supported(self.emulator, *self.kitty_graphics);
        log::debug!(
            emulator:? = self.emulator, probe:? = * self.kitty_graphics, supported;
            "Kitty graphics usable"
        );
        supported
    }
    pub fn try_restore(enable_mouse: bool) -> std::io::Result<()> {
        // Round 90: stop accepting drops first, so a drop that lands during
        // the shutdown is never half-answered. Only written where the
        // protocol was announced.
        if TERMINAL.kitty_dnd_supported() {
            let result = TERMINAL.write_escape(&format!("]{};t=A", dnd::DND_CODE));
            log::debug!(result:?; "Stopped accepting drag & drop");
        }
        let mut writer = TERMINAL.writer();
        if enable_mouse {
            execute!(writer, DisableMouseCapture)?;
        }
        execute!(writer, DisableFocusChange)?;
        if TERMINAL.kitty_keyboard_protocol {
            execute!(writer, PopKeyboardEnhancementFlags)?;
        }
        execute!(writer, DisableBracketedPaste)?;
        disable_raw_mode()?;
        execute!(writer, LeaveAlternateScreen)?;
        Ok(())
    }
    pub fn restore(enable_mouse: bool) {
        if let Err(err) = Self::try_restore(enable_mouse) {
            eprintln!("Failed to restore terminal state after panic: {err}");
        }
    }
    pub fn setup(
        enable_mouse: bool,
    ) -> Result<ratatui::Terminal<CrosstermLockingBackend>> {
        let original_hook = std::panic::take_hook();
        std::panic::set_hook(
            Box::new(move |info| {
                Self::restore(enable_mouse);
                original_hook(info);
            }),
        );
        enable_raw_mode()?;
        // Round 90: announce that drops are accepted (kitty >= 0.47 only —
        // no other terminal ever receives this). Written first, because this
        // is the one thing the shutdown path and the panic hook must undo.
        if TERMINAL.kitty_dnd_supported() {
            TERMINAL.write_escape(&format!(
                "]{};t=a;{}",
                dnd::DND_CODE,
                dnd::ACCEPTED_MIMES.join(" ")
            ))?;
            log::debug!(mimes:? = dnd::ACCEPTED_MIMES; "Announced that drops are accepted");
        }
        let mut writer = TERMINAL.writer();
        execute!(writer, EnterAlternateScreen)?;
        execute!(writer, EnableBracketedPaste)?;
        if enable_mouse {
            execute!(writer, EnableMouseCapture)?;
            execute!(writer, EnableFocusChange)?;
        }
        if TERMINAL.kitty_keyboard_protocol {
            execute!(
                writer,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS |
                KeyboardEnhancementFlags::REPORT_EVENT_TYPES,)
            )?;
        }
        let mut terminal = ratatui::Terminal::new(CrosstermLockingBackend::new(writer))?;
        terminal.clear()?;
        Ok(terminal)
    }
}
/// The kitty-graphics decision: the probe alone is not enough — see
/// [`Terminal::kitty_graphics_supported`] for why Konsole and Unknown
/// emulators are excluded.
fn kitty_supported(emulator: Emulator, probe: bool) -> bool {
    match emulator {
        Emulator::Konsole | Emulator::Unknown => false,
        _ => probe,
    }
}
