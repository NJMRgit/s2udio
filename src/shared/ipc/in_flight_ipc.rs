use std::{
    io::{BufRead, BufReader},
    os::unix::net::UnixStream,
};
use thiserror::Error;
use super::ipc_stream::{IPC_RESPONSE_ERROR, IPC_RESPONSE_SUCCESS};
use crate::shared::string_util::StringExt;
#[derive(Debug, Error)]
pub(crate) enum IpcCommandError {
    #[error("error: failed to serialize command, {0}")]
    CommandSerialization(#[from] serde_json::Error),
    #[error("error: failed to create command, {0}")]
    CommandCreate(anyhow::Error),
    #[error("error: socket error, {0}")]
    SocketError(#[from] std::io::Error),
    #[error("error: command failed, {0}")]
    CommandFailure(String),
}
pub(crate) struct InFlightIpcCommand {
    pub stream: UnixStream,
}
impl InFlightIpcCommand {
    pub(crate) fn read_response(self) -> Result<Option<String>, IpcCommandError> {
        let mut read = BufReader::new(&self.stream);
        let mut buf = String::new();
        read.read_line(&mut buf)?;
        if buf.trim().starts_with(IPC_RESPONSE_ERROR) {
            buf.drain(..IPC_RESPONSE_ERROR.len() + ": ".len());
            buf.trim_end_in_place();
            return Err(IpcCommandError::CommandFailure(buf));
        }
        if buf.trim() == IPC_RESPONSE_SUCCESS {
            return Ok(None);
        }
        let mut line_buf = String::new();
        loop {
            line_buf.clear();
            let bytes = read.read_line(&mut line_buf)?;
            if bytes == 0 {
                return Err(
                    IpcCommandError::SocketError(
                        std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            format!(
                                "Unexpected end of IPC response, got '{}' so far", buf
                                .trim()
                            ),
                        ),
                    ),
                );
            }
            if line_buf.trim() == IPC_RESPONSE_SUCCESS {
                break;
            }
            buf.push_str(&line_buf);
        }
        buf.trim_end_in_place();
        Ok(Some(buf))
    }
}
