use std::{
    io::Write,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Result;
use commands::query_tab::QueryCommand;
use crossbeam::channel::Sender;
use in_flight_ipc::{InFlightIpcCommand, IpcCommandError};
use ipc_stream::IpcStream;
use serde::{Deserialize, Serialize};
use strum::IntoDiscriminant;

use super::exit_code::ExitCode;
use crate::{
    AppEvent, WorkRequest,
    config::{
        Config,
        cli::{RemoteCmd, RemoteCmdDiscriminants},
    },
    shared::ipc::commands::{
        index_lrc::IndexLrcCommand, keybind::KeybindCommand, raise::RaiseCommand,
        set::SetIpcCommand, status_message::StatusMessageCommand, switch_tab::SwitchTabCommand,
        tmux::TmuxHookCommand,
    },
};

pub mod commands;
pub mod in_flight_ipc;
pub mod ipc_stream;

pub fn get_socket_path(pid: u32) -> PathBuf {
    let mut temp = std::env::temp_dir();
    temp.push(format!("s2u-{pid}.sock"));
    temp
}

/// Whether `filename` is an s2udio IPC socket name.
///
/// Sockets are pid-scoped and named `s2u-<pid>.sock`; the `rmpc-` prefix is
/// the legacy name and stays recognised so an upgrade does not strand a
/// running session. This is the same pairing the stale-socket sweeper uses
/// (`core/socket.rs`), and it must stay in sync with [`get_socket_path`] —
/// the listing used to match only `rmpc-`, so a pid-less remote command found
/// nothing at all and silently did nothing.
fn is_socket_name(filename: &str) -> bool {
    (filename.starts_with("s2u-") || filename.starts_with("rmpc-")) && filename.ends_with(".sock")
}

/// Every s2udio socket file in `dir`, sorted so fan-out order is stable.
fn socket_paths_in(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let filename = entry.file_name();
        if is_socket_name(&filename.to_string_lossy()) {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

pub fn list_all_socket_paths() -> Result<impl Iterator<Item = PathBuf>> {
    Ok(socket_paths_in(&std::env::temp_dir())?.into_iter())
}

/// Whether a TUI accepts a connection on any socket right now. A crashed TUI
/// leaves its `s2u-<pid>.sock` inode behind; a connect attempt is what
/// separates it from a live one.
pub(crate) fn any_live_tui_socket() -> bool {
    let Ok(paths) = list_all_socket_paths() else {
        return false;
    };
    any_live_tui_socket_in(paths.collect())
}

fn any_live_tui_socket_in(paths: Vec<PathBuf>) -> bool {
    paths.into_iter().any(|path| UnixStream::connect(path).is_ok())
}

/// Ask a live TUI to raise its own window. The TUI knows its kitty/QAT
/// session, so its raise ladder can show a hidden panel (`resize-os-window
/// --action=show`) where no external process can. `false` when no TUI
/// accepted the command — the caller then asks the daemon to bring the
/// player back.
pub(crate) fn raise_running_tui() -> bool {
    let Ok(paths) = list_all_socket_paths() else {
        return false;
    };
    raise_running_tui_in(paths.collect())
}

fn raise_running_tui_in(paths: Vec<PathBuf>) -> bool {
    for path in paths {
        let Ok(ipc) = (RemoteCmd::Raise { tab: None }).write_to_socket(&path) else {
            continue;
        };
        if ipc.read_response().is_ok() {
            return true;
        }
    }
    false
}

pub(crate) trait SocketCommandExecute {
    fn execute(
        self,
        event_tx: &Sender<AppEvent>,
        work_tx: &Sender<WorkRequest>,
        stream: IpcStream,
        config: &Config,
    ) -> Result<()>;
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum SocketCommand {
    IndexLrc(IndexLrcCommand),
    StatusMessage(StatusMessageCommand),
    TmuxHook(TmuxHookCommand),
    Set(Box<SetIpcCommand>),
    Keybind(KeybindCommand),
    SwitchTab(SwitchTabCommand),
    Raise(RaiseCommand),
    Query(QueryCommand),
}

impl SocketCommandExecute for SocketCommand {
    fn execute(
        self,
        event_tx: &Sender<AppEvent>,
        work_tx: &Sender<WorkRequest>,
        stream: IpcStream,
        config: &Config,
    ) -> Result<()> {
        match self {
            SocketCommand::IndexLrc(cmd) => cmd.execute(event_tx, work_tx, stream, config),
            SocketCommand::StatusMessage(cmd) => cmd.execute(event_tx, work_tx, stream, config),
            SocketCommand::TmuxHook(cmd) => cmd.execute(event_tx, work_tx, stream, config),
            SocketCommand::Set(cmd) => cmd.execute(event_tx, work_tx, stream, config),
            SocketCommand::Keybind(cmd) => cmd.execute(event_tx, work_tx, stream, config),
            SocketCommand::SwitchTab(cmd) => cmd.execute(event_tx, work_tx, stream, config),
            SocketCommand::Raise(cmd) => cmd.execute(event_tx, work_tx, stream, config),
            SocketCommand::Query(cmd) => cmd.execute(event_tx, work_tx, stream, config),
        }
    }
}

impl RemoteCmd {
    /// Whether this is the raise command (the one with a daemon fallback when
    /// no TUI socket is live).
    pub(crate) fn is_raise(&self) -> bool {
        matches!(self, Self::Raise { .. })
    }

    pub(crate) fn write_to_socket(
        self,
        path: &PathBuf,
    ) -> Result<InFlightIpcCommand, IpcCommandError> {
        let cmd = SocketCommand::try_from(&self).map_err(IpcCommandError::CommandCreate)?;
        let cmd = serde_json::to_string(&cmd)?;

        let mut stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;

        stream.write_all(cmd.as_bytes())?;
        stream.write_all(b"\n")?;

        return Ok(InFlightIpcCommand { stream });
    }

    pub(crate) fn handle(self, pid: Option<u32>) -> ExitCode {
        if pid.is_none() && [RemoteCmdDiscriminants::Query].contains(&self.discriminant()) {
            match list_all_socket_paths().map(|p| p.count()) {
                Ok(1) => {} // only one path found, we don't need PID
                Ok(0) => {
                    eprintln!("No socket paths found. Please start rmpc TUI instance first");
                    return ExitCode::from(1);
                }
                Ok(_) => {
                    eprintln!("Remote command '{self}' requires a PID to be specified",);
                    return ExitCode::from(1);
                }
                Err(err) => {
                    eprintln!("Failed to list socket paths: {err}");
                    return ExitCode::from(1);
                }
            }
        }

        if let Some(pid) = pid {
            let path = get_socket_path(pid);
            self.run(&path)
        } else {
            match list_all_socket_paths() {
                Ok(paths) => {
                    let mut exit_code = ExitCode::from(0);
                    for path in paths {
                        exit_code |= self.clone().run(&path);
                    }
                    exit_code
                }
                Err(err) => {
                    eprintln!("Failed to list socket paths: {err}");
                    ExitCode::from(1)
                }
            }
        }
    }

    fn run(self, path: &PathBuf) -> ExitCode {
        let ipc = match self.write_to_socket(path) {
            Ok(ipc) => ipc,
            Err(err) => {
                eprintln!("{err}");
                return ExitCode::from(1);
            }
        };
        match ipc.read_response() {
            Ok(Some(resp)) => println!("{resp}"),
            Ok(None) => {}
            Err(err) => {
                eprintln!("{err}");
                return ExitCode::from(1);
            }
        }

        ExitCode::from(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("s2u-ipc-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn socket_listing_finds_both_prefixes_and_ignores_other_files() {
        let dir = scratch_dir("listing");
        for name in [
            "s2u-11.sock",
            "rmpc-22.sock",
            "s2u-33.txt",
            "other.sock",
            "s2u-44.sock.bak",
        ] {
            fs::write(dir.join(name), b"").expect("write fixture");
        }

        let found: Vec<String> = socket_paths_in(&dir)
            .expect("list sockets")
            .iter()
            .map(|path| path.file_name().expect("file name").to_string_lossy().into_owned())
            .collect();

        // Sorted, and only the two socket names.
        assert_eq!(found, vec!["rmpc-22.sock", "s2u-11.sock"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_bound_socket_name_is_discoverable() {
        // The listing and the binder must agree on the name, or a pid-less
        // remote command runs over an empty list and silently does nothing
        // (the regression this fix removes).
        let path = get_socket_path(4321);
        let name = path.file_name().expect("socket name").to_string_lossy();
        assert!(is_socket_name(&name), "{name} is not a socket name");
    }

    #[test]
    fn a_stale_tui_socket_does_not_count_as_live() {
        let dir = scratch_dir("live");
        std::fs::write(dir.join("s2u-1.sock"), b"").expect("stale inode");
        assert!(
            !any_live_tui_socket_in(socket_paths_in(&dir).expect("list")),
            "a regular file must not count as a running TUI"
        );
        let listener = std::os::unix::net::UnixListener::bind(dir.join("s2u-2.sock")).expect("bind");
        assert!(
            any_live_tui_socket_in(socket_paths_in(&dir).expect("list")),
            "a bound socket is a live TUI"
        );
        drop(listener);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn raising_sends_the_json_raise_command_to_a_live_tui() {
        use std::io::{BufRead, BufReader, Write};
        let dir = scratch_dir("raise");
        let path = dir.join("s2u-2.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).expect("read");
            stream.write_all(b"ok\n").expect("reply");
            line
        });
        assert!(raise_running_tui_in(socket_paths_in(&dir).expect("list")));
        let line = handle.join().expect("thread");
        assert!(line.contains("\"Raise\""), "got {line}");
        let _ = fs::remove_dir_all(&dir);
    }
}
