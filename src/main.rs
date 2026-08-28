use core::{config_watcher::ERROR_CONFIG_MODAL_ID, scheduler::Scheduler};
use std::{
    io::{BufRead, Write},
    path::Path, sync::Arc, time::Duration,
};
use anyhow::{Context, Result};
use clap::Parser;
use crossbeam::channel::unbounded;
use ctx::Ctx;
use log::info;
use shared::{dependencies::CAVA, macros::{status_warn, try_skip}};
use crate::{
    config::{
        Config, ConfigFile, cli::{Args, Command, RemoteCmd},
        cli_config::CliConfig,
    },
    mpd::{client::Client, mpd_client::MpdClient, proto_client::SocketClient},
    shared::{
        config_read::{
            ConfigReadError, ConfigResult, find_first_existing_path, read_cli_config,
            read_config_and_theme, read_config_file, read_config_for_debuginfo,
        },
        dependencies::{
            DEPENDENCIES, FFMPEG, FFPROBE, PYTHON3, PYTHON3MUTAGEN, UEBERZUGPP, YTDLP,
        },
        env::ENV, events::{AppEvent, ClientRequest, WorkRequest},
        logging, mpd_query::{MpdCommand, MpdQuery, MpdQueryResult},
        paths::{config_paths, theme_paths},
        terminal::{TERMINAL, Terminal},
        tmux::{self, IS_TMUX},
    },
};
mod config;
mod core;
mod ctx;
mod jellyfin;
mod mpd;
mod radio;
mod shared;
mod ui;
fn main() -> Result<()> {
    let mut args = Args::parse();
    match args.command {
        Some(Command::Config { current: false }) => {
            std::io::stdout().write_all(include_bytes!("../assets/example_config.ron"))?;
        }
        Some(Command::Theme { current: false }) => {
            std::io::stdout().write_all(include_bytes!("../assets/example_theme.ron"))?;
        }
        Some(Command::Config { current: true }) => {
            handle_config_current(args.config.as_deref())?;
        }
        Some(Command::Theme { current: true }) => {
            handle_theme_current(args.config.as_deref(), args.theme.as_deref())?;
        }
        Some(Command::Raw { command }) => {
            handle_raw(args.config.as_deref(), args.address, args.password, &command)?;
        }
        Some(Command::DebugInfo) => {
            handle_debug_info(
                args.config.as_deref(),
                args.address,
                args.password,
                args.theme.as_deref(),
            )?;
        }
        Some(Command::Version) => {
            handle_version();
        }
        Some(Command::Rq { cmd }) => {
            crate::core::rqctl::run(cmd).map_err(anyhow::Error::msg)?;
        }
        Some(Command::Dl { cmd }) => {
            crate::core::dlctl::run(cmd).map_err(anyhow::Error::msg)?;
        }
        Some(Command::Remote { command, pid }) => {
            handle_remote(command, pid);
        }
        Some(cmd) => {
            logging::init_console().expect("Logger to initialize");
            let config = read_cli_config(
                    args.config.as_deref(),
                    args.address,
                    args.password,
                )
                .unwrap_or_else(|err| {
                    eprintln!("Error: Failed to read config");
                    eprintln!("Caused by:");
                    eprintln!("  {err}");
                    eprintln!("\nUsing the default values");
                    CliConfig::default()
                });
            let result = cmd.execute(&config)?;
            let mut client = Client::init(
                config.address.clone(),
                config.password.clone(),
                "main",
                args.partition.partition.clone(),
                args.partition.autocreate,
            )?;
            client.set_read_timeout(None)?;
            result(&mut client)?;
        }
        None => {
            run_tui(&mut args)?;
        }
    }
    Ok(())
}
fn handle_config_current(config_arg: Option<&Path>) -> Result<()> {
    let config_paths = config_paths(config_arg);
    if config_paths.is_empty() {
        eprintln!("No available config path");
        std::process::exit(1);
    }
    let Some(chosen_config_path) = find_first_existing_path(config_paths) else {
        eprintln!("No config file found to read the theme from.");
        std::process::exit(1);
    };
    std::io::stdout().write_all(&std::fs::read(chosen_config_path)?)?;
    Ok(())
}
fn handle_theme_current(
    config_arg: Option<&Path>,
    theme_arg: Option<&Path>,
) -> Result<()> {
    let config_paths = config_paths(config_arg);
    if config_paths.is_empty() {
        eprintln!("No available config path");
        std::process::exit(1);
    }
    let Some(chosen_config_path) = find_first_existing_path(config_paths) else {
        eprintln!("No config file found to read the theme from.");
        std::process::exit(1);
    };
    let config = read_config_file(&chosen_config_path)?;
    let theme_path = if let Some(theme_name) = config.theme {
        let theme_paths = theme_paths(theme_arg, &chosen_config_path, &theme_name);
        let Some(first_existing_theme_path) = find_first_existing_path(theme_paths) else {
            eprintln!(
                "Theme '{}' specified in the config file at '{}' was not found",
                theme_name, chosen_config_path.display()
            );
            std::process::exit(1);
        };
        first_existing_theme_path
    } else {
        eprintln!(
            "No theme set in the config file at '{}'. Default theme is used.",
            chosen_config_path.display()
        );
        return Ok(());
    };
    std::io::stdout().write_all(&std::fs::read(theme_path)?)?;
    Ok(())
}
fn handle_raw(
    config_arg: Option<&Path>,
    address: Option<String>,
    password: Option<String>,
    command: &str,
) -> Result<()> {
    let config = read_cli_config(config_arg, address, password)
        .unwrap_or_else(|err| {
            eprintln!("Error: Failed to read config");
            eprintln!("Caused by:");
            eprintln!("  {err}");
            eprintln!("\nUsing the default values");
            CliConfig::default()
        });
    let mut client = Client::init(
        config.address.clone(),
        config.password.clone(),
        "debug",
        None,
        false,
    )?;
    client.set_read_timeout(Some(Duration::from_secs(3)))?;
    client.set_write_timeout(Some(Duration::from_secs(3)))?;
    client.stream.write_all(command.as_bytes())?;
    client.stream.write_all(b"\n")?;
    client.stream.flush()?;
    let mut buf = String::new();
    loop {
        client.read().read_line(&mut buf)?;
        print!("{buf}");
        if buf.trim().starts_with("OK") || buf.trim().starts_with("ACK") {
            break;
        }
        buf.clear();
    }
    Ok(())
}
fn handle_debug_info(
    config_arg: Option<&Path>,
    address: Option<String>,
    password: Option<String>,
    theme_arg: Option<&Path>,
) -> Result<()> {
    println!(
        "s2udio {}{}", env!("CARGO_PKG_VERSION"), option_env!("VERGEN_GIT_DESCRIBE")
        .map(| g | format!(" git {g}")).unwrap_or_default()
    );
    let (config_file, config, config_path) = read_config_for_debuginfo(
            config_arg,
            address,
            password,
        )
        .map_or_else(
            |err| {
                if let ConfigReadError::ConfigNotFound = err {
                    println!("\nWarning:");
                    println!("No config file was found. Using default values.");
                } else {
                    println!("\nError: Failed to read config");
                    println!("Caused by:");
                    println!("  {err}");
                    println!("\nUsing the default values");
                }
                (ConfigFile::default(), Config::default(), None)
            },
            |(config_file, config, config_path)| {
                (config_file, config, Some(config_path))
            },
        );
    let mut mpd_host = ENV.var("MPD_HOST").unwrap_or_else(|_| "unset".to_string());
    if let Some(at_idx) = mpd_host.find('@') {
        mpd_host.replace_range(..at_idx, "***");
    }
    let mpd_port = ENV.var("MPD_PORT").unwrap_or_else(|_| "unset".to_string());
    let term = ENV.var("TERM").unwrap_or_else(|_| "unset".to_string());
    let term_program = if *IS_TMUX {
        tmux::environment()?
            .into_iter()
            .find(|(k, _)| k == "TERM_PROGRAM")
            .map_or_else(|| "unset".to_owned(), |(_, v)| v)
    } else {
        ENV.var("TERM_PROGRAM").unwrap_or_else(|_| "unset".to_owned())
    };
    let tmux_passthrough = match tmux::is_passthrough_enabled() {
        Ok(v) => v,
        Err(err) => {
            println!("Failed to check if tmux passthrough is enabled: {err}");
            false
        }
    };
    let tmux_version = match tmux::version() {
        Ok(v) => v,
        Err(err) => {
            println!("Failed to get tmux version: {err}");
            None
        }
    };
    let mpd_info = Client::init(
            config.address.clone(),
            config.password.clone(),
            "debug",
            None,
            false,
        )
        .and_then(|mut client| -> Result<_, _> {
            let version = client.version();
            let commands = client.commands().map(|c| c.0)?;
            let not_commands = client.not_commands().map(|c| c.0)?;
            Ok((version, commands, not_commands))
        });
    let theme_path = config
        .theme_name
        .as_ref()
        .and_then(|theme_name| {
            find_first_existing_path(
                theme_paths(theme_arg, config_path.as_ref()?, theme_name),
            )
        });
    println!("\ns2udio:");
    println!(
        "{:<20} {:?}", "Config path", config_path.map(| c | c.display().to_string())
    );
    println!("{:<20} {:?}", "Theme name", config.theme_name);
    println!("{:<20} {:?}", "Theme path", theme_path);
    println!("{:<20} {:?}", "Debug mode", cfg!(debug_assertions));
    println!("\nMPD:");
    println!("{:<20} {:?}", "Address", config_file.address);
    println!("{:<20} {:?}", "Resolved Address", config.address);
    println!("{:<20} {mpd_host}", "MPD_HOST");
    println!("{:<20} {mpd_port}", "MPD_PORT");
    match mpd_info {
        Ok((version, commands, not_commands)) => {
            println!("{:<20} Success", "Connection");
            println!("{:<20} {version}", "Version");
            println!("{:<20} {commands:?}", "Supported commands");
            println!("{:<20} {not_commands:?}", "Unsupported commands");
        }
        Err(err) => {
            println!("{:<20} Error {err:?}", "Connection");
        }
    }
    println!("\nYoutube playback:");
    println!("{:<20} {:?}", "Cache dir", config.cache_dir);
    println!("{}", FFMPEG.display());
    println!("{}", FFPROBE.display());
    println!("{}", YTDLP.display());
    println!("{}", PYTHON3.display());
    println!("{}", PYTHON3MUTAGEN.display());
    println!("\nImage protocol:");
    println!("{:<20} {}", "Requested", config_file.album_art.method);
    println!("{:<20} {}", "Resolved", config.album_art.method);
    println!("{}", UEBERZUGPP.display());
    println!("\nSystem:");
    println!("{:<20} {:<15} {:?}", "TMUX", * IS_TMUX, tmux_version);
    println!("{:<20} {tmux_passthrough}", "TMUX passthrough");
    println!("{:<20} {term}", "$TERM");
    println!("{:<20} {term_program}", "$TERM_PROGRAM");
    println!("{:<20} {}", "Emulator", TERMINAL.emulator());
    println!("{:<20} {}", "Kitty Keyboard", TERMINAL.keyboard_protocol_kitty());
    println!("{:<20} {}", "ZELLIJ", TERMINAL.zellij());
    println!("\nVisualizer:");
    println!("{}", CAVA.display());
    Ok(())
}
fn handle_version() {
    println!(
        "s2udio {}{}", env!("CARGO_PKG_VERSION"), option_env!("VERGEN_GIT_DESCRIBE")
        .map(| g | format!(" git {g}")).unwrap_or_default()
    );
}
fn handle_remote(command: RemoteCmd, pid: Option<u32>) -> ! {
    let pid = pid
        .or_else(|| {
            std::env::var("PID")
                .context("Failed to read PID from environment variable 'PID'")
                .and_then(|p| {
                    p.parse()
                        .context("Failed to parse PID from environment variable 'PID'")
                })
                .ok()
        });
    let exit_code = command.handle(pid);
    std::process::exit(exit_code.into());
}
fn run_tui(args: &mut Args) -> Result<()> {
    let (worker_tx, worker_rx) = unbounded::<WorkRequest>();
    let (client_tx, client_rx) = unbounded::<ClientRequest>();
    let (event_tx, event_rx) = unbounded::<AppEvent>();
    logging::init(event_tx.clone()).expect("Logger to initialize");
    log::debug!(rev = env!("VERGEN_GIT_DESCRIBE"); "s2udio started");
    std::thread::Builder::new()
        .name("dependency_check".to_string())
        .spawn(|| DEPENDENCIES.iter().for_each(|d| d.log()))?;
    let ConfigResult { config, config_path } = read_config_and_theme(args)
        .unwrap_or_else(|err| {
            if let ConfigReadError::ConfigNotFound = err {
                status_warn!("No config file was found. Using default values.",);
            } else {
                eprintln!("Error: Failed to read config");
                eprintln!("Caused by:");
                eprintln!("  {err}");
                eprintln!("Using the default values");
                try_skip!(
                    event_tx.send(AppEvent::InfoModal { message :
                    vec!["Error: Failed to read config".to_string(), "Caused by:"
                    .to_string(), format!("  {err}"), String::from("\n"),
                    "Using the default values".to_string(),], replacement_id :
                    Some(ERROR_CONFIG_MODAL_ID.into()), title : None, size : None }),
                    "Failed to send info modal request"
                );
            }
            ConfigResult {
                config: Config::default_cli(args),
                config_path: None,
            }
        });
    let mut config = config;
    config.apply_keybinds_override();
    config.apply_cava_override();
    let state = crate::config::state::AppStateFile::load();
    if let Some(mode) = state.video_playback.clone()
        && let Some(mode) = crate::config::video::VideoPlaybackMode::parse(&mode)
    {
        config.video.playback = mode;
    }
    if let Some(lang) = state.mpv_audio_lang.clone()
        && let Some(lang) = crate::config::mpv::MpvAudioLang::parse(&lang)
    {
        config.mpv.audio_lang = lang;
    }
    if let Some(subs) = state.mpv_subtitles.clone()
        && let Some(subs) = crate::config::mpv::MpvSubtitleMode::parse(&subs)
    {
        config.mpv.subtitles = subs;
    }
    if let Some(svp) = state.mpv_svp {
        config.mpv.svp = svp;
    }
    if let Some(proxy) = state.torrent_socks_proxy.clone() {
        config.torrent.socks_proxy = if proxy.trim().is_empty() {
            None
        } else {
            Some(proxy)
        };
    }
    if let Some(ui) = state.ui {
        config.ui = ui;
    }
    crate::shared::mpdris2::write_notify_state(
        config.cache_dir.as_deref(),
        config.ui.mpdris2_notifications,
    );
    let blur_mode_active = crate::core::blur::read_schedule_mode().is_some();
    if let Some(appearance) = state.appearance.clone() {
        crate::ui::modals::settings::apply_persisted_appearance(
            &mut config,
            &appearance,
            blur_mode_active,
        );
    }
    config.validate()?;
    if let Some(lyrics_dir) = &config.lyrics_dir {
        let _ = std::fs::create_dir_all(lyrics_dir);
    }
    if let Some(lyrics_dir) = &config.lyrics_dir && config.enable_lyrics_index {
        worker_tx
            .send(WorkRequest::IndexLyrics {
                lyrics_dir: lyrics_dir.clone(),
            })
            .context("Failed to request lyrics indexing")?;
    }
    event_tx.send(AppEvent::RequestRender).context("Failed to render first frame")?;
    let mut client = Client::init(
            config.address.clone(),
            config.password.clone(),
            "command",
            args.partition.partition.clone(),
            args.partition.autocreate,
        )
        .context("Failed to connect to MPD")?;
    client.set_read_timeout(Some(config.mpd_read_timeout))?;
    client.set_write_timeout(Some(config.mpd_write_timeout))?;
    let tx_clone = event_tx.clone();
    let ctx = Ctx::try_new(
            &mut client,
            config,
            tx_clone,
            worker_tx.clone(),
            client_tx.clone(),
            Scheduler::new((event_tx.clone(), client_tx.clone())),
        )
        .context("Failed to create app context")?;
    core::client::init(
        client_rx.clone(),
        event_tx.clone(),
        client,
        Arc::clone(&ctx.config),
    )?;
    core::work::init(
        worker_rx.clone(),
        client_tx.clone(),
        event_tx.clone(),
        Arc::clone(&ctx.config),
    )?;
    let _sock_guard = core::socket::init(
            event_tx.clone(),
            worker_tx.clone(),
            Arc::clone(&ctx.config),
        )
        .context("Failed to initialize socket listener")?;
    let _config_watcher_guard = if let Some(config_path) = config_path {
        ctx.config
            .enable_config_hot_reload
            .then_some(
                core::config_watcher::init(
                        config_path,
                        ctx.config.theme_name.as_ref().map(|n| format!("{n}.ron",)),
                        event_tx.clone(),
                    )
                    .inspect_err(|e| {
                        log::warn!("Failed to initialize config watcher: {e}")
                    }),
            )
    } else {
        log::warn!("No config file was detected, not watching config for changes");
        None
    };
    let enable_mouse = ctx.config.enable_mouse;
    let terminal = Terminal::setup(enable_mouse).context("Failed to setup terminal")?;
    core::input::init(event_tx.clone())?;
    let event_loop_handle = core::event_loop::init(ctx, event_rx, terminal)?;
    info!("Application initialized successfully");
    event_loop_handle.join().expect("event loop to not panic");
    Terminal::restore(enable_mouse);
    Ok(())
}
