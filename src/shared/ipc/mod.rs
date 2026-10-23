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
}
