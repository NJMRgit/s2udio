use std::time::Duration;

use crossbeam::channel::Sender;
use flexi_logger::{Cleanup, Criterion, FileSpec, FlexiLoggerError, LoggerHandle, Naming};

use super::events::Level;
use crate::AppEvent;

/// Rotate the log once it passes this size, keeping a bounded number of
/// generations. A long session only writes a few lines per event, so 8 MiB
/// is many hours of history; the cap matters because the log is now
/// **appended across restarts** instead of truncated, so without rotation
/// it would grow without bound (the very failure mode the round-111 report
/// was about, applied to the logger itself).
const LOG_ROTATE_BYTES: u64 = 8 * 1024 * 1024;
/// How many rotated generations to keep beside the current file.
const LOG_ROTATE_KEEP: usize = 5;

/// The file-backed log spec used by every file-logging entry point: the
/// persistent state dir (`~/.local/state/s2udio`), a stable basename with
/// **no timestamp**, so a restart appends to the same file rather than
/// creating a new one. Round 111: this replaced `std::env::temp_dir()`,
/// which on most systems is tmpfs — a crash artefact evaporated on reboot
/// and was overwritten by the next launch.
///
/// Falls back to the old temp dir only when no home/XDG dir is resolvable,
/// so logging never fails outright.
fn log_file_spec() -> FileSpec {
    match crate::shared::paths::s2udio_log_path() {
        Some(path) => {
            let dir = path.parent().map(std::path::Path::to_path_buf);
            let basename = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "s2udio".to_string());
            let mut spec = FileSpec::default().basename(basename).suppress_timestamp();
            if let Some(dir) = dir {
                spec = spec.directory(dir);
            }
            spec
        }
        None => {
            let uid = rustix::process::geteuid();
            FileSpec::default()
                .directory(std::env::temp_dir())
                .basename(format!("s2udio_{}", uid.as_raw()))
                .suppress_timestamp()
        }
    }
}

/// Append + rotate, applied to every file logger.
///
/// `append()` is the load-bearing half of the round-111 fix: without it
/// flexi_logger truncates the file at start, so the panic text from a death
/// survives only until the next successful launch.
fn with_rotation(logger: flexi_logger::Logger) -> flexi_logger::Logger {
    logger.append().rotate(
        Criterion::Size(LOG_ROTATE_BYTES),
        Naming::Numbers,
        Cleanup::KeepLogFiles(LOG_ROTATE_KEEP),
    )
}

/// Install the panic hook that makes a death **legible**.
///
/// The terminal hook (in `shared::terminal`) already restores the screen,
/// but it does not touch logging: a panic unwound with the log buffer
/// unflushed and no backtrace captured, which is why the round-111 report
/// could say only "it died after a long time" and could not attach a
/// backtrace. This hook runs *before* the terminal hook's own restore
/// (chained via `take_hook`), records the panic with a captured
/// `Backtrace` at the configured `RUST_BACKTRACE` verbosity, then flushes
/// and releases the logger so the line reaches disk.
///
/// Idempotent in practice: called once, from `main`, before any work
/// starts. The previous hook is preserved and still runs.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // `Backtrace::capture` honours RUST_BACKTRACE / RUST_LIB_BACKTRACE:
        // Disabled unless the operator asked for one, so the default death
        // stays cheap and the report's "Monitoring I can attach" recipe
        // (RUST_BACKTRACE=1) yields the full trace.
        let backtrace = std::backtrace::Backtrace::capture();
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>");

        // Written at ERROR so it survives the default `warn` filter, and
        // with the backtrace as its own kv field so the structured log
        // stays parseable (newlines inside a quoted kv value would break a
        // line-oriented reader).
        log::error!(
            panic_location:? = location.as_str(),
            panic_thread:? = thread_name,
            backtrace:? = format!("{backtrace}").as_str();
            "PANIC: {message}"
        );

        // The logger writes through a buffered writer; a process that is
        // about to abort must explicitly push that buffer out.
        log::logger().flush();

        previous(info);
    }));
}

