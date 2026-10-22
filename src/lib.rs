//! relay-pp: a streaming server on media-pp — see `docs/design.md`.
//!
//! [`Server::start`] binds every listener the configuration names and starts
//! every channel; [`Server::stop`] takes it all down again.

mod channel;
pub mod config;
mod http;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use media_pp::driver::DriverRunner;
use media_pp::elements::{RtspServer, RtspServerOptions, WebRtcServer, WebRtcServerOptions};

use channel::{Channel, Listeners};
pub use channel::{ChannelStatus, InputState};
pub use config::Config;

/// Why the server could not start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// The configuration cannot be used.
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    /// A channel could not be registered.
    #[error(transparent)]
    Channel(#[from] channel::ChannelError),
    /// A listener could not be opened.
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
    channels: http::Channels,
    listeners: Listeners,
    rtsp: Arc<DriverRunner>,
    webrtc: Arc<DriverRunner>,
    http_addr: SocketAddr,
    runtime: Option<tokio::runtime::Runtime>,
    http_stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Server {
    /// Binds every listener `config` names and starts every channel. A
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
        let listeners = Listeners {
            rtsp: rtsp_handle,
            webrtc: webrtc_handle,
        };

        let mut channels = BTreeMap::new();
        for (name, channel_config) in &config.channels {
            let channel = Channel::start(name, channel_config, &listeners)?;
            channels.insert(name.clone(), Arc::new(channel));
        }
        let channels: http::Channels = Arc::new(RwLock::new(channels));

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
            channels: Arc::clone(&channels),
            webrtc: listeners.webrtc.clone(),
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
            rtsp = %listeners.rtsp.local_addr(),
            webrtc = %listeners.webrtc.local_addr(),
            http = %http_addr,
            "listening"
        );
        Ok(Self {
            channels,
            listeners,
            rtsp,
            webrtc,
            http_addr,
            runtime: Some(runtime),
            http_stop: Some(http_stop),
        })
    }

    /// Where RTSP players connect.
    pub fn rtsp_addr(&self) -> SocketAddr {
        self.listeners.rtsp.local_addr()
    }

    /// Where WebRTC media goes.
    pub fn webrtc_addr(&self) -> SocketAddr {
        self.listeners.webrtc.local_addr()
    }

    /// Where the API and WHEP are.
    pub fn http_addr(&self) -> SocketAddr {
        self.http_addr
    }

    /// Every channel's status, by name.
    pub fn status(&self) -> Vec<ChannelStatus> {
        let channels: Vec<Arc<Channel>> = self
            .channels
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect();
        channels.iter().map(|channel| channel.status()).collect()
    }

    /// Takes no more players, stops every channel, then the listeners.
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
        let channels =
            std::mem::take(&mut *self.channels.write().unwrap_or_else(|p| p.into_inner()));
        for channel in channels.values() {
            channel.stop();
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
