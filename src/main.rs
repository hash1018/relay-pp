//! relay-pp: `relay-pp [CONFIG]` listens where the configuration file says —
//! `relay-pp.toml` where none is named, the defaults where there is none —
//! until it is told to stop: Ctrl+C, or what a service manager stops it
//! with. Channels are made over its API.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use relay_pp::{Config, Server};

fn main() -> ExitCode {
    // glibc's heap, which on Linux otherwise kept 1.8 GB for a soak using
    // under a tenth of it (bench/soak/README.md); first, before any thread.
    let trimming = media_pp::heap::keep_small();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    if let Err(error) = trimming {
        tracing::warn!("the heap is not trimmed: {error}");
    }
    // media-pp's own records, every pipeline and server inside, in a file
    // of their own beside relay-pp's.
    let _media_log = match media_pp::log::init("media-pp", "logs", media_pp::log::Level::Info, 7) {
        Ok(guard) => Some(guard),
        Err(error) => {
            tracing::warn!("media-pp's log is not kept: {error}");
            None
        }
    };

    let mut args = std::env::args_os().skip(1);
    let named = args.next().map(PathBuf::from);
    if args.next().is_some() {
        eprintln!("usage: relay-pp [CONFIG]");
        return ExitCode::from(2);
    }
    // A file named must be there; the default one need not be.
    let path = named.or_else(|| {
        let default = Path::new("relay-pp.toml");
        default.exists().then(|| default.to_owned())
    });
    let config = match &path {
        Some(path) => match Config::load(path) {
            Ok(config) => config,
            Err(error) => {
                eprintln!("{}: {error}", path.display());
                return ExitCode::FAILURE;
            }
        },
        None => Config::default(),
    };
    let server = match Server::start(config) {
        Ok(server) => server,
        Err(error) => {
            eprintln!("cannot start: {error}");
            return ExitCode::FAILURE;
        }
    };

    let (told, stop) = std::sync::mpsc::channel();
    let watching = std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || watch_signals(&told));
    if let Err(error) = watching {
        tracing::error!("cannot wait for a signal to stop: {error}");
    }
    match stop.recv() {
        Ok(Some(signal)) => tracing::info!("{signal}: stopping"),
        Ok(None) | Err(_) => tracing::info!("stopping"),
    }
    server.stop();
    tracing::info!("stopped");
    ExitCode::SUCCESS
}

/// Says on `told` which signal came first — `None` where none can be
/// waited for, which stops it at once as one would — and ends the process
/// at the second, unfinished: once a signal is taken here it no longer ends
/// the process by itself, and a stop that waits on a camera must still be
/// cut short by whoever asked for it.
fn watch_signals(told: &std::sync::mpsc::Sender<Option<&'static str>>) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!("cannot wait for a signal to stop: {error}");
            let _ = told.send(None);
            return;
        }
    };
    runtime.block_on(async {
        let mut signals = match Signals::new() {
            Ok(signals) => signals,
            Err(error) => {
                tracing::error!("cannot wait for a signal to stop: {error}");
                let _ = told.send(None);
                return;
            }
        };
        let first = signals.next().await;
        let _ = told.send(Some(first));
        let again = signals.next().await;
        tracing::warn!("{again} while stopping: ending now, unfinished");
        std::process::exit(1);
    });
}

/// What stops it: Ctrl+C, and SIGTERM — what systemd, `docker stop` and
/// `kill` send.
#[cfg(unix)]
struct Signals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl Signals {
    fn new() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    /// The next to come, by name.
    async fn next(&mut self) -> &'static str {
        tokio::select! {
            _ = self.interrupt.recv() => "SIGINT",
            _ = self.terminate.recv() => "SIGTERM",
        }
    }
}

/// What stops it: Ctrl+C and Ctrl+Break, its console closed, and the
/// machine shutting down — the last two held off, by tokio, until this
/// process has stopped or Windows gives up on it. Not a user logging off,
/// which a service hears of for every user.
#[cfg(windows)]
struct Signals {
    c: tokio::signal::windows::CtrlC,
    break_: tokio::signal::windows::CtrlBreak,
    close: tokio::signal::windows::CtrlClose,
    shutdown: tokio::signal::windows::CtrlShutdown,
}

#[cfg(windows)]
impl Signals {
    fn new() -> std::io::Result<Self> {
        use tokio::signal::windows;
        Ok(Self {
            c: windows::ctrl_c()?,
            break_: windows::ctrl_break()?,
            close: windows::ctrl_close()?,
            shutdown: windows::ctrl_shutdown()?,
        })
    }

    /// The next to come, by name.
    async fn next(&mut self) -> &'static str {
        tokio::select! {
            _ = self.c.recv() => "Ctrl+C",
            _ = self.break_.recv() => "Ctrl+Break",
            _ = self.close.recv() => "its console closed",
            _ = self.shutdown.recv() => "the machine shutting down",
        }
    }
}