/// Initialise the file logger. Appending and rotation are unconditional
/// (see `with_rotation`); verbosity is controlled by the usual `RUST_LOG`
/// env var, and the destination by `S2UDIO_LOG_FILE` / `S2UDIO_LOG_DIR`.
pub fn init(tx: Sender<AppEvent>) -> Result<LoggerHandle, FlexiLoggerError> {
    #[cfg(debug_assertions)]
    return init_debug(tx);
    #[cfg(not(debug_assertions))]
    return init_release(tx);
}

pub fn init_console() -> Result<LoggerHandle, FlexiLoggerError> {
    flexi_logger::Logger::try_with_env_or_str("warn")?
        .log_to_stderr()
        // status bar is replicated to the normal log file so it is safe to drop
        .add_writer("status_bar", Box::new(NullWriter))
        .format_for_stderr(console_format)
        .set_palette("1;3;15;4;13".to_string())
        .start()
}

/// The downloader daemon's logger (`s2udio dl serve`, round 110).
///
/// The daemon runs detached with its **stderr set to `/dev/null`** and no
/// TUI/status-bar channel, and `dl serve` never installed a logger — so every
/// decision it made (which engine it started, which stream URL it handed out,
/// when it moved a completed download) was invisible. That is why the round-110
/// report ("Stream and download never started the stream") could not be
/// root-caused after the fact. It logs to its own file, appended, so a daemon
/// restart never truncates the record.
pub fn init_daemon() -> Result<LoggerHandle, FlexiLoggerError> {
    let uid = rustix::process::geteuid();
    let spec = match crate::shared::paths::s2udio_state_dir() {
        Some(dir) => FileSpec::default()
            .directory(dir)
            .basename(format!("s2udio_dl_{}", uid.as_raw()))
            .suppress_timestamp(),
        None => FileSpec::default()
            .directory(std::env::temp_dir())
            .basename(format!("s2udio_dl_{}", uid.as_raw()))
            .suppress_timestamp(),
    };
    with_rotation(flexi_logger::Logger::try_with_env_or_str("debug")?.log_to_file(spec))
        .format_for_files(structured_detailed_format)
        .set_palette("1;3;15;4;13".to_string())
        .start()
}

#[allow(dead_code)]
fn init_release(tx: Sender<AppEvent>) -> Result<LoggerHandle, FlexiLoggerError> {
    with_rotation(flexi_logger::Logger::try_with_env_or_str("debug")?.log_to_file(log_file_spec()))
        .add_writer("status_bar", Box::new(StatusBarWriter::new(tx)))
        .format_for_files(structured_detailed_format)
        .set_palette("1;3;15;4;13".to_string())
        .start()
}

#[allow(dead_code)]
fn init_debug(tx: Sender<AppEvent>) -> Result<LoggerHandle, FlexiLoggerError> {
    with_rotation(
        flexi_logger::Logger::try_with_env_or_str("debug")?.log_to_file_and_writer(
            log_file_spec(),
            Box::new(AppEventChannelWriter::new(tx.clone())),
        ),
    )
    .add_writer("status_bar", Box::new(StatusBarWriter::new(tx)))
    .format_for_writer(structured_detailed_format)
    .format_for_files(structured_detailed_format)
    .set_palette("1;3;15;4;13".to_string())
    .start()
}

pub struct NullWriter;
impl flexi_logger::writers::LogWriter for NullWriter {
    fn write(
        &self,
        _now: &mut flexi_logger::DeferredNow,
        _record: &log::Record,
    ) -> std::io::Result<()> {
        Ok(())
    }

    fn flush(&self) -> std::io::Result<()> {
        Ok(())
    }
}

pub struct StatusBarWriter {
    tx: Sender<AppEvent>,
}

impl StatusBarWriter {
    pub fn new(tx: Sender<AppEvent>) -> Self {
        Self { tx }
    }
}

