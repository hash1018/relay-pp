//! A channel: one input, one `Broadcast`, and the outputs that read it —
//! see `docs/design.md`, "The model".
//!
//! The input here is a camera pulled over RTSP, connected again whenever it
//! drops. The outputs are the channel's registrations on the server's RTSP
//! and WebRTC listeners, which every channel shares: one port each, the
//! channel picked by the name in the URL.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use media_pp::bus::BusEvent;
use media_pp::elements::{
    Broadcast, BroadcastOptions, RtspOptions, RtspPathMode, RtspServerHandle, RtspSource,
    RtspTransport, StreamInfo, WebRtcPathMode, WebRtcServerHandle,
};
use media_pp::ffmpeg::media;
use media_pp::pipeline::Pipeline;
use serde::Serialize;

use crate::config::{ChannelConfig, Transport, redact};

/// How long a camera has to answer, or to go on sending, before it is taken
/// to be gone.
const CAMERA_TIMEOUT: Duration = Duration::from_secs(5);
/// The first wait before connecting again, doubled after each failure up to
/// [`LONGEST_WAIT`].
const FIRST_WAIT: Duration = Duration::from_secs(1);
const LONGEST_WAIT: Duration = Duration::from_secs(30);
/// How long a camera has to stay live for the wait to start again from
/// [`FIRST_WAIT`]: one that drops as soon as it is connected is waited for
/// longer and longer, not every second.
const STAYED_UP: Duration = Duration::from_secs(10);

/// What a channel's input is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputState {
    /// Opening the camera.
    Connecting,
    /// Its stream is coming in.
    Live,
    /// It failed, and is connected again after a wait.
    Waiting,
}

/// A channel's status, as the API gives it.
#[derive(Debug, Clone, Serialize)]
pub struct ChannelStatus {
    /// The channel's name.
    pub name: String,
    /// Its input's source, with any credentials cut out.
    pub source: String,
    /// What the input is doing.
    pub state: InputState,
    /// Since when, in milliseconds since the Unix epoch.
    pub since_ms: u64,
    /// Why the input last failed, while it is not live.
    pub error: Option<String>,
    /// How many times the input has gone live.
    pub connections: u64,
    /// Readers of its broadcast: players and recorders.
    pub readers: usize,
    /// What its broadcast has been written, over every connection.
    pub bytes: u64,
    /// The outputs it has: `"rtsp"`, `"webrtc"`.
    pub outputs: Vec<&'static str>,
}

/// The listeners every channel's outputs are registered on.
#[derive(Clone)]
pub(crate) struct Listeners {
    pub(crate) rtsp: RtspServerHandle,
    pub(crate) webrtc: WebRtcServerHandle,
}

/// One of a channel's outputs.
enum Output {
    /// The channel registered on the RTSP listener.
    Rtsp(RtspServerHandle),
    /// The channel registered on the WebRTC listener.
    WebRtc(WebRtcServerHandle),
}

impl Output {
    fn kind(&self) -> &'static str {
        match self {
            Output::Rtsp(_) => "rtsp",
            Output::WebRtc(_) => "webrtc",
        }
    }

    /// Takes the channel off the listener: no player opens it from here.
    fn remove(&self, name: &str) {
        match self {
            Output::Rtsp(listener) => {
                listener.remove_path(name);
            }
            Output::WebRtc(listener) => {
                listener.remove_path(name);
            }
        }
    }
}

/// Why a channel could not be started.
#[derive(Debug, thiserror::Error)]
#[error("channel {name}: {error}")]
pub struct ChannelError {
    name: String,
    error: String,
}

/// A channel being served: registered on every listener, its camera pulled
/// on a thread of its own and connected again whenever it drops.
pub struct Channel {
    /// What the channel's thread shares with it.
    inner: Arc<Inner>,
    outputs: Vec<Output>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

/// The half of a channel its thread works with.
struct Inner {
    name: String,
    /// The camera's URL, credentials and all: what is opened.
    url: String,
    transport: RtspTransport,
    broadcast: Broadcast,
    input: Mutex<Input>,
    stop: AtomicBool,
}

/// What the input is doing, as the status gives it.
struct Input {
    state: InputState,
    since: SystemTime,
    error: Option<String>,
    connections: u64,
}

impl Channel {
    /// Registers a channel on every listener and starts pulling its camera.
    /// Nothing is left registered where one registration fails.
    pub(crate) fn start(
        name: &str,
        config: &ChannelConfig,
        listeners: &Listeners,
    ) -> Result<Self, ChannelError> {
        let broadcast = Broadcast::new(name, BroadcastOptions::default());
        let mut outputs = Vec::new();
        let registered = listeners
            .rtsp
            .add_path(name, &broadcast, RtspPathMode::Play)
            .map(|()| outputs.push(Output::Rtsp(listeners.rtsp.clone())))
            .map_err(|error| error.to_string())
            .and_then(|()| {
                listeners
                    .webrtc
                    .add_path(name, &broadcast, WebRtcPathMode::Play)
                    .map(|()| outputs.push(Output::WebRtc(listeners.webrtc.clone())))
                    .map_err(|error| error.to_string())
            });
        if let Err(error) = registered {
            for output in &outputs {
                output.remove(name);
            }
            return Err(ChannelError {
                name: name.to_owned(),
                error,
            });
        }

        let inner = Arc::new(Inner {
            name: name.to_owned(),
            url: config.source.clone(),
            transport: match config.transport {
                Transport::Tcp => RtspTransport::Tcp,
                Transport::Udp => RtspTransport::Udp,
            },
            broadcast,
            input: Mutex::new(Input {
                state: InputState::Connecting,
                since: SystemTime::now(),
                error: None,
                connections: 0,
            }),
            stop: AtomicBool::new(false),
        });
        let thread = {
            let inner = Arc::clone(&inner);
            std::thread::Builder::new()
                .name(format!("channel:{name}"))
                .spawn(move || inner.run())
                .expect("a thread can be started")
        };
        tracing::info!(channel = %name, source = %redact(&config.source), "serving");
        Ok(Self {
            inner,
            outputs,
            thread: Mutex::new(Some(thread)),
        })
    }

