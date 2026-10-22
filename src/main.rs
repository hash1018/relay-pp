//! relay-pp: `relay-pp [CONFIG]` serves what the configuration file says —
//! `relay-pp.toml` where none is named — until Ctrl+C.

use std::path::PathBuf;
use std::process::ExitCode;

use relay_pp::{Config, Server};

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
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
    let path = args
        .next()
        .map_or_else(|| PathBuf::from("relay-pp.toml"), PathBuf::from);
    if args.next().is_some() {
        eprintln!("usage: relay-pp [CONFIG]");
        return ExitCode::from(2);
    }

    let config = match Config::load(&path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{}: {error}", path.display());
            return ExitCode::FAILURE;
        }
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
