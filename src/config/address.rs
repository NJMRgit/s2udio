use super::utils::tilde_expand;
use crate::shared::env::ENV;
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum MpdAddress {
    IpAndPort(String),
    SocketPath(String),
    AbstractSocket(String),
}
#[derive(Default, Clone, Eq, PartialEq)]
pub struct MpdPassword(pub String);
impl std::fmt::Debug for MpdPassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "*****")
    }
}
impl From<&str> for MpdPassword {
    fn from(s: &str) -> Self {
        s.to_owned().into()
    }
}
impl From<String> for MpdPassword {
    fn from(s: String) -> Self {
        Self(s)
    }
}
impl Default for MpdAddress {
    fn default() -> Self {
        Self::IpAndPort("127.0.0.1:6600".to_string())
    }
}
impl MpdAddress {
    pub fn resolve(
        addr_from_cli: Option<String>,
        pw_from_cli: Option<String>,
        addr_from_config: String,
        pw_from_config: Option<String>,
    ) -> (MpdAddress, Option<MpdPassword>) {
        let (cli_addr, cli_pw) = Self::resolve_cli(addr_from_cli, pw_from_cli);
        let (cfg_addr, cfg_pw) = Self::resolve_config(addr_from_config, pw_from_config);
        let env = Self::resolve_env();
        if let Some(cli_addr) = cli_addr {
            return (cli_addr, cli_pw);
        }
        if let Some(env) = env {
            return env;
        }
        (cfg_addr, cfg_pw)
    }
    fn resolve_config(
        addr: String,
        pw: Option<String>,
    ) -> (MpdAddress, Option<MpdPassword>) {
        let expanded = tilde_expand(&addr);
        let addr = if expanded.starts_with('/') {
            MpdAddress::SocketPath(expanded.into_owned())
        } else if let Some(path) = expanded.strip_prefix('@') {
            MpdAddress::AbstractSocket(path.to_owned())
        } else {
            MpdAddress::IpAndPort(addr)
        };
        let pw: Option<MpdPassword> = pw.map(|pw| pw.into());
        (addr, pw)
    }
    fn resolve_cli(
        addr_from_cli: Option<String>,
        pw_from_cli: Option<String>,
    ) -> (Option<MpdAddress>, Option<MpdPassword>) {
        let addr = addr_from_cli
            .map(|addr| {
                let expanded = tilde_expand(&addr);
                if expanded.starts_with('/') {
                    MpdAddress::SocketPath(expanded.into_owned())
                } else if let Some(path) = expanded.strip_prefix('@') {
                    MpdAddress::AbstractSocket(path.to_owned())
                } else {
                    MpdAddress::IpAndPort(addr)
                }
            });
        let pw: Option<MpdPassword> = pw_from_cli.map(|pw| pw.into());
        (addr, pw)
    }
    fn resolve_env() -> Option<(MpdAddress, Option<MpdPassword>)> {
        let mpd_host = ENV.var_os("MPD_HOST");
        let mpd_host = mpd_host.as_ref().and_then(|v| v.to_str());
        let mpd_port = ENV.var_os("MPD_PORT");
        let mpd_port = mpd_port.as_ref().and_then(|v| v.to_str());
        if let Some(host) = mpd_host {
            if !host.starts_with('@')
                && let Some((password, host)) = host.split_once('@')
            {
                let expanded = tilde_expand(host);
                if expanded.starts_with('/') {
                    Some((
                        MpdAddress::SocketPath(expanded.into_owned()),
                        Some(password.to_string().into()),
                    ))
                } else if let Some(path) = expanded.strip_prefix('@') {
                    Some((
                        MpdAddress::AbstractSocket(path.to_owned()),
                        Some(password.to_string().into()),
                    ))
                } else if let Some(port) = mpd_port {
                    Some((
                        MpdAddress::IpAndPort(format!("{host}:{port}")),
                        Some(password.to_string().into()),
                    ))
                } else {
                    Some((
                        MpdAddress::IpAndPort(format!("{host}:6600")),
                        Some(password.to_string().into()),
                    ))
                }
            } else {
                let expanded = tilde_expand(host);
                if expanded.starts_with('/') {
                    Some((MpdAddress::SocketPath(expanded.into_owned()), None))
                } else if let Some(path) = expanded.strip_prefix('@') {
                    Some((MpdAddress::AbstractSocket(path.to_owned()), None))
                } else if let Some(port) = mpd_port {
                    Some((MpdAddress::IpAndPort(format!("{host}:{port}")), None))
                } else {
                    Some((MpdAddress::IpAndPort(format!("{host}:6600")), None))
                }
            }
        } else {
            return None;
        }
    }
}
