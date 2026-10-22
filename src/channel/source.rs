//! A channel's source: what takes its stream in and writes it into the
//! channel's `Broadcast`.
//!
//! The source here is a camera pulled over RTSP. A media-pp source is used
//! once — a camera that drops is opened again as a new one, in a pipeline of
//! its own — so this keeps how to open it, and the broadcast, which outlives
//! every one of them.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use media_pp::bus::BusEvent;
use media_pp::elements::{
    Broadcast, BroadcastOptions, RtspOptions, RtspSource, RtspTransport, StreamInfo,
};
use media_pp::ffmpeg::media;
use media_pp::pipeline::Pipeline;
use serde::Serialize;

use super::{ChannelConfig, Transport, redact};

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

/// What a channel's source is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceState {
    /// Opening the camera.
    Connecting,
    /// Its stream is coming in.
    Live,
    /// It failed, and is connected again after a wait.
    Waiting,
}

/// What a source is doing, at one moment.
pub(super) struct SourceStatus {
    /// Its URL, with any credentials cut out.
    pub(super) url: String,
    pub(super) state: SourceState,
    pub(super) since: SystemTime,
    /// Why it last failed, while it is not live.
    pub(super) error: Option<String>,
    /// How many times it has gone live.
    pub(super) connections: u64,
}

/// A camera pulled on a thread of its own, into a broadcast.
pub(super) struct Source {
    /// What the source's thread shares with it.
    inner: Arc<Inner>,
    /// `None` before [`Source::run`] and after [`Source::stop`].
    thread: Mutex<Option<JoinHandle<()>>>,
}

/// The half of a source its thread works with.
struct Inner {
    name: String,
    /// The camera's URL, credentials and all: what is opened.
    url: String,
    transport: RtspTransport,
    broadcast: Broadcast,
    state: Mutex<State>,
    stop: AtomicBool,
}

struct State {
    state: SourceState,
    since: SystemTime,
    error: Option<String>,
    connections: u64,
}

impl Source {
    /// A source for the channel called `name`, its broadcast made and nothing
    /// opened yet: what reads the broadcast is attached before [`Self::run`].
    pub(super) fn new(name: &str, config: &ChannelConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                name: name.to_owned(),
                url: config.source.clone(),
                transport: match config.transport {
                    Transport::Tcp => RtspTransport::Tcp,
                    Transport::Udp => RtspTransport::Udp,
                },
                broadcast: Broadcast::new(name, BroadcastOptions::default()),
                state: Mutex::new(State {
                    state: SourceState::Connecting,
                    since: SystemTime::now(),
                    error: None,
                    connections: 0,
                }),
                stop: AtomicBool::new(false),
            }),
            thread: Mutex::new(None),
        }
    }

    /// What it writes into.
    pub(super) fn broadcast(&self) -> &Broadcast {
        &self.inner.broadcast
    }

    /// Starts pulling the camera, connecting again whenever it fails.
    pub(super) fn run(&self) {
        let inner = Arc::clone(&self.inner);
        let thread = std::thread::Builder::new()
            .name(format!("source:{}", inner.name))
            .spawn(move || inner.run())
            .expect("a thread can be started");
        *self.thread.lock().unwrap_or_else(|p| p.into_inner()) = Some(thread);
    }

    pub(super) fn status(&self) -> SourceStatus {
        let state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
        SourceStatus {
            url: redact(&self.inner.url),
            state: state.state,
            since: state.since,
            error: state.error.clone(),
            connections: state.connections,
        }
    }

    /// Stops its thread — within the camera's timeout at most, the time an
    /// open already under way may take — and closes its broadcast.
    pub(super) fn stop(&self) {
        self.inner.stop.store(true, Ordering::Release);
        let thread = self.thread.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
        self.inner.broadcast.close();
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Inner {
    /// The source's thread: pulls the camera, and connects again after a
    /// wait whenever it fails, until the source stops.
    fn run(&self) {
        let mut wait = FIRST_WAIT;
        // Why it last failed, kept while it connects again.
        let mut last = None;
        while !self.stopped() {
            self.set(SourceState::Connecting, last.clone());
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
            self.set(SourceState::Waiting, Some(reason.clone()));
            last = Some(reason);
            if !self.sleep(wait) {
                break;
            }
            wait = (wait * 2).min(LONGEST_WAIT);
        }
    }

    /// Opens the camera: a media-pp source, and the streams of it a
    /// broadcast carries — pictures and sound, not a camera's data or
    /// metadata streams.
    fn open(&self) -> Result<(RtspSource, Vec<StreamInfo>), String> {
        let (source, streams) = RtspSource::open(
            format!("{}-camera", self.name),
            &self.url,
            RtspOptions {
                transport: self.transport,
                timeout: CAMERA_TIMEOUT,
            },
        )
        .map_err(|error| format!("cannot open the camera: {error}"))?;
        let kept: Vec<StreamInfo> = streams
            .into_iter()
            .filter(|stream| matches!(stream.kind(), media::Type::Video | media::Type::Audio))
            .collect();
        if kept.is_empty() {
            return Err("the camera sends neither pictures nor sound".into());
        }
        Ok((source, kept))
    }

    /// Pulls the camera until it fails — the reason — or the source stops:
    /// `Ok`.
    ///
    /// Each connection is a pipeline of its own ending in the broadcast,
    /// published again: the broadcast is what outlives it, and publishing
    /// again is what tells its readers that one run ended and another began.
    fn pull(&self) -> Result<(), String> {
        let (source, streams) = self.open()?;
        let sinks = self
            .broadcast
            .publish(&streams)
            .map_err(|error| format!("cannot publish: {error}"))?;
        let (pipeline, ()) = Pipeline::new(format!("{}-pull", self.name), source, |source, ctx| {
            for (stream, sink) in streams.iter().zip(sinks) {
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

    /// Waits `wait`, or less where the source stops: whether it waited it
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

    fn set(&self, state: SourceState, error: Option<String>) {
        let mut current = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if current.state != state {
            current.since = SystemTime::now();
        }
        current.state = state;
        current.error = error;
    }

    fn went_live(&self) {
        self.set(SourceState::Live, None);
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .connections += 1;
        tracing::info!(channel = %self.name, "live");
    }
}
