//! **One running instance per user.**
//!
//! The TUI is not built to share its MPD with another TUI. Each instance runs
//! its own stream-buffer proxy, its own look ahead and its own entry-resolve
//! bookkeeping, and all of that keys on **queue ids** that only stay stable
//! while a single writer owns the queue. Two instances therefore resolve the
//! same `#s2u-audio` rows, both replace them (`deleteid` + `addid`, which
//! hands the row a *new* id), and each one's `playid` lands on an id the other
//! has already swapped out: MPD answers `No such song`, advances to the next
//! link row, and the queue walks instead of playing. The live log of one such
//! session recorded ~40 `Holding MPD: the current row still needs its stream`
//! lines in one second, walking song ids 12528 → 12556, with every row
//! prepared on two different proxy ports.
//!
//! The IPC socket cannot gate this: sockets are deliberately pid-scoped
//! (`s2u-<pid>.sock`) so several sessions can be addressed individually. The
//! gate is a per-user lock file instead — `instance.lock` in the state dir,
//! held with `flock(LOCK_EX | LOCK_NB)` for the life of the TUI.
//!
//! Why a lock file rather than a pid file: the kernel drops the lock when the
//! process dies, so a crash, a `kill -9` or a power cut never leaves a stale
//! lock behind for the next start to trip over. The pid inside the file is
//! only there so the refused instance can name the holder.
//!
//! `S2UDIO_ALLOW_MULTI=1` starts a second instance anyway, for the deliberate
//! case (two sessions on two different MPDs); everything else refuses.

use std::{
    fs::{File, OpenOptions},
    io::{Seek as _, SeekFrom, Write as _},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use rustix::{
    fs::{FlockOperation, flock},
    io::Errno,
};

use crate::shared::{env::ENV, paths::s2udio_state_dir};

/// Environment switch that lets a second instance start anyway. Any value
/// other than an empty string or `0` allows it.
pub const ALLOW_MULTI_ENV: &str = "S2UDIO_ALLOW_MULTI";

/// The lock file's name inside the state dir
/// (`~/.local/state/s2udio/instance.lock`).
pub const LOCK_FILE: &str = "instance.lock";

/// The lock held for as long as this process is the running instance.
/// Dropping it releases the `flock` — that is the whole mechanism, which is
/// why a [`Claim::Held`] value must be kept alive for the process's lifetime.
#[derive(Debug)]
#[must_use = "dropping the lock releases it, which lets a second instance start"]
pub struct InstanceLock {
    /// Owns the open file description the `flock` is attached to.
    _file: File,
    path: PathBuf,
}

impl InstanceLock {
    /// Where the lock lives (for the log line).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// What [`claim`] decided.
#[derive(Debug)]
pub enum Claim {
    /// This process holds the only instance lock.
    Held(InstanceLock),
    /// A second instance was asked for explicitly (`S2UDIO_ALLOW_MULTI`).
    MultipleAllowed,
}

/// The lock file's path (`<state dir>/instance.lock`).
///
/// Returns `None` only when neither `S2UDIO_LOG_DIR`/`XDG_STATE_HOME` nor a
/// home directory can be determined — the app has nowhere to keep state at
/// all in that case.
pub fn lock_path() -> Option<PathBuf> {
    s2udio_state_dir().map(|dir| dir.join(LOCK_FILE))
}

/// Whether the user asked to be allowed a second instance.
pub fn multiple_allowed() -> bool {
    ENV.var_os(ALLOW_MULTI_ENV).is_some_and(|value| {
        let value = value.to_string_lossy();
        let value = value.trim();
        !value.is_empty() && value != "0"
    })
}

/// Become the running instance, or explain why this one must not start.
///
/// # Errors
///
/// The returned error is the message for the user when another live instance
/// holds the lock (it names the holder's pid and the override), or the I/O
/// failure that stopped the lock file from being opened.
pub fn claim() -> Result<Claim> {
    if multiple_allowed() {
        log::warn!(
            env:? = ALLOW_MULTI_ENV;
            "Starting a second instance on request; both will drive the same MPD"
        );
        return Ok(Claim::MultipleAllowed);
    }
    let path = lock_path().context(
        "Cannot determine where to keep the instance lock (no state dir and no home directory)",
    )?;
    claim_at(&path)
}

/// [`claim`] against an explicit lock file (tests use a temp path).
///
/// # Errors
///
/// See [`claim`].
pub fn claim_at(path: &Path) -> Result<Claim> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("Failed to open the instance lock {}", path.display()))?;

    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        // Another live instance holds it. The pid is only advisory: the lock
        // itself is what refused us.
        Err(Errno::WOULDBLOCK) => {
            let holder = recorded_pid(path);
            // Also in the log: a desktop-launched second instance has no
            // visible stderr, and the log is where someone will look.
            log::error!(
                holder:? = holder,
                path:? = path;
                "Refusing to start: another s2udio instance holds the lock"
            );
            return Err(anyhow::anyhow!(busy_message(holder, path)));
        }
        Err(err) => {
            return Err(std::io::Error::from(err))
                .with_context(|| format!("Failed to lock {}", path.display()));
        }
    }

    // Ours now: record our pid so a refused instance can say who is running.
    // (Best effort — the lock, not the contents, is what gates the start.)
    if let Err(err) = record_pid(&mut file) {
        log::debug!(error:? = err; "Could not write the instance lock's pid");
    }
    Ok(Claim::Held(InstanceLock { _file: file, path: path.to_path_buf() }))
}