pub struct AppEventChannelWriter {
    tx: Sender<AppEvent>,
    format_fn: Option<flexi_logger::FormatFunction>,
}

impl flexi_logger::writers::LogWriter for StatusBarWriter {
    fn write(
        &self,
        _now: &mut flexi_logger::DeferredNow,
        record: &log::Record,
    ) -> std::io::Result<()> {
        match self.tx.send(AppEvent::Status(
            format!("{}", record.args()),
            record.level().into(),
            Duration::from_secs(5),
        )) {
            Ok(v) => Ok(v),
            Err(err) => Err(std::io::Error::other(err)),
        }
    }

    fn flush(&self) -> std::io::Result<()> {
        Ok(())
    }
}

impl AppEventChannelWriter {
    pub fn new(tx: Sender<AppEvent>) -> Self {
        Self { tx, format_fn: None }
    }
}

impl flexi_logger::writers::LogWriter for AppEventChannelWriter {
    fn write(
        &self,
        now: &mut flexi_logger::DeferredNow,
        record: &log::Record,
    ) -> std::io::Result<()> {
        let mut buf = Vec::new();
        (self.format_fn).map(|fun| fun(&mut buf, now, record));

        match self.tx.send(AppEvent::Log(buf)) {
            Ok(v) => Ok(v),
            Err(err) => Err(std::io::Error::other(err)),
        }
    }

    fn flush(&self) -> std::io::Result<()> {
        Ok(())
    }

    fn format(&mut self, format: flexi_logger::FormatFunction) {
        self.format_fn = Some(format);
    }
}

impl From<log::Level> for Level {
    fn from(level: log::Level) -> Self {
        match level {
            log::Level::Error => Level::Error,
            log::Level::Warn => Level::Warn,
            log::Level::Info => Level::Info,
            log::Level::Debug => Level::Debug,
            log::Level::Trace => Level::Trace,
        }
    }
}

pub fn console_format(
    w: &mut dyn std::io::Write,
    _now: &mut flexi_logger::DeferredNow,
    record: &log::Record,
) -> anyhow::Result<(), std::io::Error> {
    let mut visitor = Visitor::new();
    match record.key_values().visit(&mut visitor) {
        Ok(()) => {}
        Err(err) => {
            return Err(std::io::Error::other(err));
        }
    }
    let level = record.level();

    write!(
        w,
        r"{:<5}: {} {}",
        flexi_logger::style(level).paint(level.to_string()),
        &record.args().to_string(),
        visitor
    )
}

pub fn structured_detailed_format(
    w: &mut dyn std::io::Write,
    now: &mut flexi_logger::DeferredNow,
    record: &log::Record,
) -> anyhow::Result<(), std::io::Error> {
    let mut visitor = Visitor::new();
    match record.key_values().visit(&mut visitor) {
        Ok(()) => {}
        Err(err) => {
            return Err(std::io::Error::other(err));
        }
    }
    write!(
        w,
        r#"{} {:<5} thread={} {}:{} message="{}" {}"#,
        now.now_utc_owned().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        record.level().to_string(),
        std::thread::current().name().unwrap_or("<unnamed>"),
        record.file().unwrap_or("<unnamed>"),
        record.line().unwrap_or(0),
        &record.args().to_string(),
        visitor
    )
}

#[derive(Debug)]
struct Visitor {
    values: Vec<(String, String)>,
}

impl Visitor {
    fn new() -> Self {
        Self { values: Vec::new() }
    }
}

impl std::fmt::Display for Visitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for ele in &self.values {
            write!(f, r#"{}="{}" "#, ele.0, ele.1)?;
        }
        Ok(())
    }
}

