//! relay-pp: a streaming server on media-pp — see `docs/design.md`.
//!
//! [`Server::start`] binds every listener the configuration names and starts
//! pulling every path's camera; [`Server::stop`] takes it all down again.

pub mod config;
mod http;
mod path;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use media_pp::driver::DriverRunner;
use media_pp::elements::{
    RtspPathMode, RtspServer, RtspServerHandle, RtspServerOptions, WebRtcPathMode, WebRtcServer,
    WebRtcServerHandle, WebRtcServerOptions,
};

pub use config::Config;
use path::ServedPath;
pub use path::{PathStatus, SourceState};

/// Why the server could not start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// The configuration cannot be used.
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    /// A listener could not be opened, or a path not served.
    #[error("{what}: {error}")]
    Listen {
        /// Which.
        what: &'static str,
        /// Why.
        error: String,
    },
}

/// A running server.
pub struct Server {
    paths: Arc<Mutex<BTreeMap<String, ServedPath>>>,
    rtsp: Arc<DriverRunner>,
    rtsp_handle: RtspServerHandle,
    webrtc: Arc<DriverRunner>,
    webrtc_handle: WebRtcServerHandle,
    http_addr: SocketAddr,
    runtime: Option<tokio::runtime::Runtime>,
    http_stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Server {
    /// Binds every listener `config` names and starts every path. A
    /// listener that cannot be bound stops the start, saying which.
    pub fn start(config: Config) -> Result<Self, StartError> {
        let listen = |what: &'static str| {
            move |error: media_pp::Error| StartError::Listen {
                what,
                error: error.to_string(),
            }
        };

        let (rtsp, rtsp_handle) = RtspServer::bind(
            "rtsp",
            config.rtsp.listen,
            RtspServerOptions {
                udp: config.rtsp.udp,
                ..RtspServerOptions::default()
            },
        )
        .map_err(|error| listen("rtsp.listen")(error.into()))?;
        let defaults = WebRtcServerOptions::default();
        let webrtc_options = WebRtcServerOptions {
            candidates: config.webrtc.candidate_addrs()?,
            threads: config.webrtc.threads.unwrap_or(defaults.threads),
            ..defaults
        };
        let (webrtc, webrtc_handle) =
            WebRtcServer::bind("webrtc", config.webrtc.listen, webrtc_options)
                .map_err(|error| listen("webrtc.listen")(error.into()))?;

        let mut paths = BTreeMap::new();
        for (name, path_config) in &config.paths {
            let path = ServedPath::start(name, path_config);
            rtsp_handle
                .add_path(name, path.broadcast(), RtspPathMode::Play)
                .map_err(|error| listen("paths")(error.into()))?;
            webrtc_handle
                .add_path(name, path.broadcast(), WebRtcPathMode::Play)
                .map_err(|error| listen("paths")(error.into()))?;
            tracing::info!(path = %name, source = %config::redact(&path_config.source), "serving");
            paths.insert(name.clone(), path);
        }
        let paths = Arc::new(Mutex::new(paths));

        let rtsp = DriverRunner::new(rtsp);
        rtsp.run().map_err(listen("rtsp"))?;
        let webrtc = DriverRunner::new(webrtc);
        webrtc.run().map_err(listen("webrtc"))?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("http")
            .enable_all()
            .build()
            .map_err(|error| StartError::Listen {
                what: "http",
                error: error.to_string(),
            })?;
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind(config.http.listen))
            .map_err(|error| StartError::Listen {
                what: "http.listen",
                error: error.to_string(),
            })?;
        let http_addr = listener.local_addr().map_err(|error| StartError::Listen {
            what: "http.listen",
            error: error.to_string(),
        })?;
        let router = http::router(http::AppState {
            paths: Arc::clone(&paths),
            webrtc: webrtc_handle.clone(),
        });
        let (http_stop, stopped) = tokio::sync::oneshot::channel::<()>();
        runtime.spawn(async move {
            let served = axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await;
            if let Err(error) = served {
                tracing::error!("http: {error}");
            }
        });
        tracing::info!(
            rtsp = %rtsp_handle.local_addr(),
            webrtc = %webrtc_handle.local_addr(),
            http = %http_addr,
            "listening"
        );
        Ok(Self {
            paths,
            rtsp,
            rtsp_handle,
            webrtc,
            webrtc_handle,
            http_addr,
            runtime: Some(runtime),
            http_stop: Some(http_stop),
        })
    }

    /// Where RTSP players connect.
    pub fn rtsp_addr(&self) -> SocketAddr {
        self.rtsp_handle.local_addr()
    }

    /// Where WebRTC media goes.
    pub fn webrtc_addr(&self) -> SocketAddr {
        self.webrtc_handle.local_addr()
    }

    /// Where the API and WHEP are.
    pub fn http_addr(&self) -> SocketAddr {
        self.http_addr
    }

    /// Every path's status, by name.
    pub fn status(&self) -> Vec<PathStatus> {
        let paths = self.paths.lock().unwrap_or_else(|p| p.into_inner());
        paths.values().map(ServedPath::status).collect()
    }

    /// Takes no more players, stops every path, then the listeners.
    pub fn stop(mut self) {
        self.shut_down();
    }

    fn shut_down(&mut self) {
        if self.runtime.is_none() {
            return;
        }
        if let Some(stop) = self.http_stop.take() {
            let _ = stop.send(());
        }
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(std::time::Duration::from_secs(2));
        }
        let paths = std::mem::take(&mut *self.paths.lock().unwrap_or_else(|p| p.into_inner()));
        for (_, mut path) in paths {
            path.stop();
        }
        self.webrtc.stop();
        self.rtsp.stop();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shut_down();
    }
}
