use std::{
    io::{BufRead, BufReader},
    os::unix::net::UnixListener,
    path::PathBuf,
    sync::Arc,
};

use anyhow::{Context, Result};
use crossbeam::channel::Sender;

use crate::{
    AppEvent, WorkRequest,
    config::Config,
    shared::{
        ipc::{SocketCommand, SocketCommandExecute, get_socket_path},
        macros::try_cont,
    },
    try_skip,
};

/// Best-effort cleanup of stale per-session IPC sockets
/// (`s2u-<pid>.sock` / legacy `rmpc-<pid>.sock` whose pid is gone) left
/// behind by non-clean exits — kills, freezes and crashes never run
/// [`SocketGuard`]'s `Drop`, so /tmp accumulates one dead inode per
/// interrupted session. Each run binds its own pid-scoped name, so
/// removing dead siblings is always safe (the same hygiene
/// `write_mpv_m3u` applies to its private playlist dir).
fn sweep_stale_sockets() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(pid) = name
            .strip_prefix("s2u-")
            .or_else(|| name.strip_prefix("rmpc-"))
            .and_then(|rest| rest.strip_suffix(".sock"))
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        // Only dead pids are swept: a live /proc/<pid> (current session or
        // a pid-reused process) keeps its file untouched.
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

pub(crate) fn init(
    event_tx: Sender<AppEvent>,
    work_tx: Sender<WorkRequest>,
    config: Arc<Config>,
) -> Result<SocketGuard> {
    sweep_stale_sockets();
    let pid = std::process::id();
    let addr = get_socket_path(pid);
    let guard = SocketGuard(addr.clone());
    let listener = UnixListener::bind(&addr).context("Failed to bind to unix socket")?;

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = try_cont!(stream, "Failed to connect to socket client");
            let mut reader = BufReader::new(&stream);

            let mut buf = String::new();
            try_cont!(reader.read_line(&mut buf), "Failed to read from socket client");
            let command: SocketCommand =
                try_cont!(serde_json::from_str(&buf), "Failed to parse socket command");

            log::debug!(command:?, addr:?; "Got command from unix socket");
            try_skip!(
                command.execute(&event_tx, &work_tx, stream.into(), &config),
                "Socket command execution failed"
            );
        }
    });
    Ok(guard)
}

/// The guard handles deletion of the unix domain socket upon dropping
#[must_use]
pub struct SocketGuard(PathBuf);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        // Ignore, the app is exiting, theres nothing else we can do
        _ = std::fs::remove_file(&self.0);
    }
}