    /// What it is doing now.
    pub fn status(&self) -> ChannelStatus {
        let inner = &self.inner;
        let input = inner.input.lock().unwrap_or_else(|p| p.into_inner());
        let stats = inner.broadcast.stats();
        ChannelStatus {
            name: inner.name.clone(),
            source: redact(&inner.url),
            state: input.state,
            since_ms: input
                .since
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |since| since.as_millis() as u64),
            error: input.error.clone(),
            connections: input.connections,
            readers: stats.readers,
            bytes: stats.bytes,
            outputs: self.outputs.iter().map(Output::kind).collect(),
        }
    }

    /// Takes it off every listener, stops its thread — within the camera's
    /// timeout at most, the time an open already under way may take — and
    /// closes its broadcast.
    pub(crate) fn stop(&self) {
        for output in &self.outputs {
            output.remove(&self.inner.name);
        }
        self.inner.stop.store(true, Ordering::Release);
        let thread = self.thread.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
        self.inner.broadcast.close();
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Inner {
    /// The channel's thread: pulls the camera, and connects again after a
    /// wait whenever it fails, until the channel stops.
    fn run(&self) {
        let mut wait = FIRST_WAIT;
        // Why it last failed, kept while it connects again.
        let mut last = None;
        while !self.stopped() {
            self.set(InputState::Connecting, last.clone());
            let began = Instant::now();
            let reason = match self.pull() {
                Ok(()) => break,
                Err(reason) => reason,
            };
            if began.elapsed() >= STAYED_UP {
                wait = FIRST_WAIT;
            }
            tracing::warn!(
                channel = %self.name,
                "{reason}; connecting again in {}s",
                wait.as_secs()
            );
            self.set(InputState::Waiting, Some(reason.clone()));
            last = Some(reason);
            if !self.sleep(wait) {
                break;
            }
            wait = (wait * 2).min(LONGEST_WAIT);
        }
    }

    /// Pulls the camera until it fails — the reason — or the channel stops:
    /// `Ok`.
    ///
    /// Each connection is a pipeline of its own ending in the broadcast,
    /// published again: the broadcast is what outlives it, and publishing
    /// again is what tells its readers that one run ended and another began.
    fn pull(&self) -> Result<(), String> {
        let (source, streams) = RtspSource::open(
            format!("{}-camera", self.name),
            &self.url,
            RtspOptions {
                transport: self.transport,
                timeout: CAMERA_TIMEOUT,
            },
        )
        .map_err(|error| format!("cannot open the camera: {error}"))?;
        // A camera's data or metadata stream goes nowhere: a broadcast
        // carries pictures and sound.
        let kept: Vec<StreamInfo> = streams
            .into_iter()
            .filter(|stream| matches!(stream.kind(), media::Type::Video | media::Type::Audio))
            .collect();
        if kept.is_empty() {
            return Err("the camera sends neither pictures nor sound".into());
        }
        let sinks = self
            .broadcast
            .publish(&kept)
            .map_err(|error| format!("cannot publish: {error}"))?;
        let (pipeline, ()) = Pipeline::new(format!("{}-pull", self.name), source, |source, ctx| {
            for (stream, sink) in kept.iter().zip(sinks) {
                ctx.attach(source, stream.index(), ctx.branch().to(sink)?)?;
            }
            Ok(())
        })
        .map_err(|error| format!("cannot wire the camera: {error}"))?;
        pipeline
            .run()
            .map_err(|error| format!("cannot start the camera: {error}"))?;
        self.went_live();

        let ended = loop {
            if self.stopped() {
                pipeline.stop();
                return Ok(());
            }
            match pipeline.bus().recv_timeout(Duration::from_millis(200)) {
                Ok(BusEvent::Error { error, .. }) => break format!("the camera failed: {error}"),
                Ok(BusEvent::Finished) => break "the camera ended its stream".to_owned(),
                _ => {}
            }
        };
        pipeline.stop();
        Err(ended)
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// Waits `wait`, or less where the channel stops: whether it waited it
    /// all.
    fn sleep(&self, wait: Duration) -> bool {
        let until = Instant::now() + wait;
        while Instant::now() < until {
            if self.stopped() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        !self.stopped()
    }

    fn set(&self, state: InputState, error: Option<String>) {
        let mut input = self.input.lock().unwrap_or_else(|p| p.into_inner());
        if input.state != state {
            input.since = SystemTime::now();
        }
        input.state = state;
        input.error = error;
    }

    fn went_live(&self) {
        self.set(InputState::Live, None);
        self.input
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .connections += 1;
        tracing::info!(channel = %self.name, "live");
    }
}
