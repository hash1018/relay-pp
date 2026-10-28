//! relay-pp: `relay-pp [CONFIG]` listens where the configuration file says —
//! `relay-pp.toml` where none is named, the defaults where there is none —
//! until Ctrl+C. Channels are made over its API.

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

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime for the signal");
    if let Err(error) = runtime.block_on(tokio::signal::ctrl_c()) {
        tracing::error!("cannot wait for Ctrl+C: {error}");
    }
    tracing::info!("stopping");
    server.stop();
    ExitCode::SUCCESS
}
