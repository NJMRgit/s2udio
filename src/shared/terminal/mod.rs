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
mod emulator;
mod features;
mod tty;
pub use emulator::Emulator;
pub use features::ImageBackend;
pub use tty::{TtyReader, TtyWriter};
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
