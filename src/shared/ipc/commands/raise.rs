use anyhow::Result;
use crossbeam::channel::Sender;
use serde::{Deserialize, Serialize};

use crate::{
    AppEvent, WorkRequest,
    config::Config,
    shared::ipc::{IpcStream, SocketCommandExecute},
};

/// `s2udio remote raise`: bring the running TUI's terminal window forward.
///
/// The optional tab is handled by the event loop (the config handed to
/// `execute` can be stale after a hot reload), next to the existing
/// `RemoteSwitchTab` arm.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct RaiseCommand {
    #[serde(default)]
    pub(crate) tab: Option<String>,
}

impl SocketCommandExecute for RaiseCommand {
    fn execute(
        self,
        event_tx: &Sender<AppEvent>,
        _work_tx: &Sender<WorkRequest>,
        stream: IpcStream,
        _config: &Config,
    ) -> Result<()> {
        event_tx
            .send(AppEvent::RemoteRaise { tab: self.tab, stream })
            .map_err(|err| anyhow::anyhow!("Failed to send raise event: {err}"))?;
        Ok(())
    }
}
