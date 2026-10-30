//! `shepherd host`: the per-machine daemon. [`runtime`] is the loop, generic over ports; this module wires it to
//! the real adapters under a root directory (`~/.shepherd` in production, a tempdir in tests).
//!
//! Startup order matters:
//! 1. The root is created `0700`, or checked to be private (decision 0010), before anything else touches it.
//! 2. The event log is opened, which takes the root's exclusive lock. A second host fails here, cleanly, before it
//!    could mistake the first host's socket for a stale one and replace it.
//! 3. Only then is the socket bound.
//! 4. Once serving, `shepherd host` releases its stderr (`shepherd_adapters::launcher::release_stderr`), which tells
//!    a client that launched it that it started. A startup error before that is printed there, for that client.

mod env;
mod run_files;
mod runtime;
mod worker;

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;

use shepherd_adapters::private_fs::{self, PrivateFsError};
use shepherd_adapters::{
    GitWorktrees, JsonlEventLog, PortablePty, ShellCommands, TomlConfigSource, UnixLinkError,
    UnixListenLink,
};
use shepherd_core::config::{Config, ConfigError};
use shepherd_core::ports::{ConfigSource, LogError, LogId};

pub use env::{Env, RealEnv};
pub use runtime::{
    CATCH_UP_BATCH, CATCH_UP_TICK, HostRuntime, INITIAL_SIZE, LOCAL_MACHINE, LOW_WATER,
    MAX_CATCH_UP_TICK, RuntimeEvent, RuntimeRx, RuntimeSettings, RuntimeTx, SHUTDOWN_GRACE,
    STALL_TIMEOUT, StartError, runtime_channel,
};

/// The host runtime on the real adapters.
pub type RealHost =
    HostRuntime<PortablePty, GitWorktrees, ShellCommands, UnixListenLink, JsonlEventLog, RealEnv>;

/// Files under the root.
pub const SOCKET_FILE: &str = "host.sock";
pub const LOG_FILE: &str = "host.log";
pub const CONFIG_FILE: &str = "config.toml";
/// Each session's run files (e.g. Claude Code's generated settings) go in `<root>/run/<session>/`.
pub const RUN_DIR: &str = "run";
/// Managed worktrees go in `<root>/worktrees/<repo>/<name>`.
pub const WORKTREES_DIR: &str = "worktrees";

/// Why the host couldn't start. Each message says what to do about it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostStartError {
    #[error(
        "{path} has mode {mode:o}, but it holds your agents' logs and the host socket, so only you may access it. \
         Fix it with: chmod 700 {path}"
    )]
    InsecureRoot { path: PathBuf, mode: u32 },
    #[error("cannot use {path}: {reason}")]
    Root { path: PathBuf, reason: String },
    #[error("a shepherd host is already running for {0}")]
    AlreadyRunning(PathBuf),
    #[error("cannot open the event log in {root}: {error}")]
    Log { root: PathBuf, error: LogError },
    #[error(
        "{path} is corrupt at line {line}: {reason}. Shepherd never rewrites history, so fix or delete that line, \
         or move the file aside to start without it"
    )]
    CorruptLog {
        path: PathBuf,
        line: u64,
        reason: String,
    },
    #[error("cannot listen on the host socket: {0}")]
    Socket(UnixLinkError),
    #[error("{path}: {error}")]
    Config { path: PathBuf, error: ConfigError },
    #[error("cannot open {path}: {reason}")]
    LogFile { path: PathBuf, reason: String },
    #[error("cannot find the running shepherd executable: {0}")]
    Executable(String),
    #[error("cannot start a host thread: {0}")]
    Thread(String),
}

fn root_error(root: &Path, e: PrivateFsError) -> HostStartError {
    match e {
        PrivateFsError::Insecure { path, mode } => HostStartError::InsecureRoot { path, mode },
        other => HostStartError::Root {
            path: root.to_path_buf(),
            reason: other.to_string(),
        },
    }
}

/// Makes sure `root` exists and is private, before anything else touches it.
pub fn prepare_root(root: &Path) -> Result<(), HostStartError> {
    private_fs::ensure_private_dir(root).map_err(|e| root_error(root, e))
}

/// Opens `<root>/host.log` for appending, creating it `0600`. `root` must already be prepared.
pub fn open_log_file(root: &Path) -> Result<File, HostStartError> {
    let path = root.join(LOG_FILE);
    private_fs::create_or_open_private_file(&path, OpenOptions::new().append(true)).map_err(|e| {
        HostStartError::LogFile {
            path,
            reason: e.to_string(),
        }
    })
}

/// A tracing subscriber writing plain-text lines to `file`.
pub fn file_subscriber(file: File) -> impl tracing::Subscriber + Send + Sync {
    tracing_subscriber::fmt()
        .with_writer(Mutex::new(file))
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish()
}