impl<'kvs> log::kv::VisitSource<'kvs> for Visitor {
    fn visit_pair(
        &mut self,
        key: log::kv::Key<'kvs>,
        value: log::kv::Value<'kvs>,
    ) -> Result<(), log::kv::Error> {
        self.values.push((key.to_string(), value.to_string()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Apply exactly the configuration `with_rotation` applies, to a
    /// **file writer** rather than the global logger.
    ///
    /// The append/rotate decision lives in `Logger`, but a process may
    /// install only one global logger, and the test binary has already
    /// installed (or another test may install) one. Driving the same
    /// `FileLogWriter` configuration directly tests the identical
    /// file-handling behaviour without touching the global logger — and it
    /// is the file handling that the report was about.
    fn file_writer(dir: &std::path::Path) -> flexi_logger::writers::FileLogWriter {
        flexi_logger::writers::FileLogWriter::builder(
            FileSpec::default()
                .directory(dir)
                .basename("s2u-append-test")
                .suppress_timestamp(),
        )
        .append()
        .rotate(
            Criterion::Size(LOG_ROTATE_BYTES),
            Naming::Numbers,
            Cleanup::KeepLogFiles(LOG_ROTATE_KEEP),
        )
        .try_build()
        .expect("file log writer builds")
    }

    /// Round 111 regression: the report's central complaint was that
    /// "crash evidence is overwritten on every start". With `append()`
    /// dropped (the pre-round-111 behaviour) a second start on the same path
    /// truncates the file, so the previous session's last lines — the panic
    /// text, the only evidence — are gone.
    #[test]
    fn a_restart_appends_to_the_log_instead_of_truncating_it() {
        use flexi_logger::writers::LogWriter;
        let dir = std::env::temp_dir().join(format!("s2u-logtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // flexi_logger names the live file `<basename>_rCURRENT.log`.
        let path = dir.join("s2u-append-test_rCURRENT.log");

        let write_marker = |marker: &str| {
            let writer = file_writer(&dir);
            let mut now = flexi_logger::DeferredNow::new();
            writer
                .write(&mut now, &log::Record::builder().level(log::Level::Error).args(format_args!("{marker}")).build())
                .unwrap();
            writer.flush().unwrap();
        };

        // First "run": the line stands in for a logged panic.
        write_marker("FIRST-RUN-MARKER");
        let after_first = std::fs::read_to_string(&path).unwrap();
        assert!(after_first.contains("FIRST-RUN-MARKER"), "first run wrote its line");

        // Second "run" on the same path: the first run's line must survive.
        write_marker("SECOND-RUN-MARKER");
        let after_second = std::fs::read_to_string(&path).unwrap();

        assert!(
            after_second.contains("FIRST-RUN-MARKER"),
            "the previous session's log survived the restart — this is the \
             evidence the report could not attach. Got:\n{after_second}"
        );
        assert!(after_second.contains("SECOND-RUN-MARKER"), "second run wrote its own line");
        assert!(
            after_second.len() > after_first.len(),
            "the file grew across restarts instead of being truncated"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The poison-pill case for a long-lived, appended log: it must be
    /// bounded. `with_rotation` caps the kept generations, so a session that
    /// logs for weeks cannot fill the disk.
    #[test]
    fn rotation_is_configured_with_a_size_cap() {
        assert!(
            LOG_ROTATE_BYTES > 0 && LOG_ROTATE_BYTES <= 64 * 1024 * 1024,
            "rotation cap is sane, got {LOG_ROTATE_BYTES}"
        );
        assert!(LOG_ROTATE_KEEP > 0, "at least the current generation is kept");
    }

    /// An explicit `S2UDIO_LOG_FILE` override is used verbatim, so the dev
    /// harness and tests can aim the log at a scratch path and never touch
    /// the operator's real state dir.
    #[test]
    fn log_file_spec_honours_the_explicit_override() {
        let _home_guard = crate::tests::fixtures::HOME_LOCK.lock().unwrap();
        use crate::shared::env::ENV;
        let explicit = std::path::PathBuf::from("/s2u-explicit/place.log");
        ENV.set("S2UDIO_LOG_FILE".to_string(), explicit.to_string_lossy().into_owned());

        let path = crate::shared::paths::s2udio_log_path();

        ENV.remove("S2UDIO_LOG_FILE");
        assert_eq!(path, Some(explicit));
    }
}
