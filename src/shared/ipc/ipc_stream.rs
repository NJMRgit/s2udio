use std::{
    collections::HashMap, io::{Read, Write},
    os::unix::net::UnixStream, time::Duration,
};
pub const IPC_RESPONSE_SUCCESS: &str = "ok";
pub const IPC_RESPONSE_ERROR: &str = "error";
/// Wrapper around a [`UnixStream`] that handles IPC communication on the
/// "server" side. Automatically writes a well formed IPC response when dropped.
#[derive(Debug)]
pub(crate) struct IpcStream {
    inner: UnixStream,
    response: HashMap<String, serde_json::Value>,
    error: Option<String>,
}
impl IpcStream {
    /// Consumes the stream as an error, meaning a [`IPC_RESPONSE_ERROR`]
    /// followed by an error messarge will be sent. If no error is reported, a
    /// [`Self::response`] followed by [`IPC_RESPONSE_SUCCESS`] will be sent
    /// instead.
    pub fn error(mut self, error: String) {
        self.error = Some(error);
    }
    pub fn insert_response(
        &mut self,
        key: impl Into<String>,
        response: impl Into<serde_json::Value>,
    ) {
        self.response.insert(key.into(), response.into());
    }
}
impl From<UnixStream> for IpcStream {
    fn from(stream: UnixStream) -> Self {
        IpcStream {
            inner: stream,
            response: HashMap::new(),
            error: None,
        }
    }
}
impl Write for IpcStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
impl Read for IpcStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}
impl Drop for IpcStream {
    fn drop(&mut self) {
        if let Err(err) = self.inner.set_write_timeout(Some(Duration::from_secs(1))) {
            log::error!(err:?; "Failed to set write timeout on IPC stream");
            return;
        }
        if let Some(err) = &self.error {
            if let Err(err) = self.inner.write_all(b"error: ") {
                log::error!(
                    err:?; "Failed to write error response start to IPC stream on drop"
                );
                return;
            }
            if let Err(err) = self.inner.write_all(err.as_bytes()) {
                log::error!(
                    err:?; "Failed to write error response to IPC stream on drop"
                );
                return;
            }
        } else {
            if !self.response.is_empty() {
                match serde_json::to_string(&self.response) {
                    Ok(serialized) => {
                        if let Err(err) = self.inner.write_all(serialized.as_bytes()) {
                            log::error!(
                                err:?; "Failed to write response to IPC stream on drop"
                            );
                            return;
                        }
                        if let Err(err) = self.inner.write_all(b"\n") {
                            log::error!(
                                err:?; "Failed to write newline to IPC stream on drop"
                            );
                            return;
                        }
                    }
                    Err(err) => {
                        log::error!(
                            err:?;
                            "Failed to serialize response to IPC stream. This should not ever happen, please report this."
                        );
                        if let Err(err) = self.inner.write_all(b"error: ") {
                            log::error!(
                                err:?;
                                "Failed to write error response start to IPC stream on drop"
                            );
                            return;
                        }
                        if let Err(err) = self
                            .inner
                            .write_all(err.to_string().as_bytes())
                        {
                            log::error!(
                                err:?;
                                "Failed to write error response to IPC stream on drop"
                            );
                            return;
                        }
                    }
                }
            }
            if let Err(err) = self.inner.write_all(IPC_RESPONSE_SUCCESS.as_bytes()) {
                log::error!(
                    err:?; "Failed to write response finisher to IPC stream on drop"
                );
                return;
            }
        }
        if let Err(err) = self.inner.write_all(b"\n") {
            log::error!(err:?; "Failed to write newline to IPC stream on drop");
            return;
        }
    }
}
