use std::{
    ffi::{OsStr, OsString},
    sync::LazyLock,
};

pub struct Env;

pub static ENV: LazyLock<Env> = LazyLock::new(|| Env);

impl Env {
    pub fn var<K: AsRef<OsStr>>(&self, key: K) -> Result<String, std::env::VarError> {
        std::env::var(key)
    }

    pub fn var_os<K: AsRef<OsStr>>(&self, key: K) -> Option<OsString> {
        std::env::var_os(key)
    }
}