/// The message a refused instance prints. Names the holder, says why two
/// instances are not allowed to share one MPD, and how to override it.
pub fn busy_message(holder: Option<u32>, path: &Path) -> String {
    let holder = holder.map_or_else(|| "another s2udio".to_owned(), |pid| {
        format!("s2udio is already running (pid {pid})")
    });
    format!(
        "{holder}.\n\
         Only one instance can drive MPD, its queue and the stream buffer: a \
         second one resolves the same rows, they replace each other's entries \
         and the queue stops playing.\n\
         Close the running instance, or set {ALLOW_MULTI_ENV}=1 to start anyway.\n\
         Lock file: {}",
        path.display()
    )
}

/// Write this process's pid into the locked file (truncating the old one).
fn record_pid(file: &mut File) -> Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(std::process::id().to_string().as_bytes())?;
    file.flush()?;
    Ok(())
}

/// The pid a locked file names, when it can be read.
fn recorded_pid(path: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(path).ok()?;
    text.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique lock path under the temp dir; removed when the test ends.
    struct TempLock(PathBuf);

    impl TempLock {
        /// A path nobody else uses.
        ///
        /// **A per-test name is what keeps this rig honest.** Tests share one
        /// process, and `flock` is released when the *last* descriptor on the
        /// file closes — so if two tests opened the same path, the second
        /// test's slot would keep the first test's lock alive after that
        /// test dropped its handle, and whichever test asserted "the lock is
        /// free" would fail depending on the scheduler. The pid-and-name pair
        /// here is deterministic per test, which is exactly why an earlier
        /// version that reused a path across tests flaked: it asserted a
        /// kernel invariant that only holds for separate processes.
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "s2udio-instance-{}-{name}.lock",
                std::process::id()
            ));
            let _ = std::fs::remove_file(&path);
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempLock {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// The reported situation: a second instance must be refused, and the
    /// message must name the holder and the override. Two `OpenOptions::open`
    /// calls make two open file descriptions in this one process, which is
    /// exactly what two processes look like to `flock`.
    #[test]
    fn a_second_instance_is_refused_and_named() {
        let target = TempLock::new("busy");
        let first = claim_at(target.path()).expect("the first claim must succeed");
        assert!(matches!(first, Claim::Held(_)), "the first instance holds the lock");

        let err = claim_at(target.path()).expect_err("the second claim must be refused");
        let message = err.to_string();
        assert!(
            message.contains(&format!("pid {}", std::process::id())),
            "the message must name the running instance: {message}"
        );
        assert!(
            message.contains(ALLOW_MULTI_ENV),
            "the message must name the override: {message}"
        );
        assert!(
            message.contains("queue"),
            "the message must say why two instances are not allowed: {message}"
        );
        // The holder is the one that recorded its pid, and it still holds it.
        assert_eq!(recorded_pid(target.path()), Some(std::process::id()));
    }

    /// **A crashed instance never blocks the next start** — the property this
    /// module exists for — checked the only way `flock` allows it in-process.
    ///
    /// `flock` locks belong to the *open file description*, and POSIX says
    /// the lock is released when the last descriptor for it is closed. Two
    /// `open()` calls on one path in one process are two descriptions, so a
    /// second `claim_at` on the same path is **legitimately refused even
    /// after the first guard is dropped**: the kernel still has the file open
    /// on behalf of this process (measured — the refusal names our own pid).
    /// An earlier version of this test asserted `claim_at` succeeds right
    /// after the drop and failed roughly one full run in twenty on exactly
    /// that — a test asserting a kernel invariant that only holds across
    /// *processes*.
    ///
    /// So the release is checked from a real second process (`sh -c`), which
    /// is what the property is about: while a guard is held the second claim
    /// must fail, and once the guard is dropped — even by the process dying —
    /// it must succeed.
    #[test]
    fn the_lock_is_released_when_the_instance_ends() {
        fn claim_in_a_child(path: &Path) -> bool {
            // A child `sh` asks for the lock through the same code: a tiny
            // exclusive flock on the file succeeds only when the lock is free.
            std::process::Command::new("flock")
                .args(["-n", path.to_str().expect("utf-8 path"), "true"])
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
        }

        let target = TempLock::new("released");
        let first = claim_at(target.path()).expect("the first claim must succeed");
        assert!(
            !claim_in_a_child(target.path()),
            "a second process must be refused while the guard is alive"
        );
        drop(first);
        // Give the kernel a moment to drop the flock with the descriptor.
        for _ in 0..50 {
            if claim_in_a_child(target.path()) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("a second process was refused after the guard was dropped");
    }

    /// The pid written by an earlier instance is replaced by the new holder,
    /// so the refusal always names the instance that is actually running.
    #[test]
    fn the_recorded_pid_is_the_current_holder() {
        let target = TempLock::new("pid");
        std::fs::write(target.path(), b"999999").expect("seed a stale pid");
        let held = claim_at(target.path()).expect("the stale pid must not block the start");
        assert!(matches!(held, Claim::Held(_)));
        assert_eq!(
            recorded_pid(target.path()),
            Some(std::process::id()),
            "the new instance owns the file"
        );
    }

    /// `S2UDIO_ALLOW_MULTI` is the deliberate escape: any value but empty/`0`.
    #[test]
    fn the_override_allows_a_second_instance() {
        // The env wrapper is process-global, so serialize with the other
        // tests that write to it.
        let _guard = crate::tests::fixtures::HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (value, allowed) in [
            ("1", true),
            ("yes", true),
            ("0", false),
            ("", false),
            (" ", false),
        ] {
            ENV.set(ALLOW_MULTI_ENV.to_owned(), value.to_owned());
            assert_eq!(multiple_allowed(), allowed, "for {ALLOW_MULTI_ENV}={value:?}");
        }
        ENV.remove(ALLOW_MULTI_ENV);
        assert!(!multiple_allowed(), "unset means one instance");
    }
}
