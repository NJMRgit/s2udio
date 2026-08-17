use std::time::Duration;

use crossbeam::channel::Sender;
use crossterm::event::Event;

use crate::shared::{
    events::AppEvent,
    mouse_event::{MouseEvent, MouseEventKind, MouseEventTracker},
};

pub fn init(event_tx: Sender<AppEvent>) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("input".to_owned()).spawn(move || input_poll_task(&event_tx))
}

/// Read the system clipboard (or primary selection for middle-click) and
/// feed it through the paste pipeline. Runs on a background thread so a
/// slow clipboard owner never blocks the UI; the clipboard tool is wrapped
/// in a timeout for the same reason.
fn read_clipboard_and_paste(event_tx: Sender<AppEvent>, primary: bool) {
    std::thread::Builder::new()
        .name("clipboard".to_owned())
        .spawn(move || {
            let text = read_clipboard(primary);
            if let Some(text) = text
                && !text.trim().is_empty()
            {
                let _ = event_tx.send(AppEvent::UserPaste(text));
            }
        })
        .ok();
}

/// Fetch the clipboard contents: Wayland (`wl-paste`), then X11
/// (`xclip` / `xsel`) as fallbacks. `primary` reads the primary selection
/// (middle-click); otherwise the clipboard proper (Ctrl+V).
fn read_clipboard(primary: bool) -> Option<String> {
    use std::process::{Command, Stdio};
    let run = |program: &str, args: &[&str]| {
        // `timeout` keeps a stalled clipboard owner from hanging the
        // helper forever (a dangling app holding the Wayland clipboard
        // never answers).
        Command::new("timeout")
            .args(["2", program])
            .args(args)
            .stdin(Stdio::null())
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let text = if primary {
        run("wl-paste", &["--primary", "--no-newline"])
    } else {
        run("wl-paste", &["--no-newline"])
    }
    .or_else(|| run("xclip", &["-selection", if primary { "primary" } else { "clipboard" }, "-o"]))
    .or_else(|| run("xsel", &[if primary { "--primary" } else { "--clipboard" }, "--output"]))?;
    if text.trim().is_empty() { None } else { Some(text) }
}

fn input_poll_task(event_tx: &Sender<AppEvent>) {
    // Sometimes in there are inputs left in the buffer(because of tmux maybe?)
    // before starting to read inputs (from reading terminal sequences), this
    // results in random stuff happening in the program. Simply drain them.
    drain_crossterm_events();

    let mut mouse_event_tracker = MouseEventTracker::default();
    loop {
        match crossterm::event::poll(Duration::from_millis(250)) {
            Ok(true) => match crossterm::event::read() {
                Ok(Event::Mouse(mouse)) => {
                    // Middle-click pastes the primary selection: with mouse
                    // capture enabled the terminal no longer does its own
                    // middle-click paste, so read the clipboard and feed the
                    // same paste pipeline (the event is consumed here).
                    if matches!(
                        mouse.kind,
                        crossterm::event::MouseEventKind::Down(
                            crossterm::event::MouseButton::Middle
                        )
                    ) {
                        read_clipboard_and_paste(event_tx.clone(), true);
                        continue;
                    }
                    if let Some(ev) = mouse_event_tracker.track_and_get(mouse)
                        && let Err(err) = event_tx.send(AppEvent::UserMouseInput(ev))
                    {
                        log::error!(error:? = err; "Failed to send user mouse input");
                    }
                }
                Ok(Event::Key(key)) => {
                    // Ctrl+V pastes the clipboard (kitty does not remap it
                    // by default, and with raw mode the terminal sends the
                    // bare control code): read the clipboard and feed the
                    // paste pipeline.
                    let ctrl = key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL);
                    let is_ctrl_v =
                        ctrl && matches!(key.code, crossterm::event::KeyCode::Char('v' | '\x16'));
                    if is_ctrl_v {
                        read_clipboard_and_paste(event_tx.clone(), false);
                        continue;
                    }
                    if let Err(err) = event_tx.send(AppEvent::UserKeyInput(key)) {
                        log::error!(error:? = err; "Failed to send user input");
                    }
                }
                Ok(Event::Resize(columns, rows)) => {
                    if let Err(err) = event_tx.send(AppEvent::Resized { columns, rows }) {
                        log::error!(error:? = err; "Failed to render request after resize");
                    }
                }
                // Bracketed paste: terminal drag&dropped files/URLs (and
                // Ctrl+Shift+V-style terminal pastes) arrive here. Handed to
                // the UI, which offers the play/enqueue popup when the
                // content looks like audio/video. Middle-click and Ctrl+V are
                // handled above (mouse capture swallows the terminal's own
                // middle-click paste, and kitty sends Ctrl+V as a literal
                // control code).
                Ok(Event::Paste(text)) => {
                    if let Err(err) = event_tx.send(AppEvent::UserPaste(text)) {
                        log::error!(error:? = err; "Failed to send user paste");
                    }
                }
                Ok(Event::FocusLost) => {
                    // The window lost focus: the pointer can be anywhere now,
                    // so clear the hover position (the 65535 leave convention
                    // covers most terminals; this is the rest).
                    let ev = MouseEvent {
                        x: u16::MAX,
                        y: u16::MAX,
                        kind: MouseEventKind::Moved,
                        modifiers: crossterm::event::KeyModifiers::NONE,
                    };
                    if let Err(err) = event_tx.send(AppEvent::UserMouseInput(ev)) {
                        log::error!(error:? = err; "Failed to send focus-lost mouse input");
                    }
                }
                // Focus regain needs no action (the next mouse move
                // refreshes the hover position).
                Ok(Event::FocusGained) => {}
                Err(err) => {
                    log::warn!(error:? = err; "Failed to read input event");
                }
            },
            Ok(_) => {}
            Err(e) => log::warn!(error:? = e; "Error when polling for event"),
        }
    }
}

fn drain_crossterm_events() {
    while crossterm::event::poll(Duration::from_millis(0)).unwrap_or(false) {
        let _ = crossterm::event::read();
    }
}
