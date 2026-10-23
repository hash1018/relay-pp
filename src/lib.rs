//! relay-pp: a streaming server on media-pp — see `docs/design.md`.
//!
//! [`Server::start`] binds every listener the configuration names, with no
//! channel yet; channels are made and removed over the API, or
//! [`Server::add_channel`] and [`Server::remove_channel`]. [`Server::stop`]
//! takes it all down again.

mod auth;
mod channel;
mod channels;
mod clock;
pub mod config;
mod http;
mod recordings;

use std::net::SocketAddr;
use std::sync::Arc;

use media_pp::driver::DriverRunner;
use media_pp::elements::{
    Access, Authorize, RtmpServer, RtmpServerOptions, RtspServer, RtspServerOptions, WebRtcServer,
    WebRtcServerOptions,
};

use channel::Listeners;
pub use channel::{
    AttachError, ChannelConfig, ChannelError, ChannelStatus, Client, SourceState, Transport,
};
use channels::Channels;
pub use config::{Config, ConvertChoice, ConvertConfig, SoundConversion, VideoConversion};

/// Why the server could not start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// The configuration cannot be used.
    #[error(transparent)]
    Config(#[from] config::ConfigError),
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
    channels: Arc<Channels>,
    listeners: Listeners,
    rtsp: Arc<DriverRunner>,
    webrtc: Arc<DriverRunner>,
    rtmp: Arc<DriverRunner>,
    http_addr: SocketAddr,
    runtime: Option<tokio::runtime::Runtime>,
    http_stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Server {
    /// Binds every listener `config` names. A listener that cannot be bound
    /// stops the start, saying which.
    pub fn start(config: Config) -> Result<Self, StartError> {
        let listen = |what: &'static str| {
            move |error: media_pp::Error| StartError::Listen {
                what,
                error: error.to_string(),
            }
        };

        let auth = config
            .auth
            .as_ref()
            .map(|auth| Arc::new(auth::Auth::new(auth)));
        if auth.is_none() {
            tracing::warn!(
                "no [auth]: anybody who reaches a port may make and remove channels, play them and watch every recording"
            );
        }
        // A player plays, and a publisher publishes, what it may, as the
        // HTTP routes decide.
        let authorize = |auth: &Arc<auth::Auth>, prefix: &'static str| {
            let auth = Arc::clone(auth);
            Authorize::new(move |request| {
                let right = match request.access {
                    Access::Play => auth::Right::Play,
                    Access::Publish => auth::Right::Publish,
                };
                let channel = request.path.strip_prefix(prefix).unwrap_or(&request.path);
                auth.allows(
                    &auth::from_credentials(&request.credentials),
                    right,
                    channel,
                )
            })
        };
        let rtsp_authorize = auth.as_ref().map(|auth| authorize(auth, ""));
        let rtmp_authorize = auth.as_ref().map(|auth| authorize(auth, channel::RTMP_APP));
        let (rtsp, rtsp_handle) = RtspServer::bind(
            "rtsp",
            config.rtsp.listen,
            RtspServerOptions {
                udp: config.rtsp.udp,
                authorize: rtsp_authorize,
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
        let (rtmp, rtmp_handle) = RtmpServer::bind(
            "rtmp",
            config.rtmp.listen,
            RtmpServerOptions {
                authorize: rtmp_authorize,
                ..RtmpServerOptions::default()
            },
        )
        .map_err(|error| listen("rtmp.listen")(error.into()))?;
        let listeners = Listeners {
            rtsp: rtsp_handle,
            webrtc: webrtc_handle,
            rtmp: rtmp_handle,
        };

        let rtsp = DriverRunner::new(rtsp);
        rtsp.run().map_err(listen("rtsp"))?;
        let webrtc = DriverRunner::new(webrtc);
        webrtc.run().map_err(listen("webrtc"))?;
        let rtmp = DriverRunner::new(rtmp);
        rtmp.run().map_err(listen("rtmp"))?;

        // The API, WHEP, and every channel's source task: what waits, not
        // what carries media, which runs on threads of its own.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("relay")
            .enable_all()
            .build()
            .map_err(|error| StartError::Listen {
                what: "runtime",
                error: error.to_string(),
            })?;
        let channels = Arc::new(Channels::new(
            listeners.clone(),
            config.record.clone(),
            config.convert,
            runtime.handle().clone(),
        ));
        runtime.spawn(recordings::keep(config.record.clone()));
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
            rtsp_port: listeners.rtsp.local_addr().port(),
            rtsp_public_url: config.rtsp.public_url.clone(),
            auth,
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
            rtmp = %listeners.rtmp.local_addr(),
            http = %http_addr,
            "listening"
        );
        Ok(Self {
            channels,
            listeners,
            rtsp,
            webrtc,
            rtmp,
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

    /// Where RTMP publishers connect.
    pub fn rtmp_addr(&self) -> SocketAddr {
        self.listeners.rtmp.local_addr()
    }

    /// Where the API and WHEP are.
    pub fn http_addr(&self) -> SocketAddr {
        self.http_addr
    }

    /// Every channel's status, by name.
    pub fn status(&self) -> Vec<ChannelStatus> {
        self.channels
            .list()
            .iter()
            .map(|channel| channel.status())
            .collect()
    }

    /// Makes a channel and starts pulling its source, as
    /// `POST /api/v1/channels` does.
    pub fn add_channel(&self, config: &ChannelConfig) -> Result<ChannelStatus, ChannelError> {
        self.channels.add(config).map(|channel| channel.status())
    }

    /// Removes a channel, as `DELETE /api/v1/channels/{name}` does, once it
    /// is off every listener and its source has stopped. Whether there was
    /// one. Blocks the thread; not for an async caller.
    pub fn remove_channel(&self, name: &str) -> bool {
        match &self.runtime {
            Some(runtime) => runtime.block_on(self.channels.remove(name)),
            None => false,
        }
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
        // The channels before the runtime: their source tasks run on it, and
        // stop their pipelines there as they end.
        if let Some(runtime) = &self.runtime {
            runtime.block_on(self.channels.clear());
        }
        if let Some(runtime) = self.runtime.take() {
            // What is left on it is a request under way and an open a halt
            // let go of, which ends within a camera's timeout by itself.
            runtime.shutdown_timeout(std::time::Duration::from_secs(2));
        }
        self.rtmp.stop();
        self.webrtc.stop();
        self.rtsp.stop();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shut_down();
    }
}