/// Builds the host on the real adapters under `root`, in the order the module docs give. `home` is where new
/// sessions start, `login_shell` the user's `$SHELL`. A failure is also logged, for a host started in the
/// background with no stderr.
pub fn open(
    root: &Path,
    home: PathBuf,
    login_shell: Option<String>,
) -> Result<RealHost, HostStartError> {
    open_inner(root, home, login_shell)
        .inspect_err(|e| tracing::error!(error = %e, "host failed to start"))
}

fn open_inner(
    root: &Path,
    home: PathBuf,
    login_shell: Option<String>,
) -> Result<RealHost, HostStartError> {
    prepare_root(root)?;
    let log = JsonlEventLog::open(root).map_err(|e| log_error(root, e))?;
    let (tx, rx) = runtime_channel();
    let link = UnixListenLink::bind(root.join(SOCKET_FILE), tx.link_sink())
        .map_err(HostStartError::Socket)?;
    let config_path = root.join(CONFIG_FILE);
    let config_error = |error| HostStartError::Config {
        path: config_path.clone(),
        error,
    };
    let raw = TomlConfigSource::new(&config_path)
        .load()
        .map_err(config_error)?;
    let config = Config::resolve(raw, login_shell).map_err(config_error)?;
    let shepherd =
        std::env::current_exe().map_err(|e| HostStartError::Executable(e.to_string()))?;
    let pty = PortablePty::new(tx.pty_sink());
    let worktrees = GitWorktrees::new(root.join(WORKTREES_DIR));
    let settings = RuntimeSettings {
        config,
        home,
        run_root: root.join(RUN_DIR),
        shepherd,
        socket: root.join(SOCKET_FILE),
    };
    let commands = ShellCommands::new();
    HostRuntime::start(
        pty, worktrees, commands, link, log, RealEnv, settings, tx, rx,
    )
    .map_err(|e| match e {
        StartError::Log(e) => log_error(root, e),
        StartError::Worker(e) => HostStartError::Thread(e.to_string()),
    })
}

fn log_error(root: &Path, error: LogError) -> HostStartError {
    match error {
        LogError::Locked => HostStartError::AlreadyRunning(root.to_path_buf()),
        LogError::Corrupt { log, line, reason } => HostStartError::CorruptLog {
            path: log_path(root, &log),
            line,
            reason,
        },
        error => HostStartError::Log {
            root: root.to_path_buf(),
            error,
        },
    }
}

/// Where the JSONL adapter keeps `log` (the layout in `docs/acpp.md`, "Storage").
fn log_path(root: &Path, log: &LogId) -> PathBuf {
    match log {
        LogId::Host => root.join("host.jsonl"),
        LogId::Session(id) => root.join("sessions").join(format!("{id}.jsonl")),
    }
}

/// `shepherd host`: logs to `~/.shepherd/host.log`, runs until SIGTERM or SIGINT, then shuts down cleanly.
pub fn main() -> ExitCode {
    match run_default() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            report(&e);
            ExitCode::FAILURE
        }
    }
}

/// The one place the host prints: a startup error happens before (or instead of) its log file. A client that
/// launched the host reads it from the host's stderr and shows it (see `shepherd_adapters::launcher`).
#[allow(clippy::print_stderr)]
fn report(e: &HostStartError) {
    eprintln!("shepherd host: {e}");
}

fn run_default() -> Result<(), HostStartError> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| HostStartError::Root {
            path: PathBuf::from("~/.shepherd"),
            reason: "$HOME is not set".to_owned(),
        })?;
    let root = home.join(".shepherd");
    let shell = std::env::var("SHELL").ok();

    prepare_root(&root)?;
    let file = open_log_file(&root)?;
    // Only fails if a subscriber is already set, which nothing else in this process does.
    let _ = tracing::subscriber::set_global_default(file_subscriber(file));

    let mut host = open(&root, home, shell)?;
    forward_signals(host.sender());
    tracing::info!(root = %root.display(), "host started");
    // Serving, with the log open: tell a launching client so, by closing the stderr it reads.
    if let Err(error) = shepherd_adapters::launcher::release_stderr() {
        tracing::warn!(%error, "cannot release stderr");
    }
    host.run();
    tracing::info!("host stopped");
    Ok(())
}

/// Turns SIGTERM and SIGINT into [`RuntimeEvent::Shutdown`].
fn forward_signals(tx: RuntimeTx) {
    use signal_hook::consts::{SIGINT, SIGTERM};
    match signal_hook::iterator::Signals::new([SIGTERM, SIGINT]) {
        Ok(mut signals) => {
            let spawned = std::thread::Builder::new()
                .name("host-signals".into())
                .spawn(move || {
                    if signals.forever().next().is_some() {
                        tx.send(RuntimeEvent::Shutdown);
                    }
                });
            if let Err(error) = spawned {
                tracing::error!(%error, "cannot watch for signals");
            }
        }
        Err(error) => tracing::error!(%error, "cannot register signal handlers"),
    }
}
