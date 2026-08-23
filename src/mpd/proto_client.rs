use std::{
    io::{BufRead, Read},
    str::FromStr,
};
use anyhow::Result;
use super::{
    FromMpd, errors::{MpdError, MpdFailureResponse},
    split_line, version::Version,
};
use crate::{mpd::errors::ErrorCode, shared::string_util::StringExt};
type MpdResult<T> = Result<T, MpdError>;
#[derive(Debug, Default, PartialEq)]
pub struct BinaryMpdResponse {
    pub bytes_read: u64,
    pub size_total: u32,
    pub mime_type: Option<String>,
}
#[derive(Debug, PartialEq, Eq)]
pub enum MpdLine {
    Ok,
    Value(String),
}
pub trait SocketClient {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()>;
    fn read(&mut self) -> &mut impl BufRead;
    fn version(&self) -> Version;
    fn clear_read_buf(&mut self) -> Result<()>;
}
pub trait ProtoClient {
    fn should_reinit_buffer(err: &MpdError) -> bool {
        !matches!(
            err, MpdError::Mpd(MpdFailureResponse { code : ErrorCode::NoExist, .. }) |
            MpdError::TimedOut(_)
        )
    }
    fn reinit_buffer_if_needed(&mut self, err: &MpdError) -> Result<()>;
    fn execute(&mut self, command: &str) -> Result<(), MpdError>;
    fn read_ok(&mut self) -> Result<(), MpdError>;
    fn read_response<V>(&mut self) -> Result<V, MpdError>
    where
        V: FromMpd + Default;
    fn read_opt_response<V>(&mut self) -> Result<Option<V>, MpdError>
    where
        V: FromMpd + Default;
    fn read_bin(&mut self, command: &str) -> MpdResult<Option<Vec<u8>>>;
    fn read_bin_inner(
        &mut self,
        binary_buf: &mut Vec<u8>,
    ) -> Result<Option<BinaryMpdResponse>, MpdError>;
    fn read_line(read: &mut impl BufRead) -> Result<MpdLine, MpdError>;
}
impl<T: SocketClient> ProtoClient for T {
    fn reinit_buffer_if_needed(&mut self, err: &MpdError) -> Result<()> {
        if Self::should_reinit_buffer(err) {
            log::error!(err:?; "read buffer was reinitialized");
            self.clear_read_buf()?;
        }
        Ok(())
    }
    fn execute(&mut self, command: &str) -> Result<(), MpdError> {
        log::trace!(command; "Executing MPD command");
        Ok(self.write([command, "\n"].concat().as_bytes())?)
    }
    fn read_ok(&mut self) -> Result<(), MpdError> {
        let read = self.read();
        match Self::read_line(read) {
            Ok(MpdLine::Ok) => Ok(()),
            Ok(MpdLine::Value(val)) => {
                log::error!(
                    val = val.as_str();
                    "read buffer was reinitialized because we got a value when expecting ok"
                );
                self.clear_read_buf()?;
                Err(MpdError::Generic(format!("Expected 'OK' but got '{val}'")))
            }
            Err(e) => {
                self.reinit_buffer_if_needed(&e)?;
                Err(e)
            }
        }
    }
    fn read_response<V>(&mut self) -> Result<V, MpdError>
    where
        V: FromMpd + Default,
    {
        let mut result = V::default();
        let read = self.read();
        loop {
            match Self::read_line(read) {
                Ok(MpdLine::Ok) => return Ok(result),
                Ok(MpdLine::Value(val)) => {
                    if let Err(e) = result.next(val) {
                        self.reinit_buffer_if_needed(&e)?;
                        return Err(e);
                    }
                }
                Err(e) => {
                    self.reinit_buffer_if_needed(&e)?;
                    return Err(e);
                }
            }
        }
    }
    fn read_opt_response<V>(&mut self) -> Result<Option<V>, MpdError>
    where
        V: FromMpd + Default,
    {
        let mut result = V::default();
        let mut found_any = false;
        let read = self.read();
        loop {
            match Self::read_line(read) {
                Ok(MpdLine::Ok) => {
                    return if found_any { Ok(Some(result)) } else { Ok(None) };
                }
                Ok(MpdLine::Value(val)) => {
                    found_any = true;
                    if let Err(e) = result.next(val) {
                        self.reinit_buffer_if_needed(&e)?;
                        return Err(e);
                    }
                }
                Err(e) => {
                    self.reinit_buffer_if_needed(&e)?;
                    return Err(e);
                }
            }
        }
    }
    fn read_bin(&mut self, command: &str) -> MpdResult<Option<Vec<u8>>> {
        let mut buf = Vec::new();
        let _ = match self.read_bin_inner(&mut buf) {
            Ok(Some(v)) => Ok(Some(v)),
            Ok(None) => return Ok(None),
            Err(e) => {
                self.reinit_buffer_if_needed(&e)?;
                Err(e)
            }
        };
        loop {
            let command = command.trim_end_matches(" 0");
            let command = format!("{} {}", command, buf.len());
            self.execute(command.as_ref())?;
            match self.read_bin_inner(&mut buf) {
                Ok(Some(response)) => {
                    if buf.len() >= response.size_total as usize
                        || response.bytes_read == 0
                    {
                        log::trace!(len = buf.len(); "Finished reading binary response");
                        break;
                    }
                }
                Ok(None) => return Ok(None),
                Err(e) => {
                    self.reinit_buffer_if_needed(&e)?;
                    return Err(e);
                }
            }
        }
        Ok(Some(buf))
    }
    fn read_bin_inner(
        &mut self,
        binary_buf: &mut Vec<u8>,
    ) -> Result<Option<BinaryMpdResponse>, MpdError> {
        let mut result = BinaryMpdResponse::default();
        let read = self.read();
        {
            loop {
                match Self::read_line(read)? {
                    MpdLine::Ok => {
                        log::warn!("Expected binary data but got 'OK'");
                        return Ok(None);
                    }
                    MpdLine::Value(val) => {
                        let (key, value) = split_line(val)?;
                        match key.to_lowercase().as_ref() {
                            "size" => result.size_total = value.parse()?,
                            "type" => result.mime_type = Some(value),
                            "binary" => {
                                result.bytes_read = value.parse()?;
                                break;
                            }
                            key => {
                                return Err(
                                    MpdError::Generic(
                                        format!(
                                            "Unexpected key when parsing binary response: '{key}'"
                                        ),
                                    ),
                                );
                            }
                        }
                    }
                }
            }
        }
        let mut handle = read.take(result.bytes_read);
        let _ = handle.read_to_end(binary_buf)?;
        let _ = read.read_line(&mut String::new());
        match Self::read_line(read)? {
            MpdLine::Ok => Ok(Some(result)),
            MpdLine::Value(val) => {
                Err(MpdError::Generic(format!("Expected 'OK' but got '{val}'")))
            }
        }
    }
    fn read_line(read: &mut impl BufRead) -> Result<MpdLine, MpdError> {
        let mut buf = Vec::new();
        let bytes_read = match read.read_until(b'\n', &mut buf) {
            Ok(v) => Ok(v),
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                log::error!(err:? = e; "Got broken pipe from mpd");
                Err(MpdError::ClientClosed)
            }
            Err(
                e,
            ) if e.kind() == std::io::ErrorKind::TimedOut
                || e.kind() == std::io::ErrorKind::WouldBlock => {
                log::trace!(err:? = e; "Reading line from MPD timed out");
                Err(e.into())
            }
            Err(e) => {
                log::error!(
                    err:? = e;
                    "Encountered unexpected error when reading a response line from MPD"
                );
                Err(e.into())
            }
        }?;
        let mut line = String::from_utf8_lossy_as_owned(buf);
        if bytes_read == 0 {
            log::error!("Got an empty line in MPD's response");
            return Err(
                MpdError::ValueExpected(
                    "Expected value when reading MPD's response but the stream reached EOF"
                        .to_string(),
                ),
            );
        }
        if line.starts_with("OK") || line.starts_with("list_OK") {
            log::trace!(line = line.as_str().trim(); "Read MPD line OK");
            return Ok(MpdLine::Ok);
        }
        if line.starts_with("ACK") {
            log::error!(line = line.as_str().trim(); "Read MPD line with error");
            return Err(MpdError::Mpd(MpdFailureResponse::from_str(&line)?));
        }
        line.pop();
        log::trace!(line = line.as_str().trim(); "Read MPD line");
        Ok(MpdLine::Value(line))
    }
}
