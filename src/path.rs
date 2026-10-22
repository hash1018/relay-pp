//! One path: a camera pulled into a `Broadcast`, connected again whenever it
//! drops, and what it is doing for the status API — see `docs/design.md`,
//! "The model".

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

use crate::config::{PathConfig, Transport, redact};

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

/// What a path's source is doing.
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

/// A path's status, as the API gives it.
#[derive(Debug, Clone, Serialize)]
pub struct PathStatus {
    /// The path's name.
    pub name: String,
    /// Its source, with any credentials cut out.
    pub source: String,
    /// What the source is doing.
    pub state: SourceState,
    /// Since when, in milliseconds since the Unix epoch.
    pub since_ms: u64,
    /// Why it last failed, while it is not live.
    pub error: Option<String>,
    /// How many times it has gone live.
    pub connections: u64,
    /// Readers of its broadcast: players and recorders.
    pub readers: usize,
    /// What its broadcast has been written, over every connection.
    pub bytes: u64,
}

struct Shared {
    state: SourceState,
    since: SystemTime,
    error: Option<String>,
    connections: u64,
}

/// A path being served: its camera pulled on a thread of its own.
pub struct ServedPath {
    name: String,
    source: String,
    broadcast: Broadcast,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ServedPath {
    /// Starts pulling `config`'s source into a broadcast of its own.
    pub fn start(name: &str, config: &PathConfig) -> Self {
        let broadcast = Broadcast::new(name, BroadcastOptions::default());
        let shared = Arc::new(Mutex::new(Shared {
            state: SourceState::Connecting,
            since: SystemTime::now(),
            error: None,
            connections: 0,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let puller = Puller {
            name: name.to_owned(),
            url: config.source.clone(),
            transport: match config.transport {
                Transport::Tcp => RtspTransport::Tcp,
                Transport::Udp => RtspTransport::Udp,
            },
            broadcast: broadcast.clone(),
            shared: Arc::clone(&shared),
            stop: Arc::clone(&stop),
        };
        let thread = std::thread::Builder::new()
            .name(format!("path:{name}"))
            .spawn(move || puller.run())
            .expect("a thread can be started");
        Self {
            name: name.to_owned(),
            source: redact(&config.source),
            broadcast,
            shared,
            stop,
            thread: Some(thread),
        }
    }

    /// The broadcast its players read.
    pub fn broadcast(&self) -> &Broadcast {
        &self.broadcast
    }

    /// What it is doing now.
    pub fn status(&self) -> PathStatus {
        let shared = self.shared.lock().unwrap_or_else(|p| p.into_inner());
        let stats = self.broadcast.stats();
        PathStatus {
            name: self.name.clone(),
            source: self.source.clone(),
            state: shared.state,
            since_ms: shared
                .since
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |since| since.as_millis() as u64),
            error: shared.error.clone(),
            connections: shared.connections,
            readers: stats.readers,
            bytes: stats.bytes,
        }
    }

    /// Stops pulling: within the camera's timeout at most, the time an open
    /// already under way may take.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.broadcast.close();
    }
}

impl Drop for ServedPath {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What the path's own thread does.
struct Puller {
    name: String,
    url: String,
    transport: RtspTransport,
    broadcast: Broadcast,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
}

impl Puller {
    fn run(self) {
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
                path = %self.name,
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

    /// Pulls the camera until it fails — the reason — or relay-pp stops:
    /// `Ok`.
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
        let kept: Vec<&StreamInfo> = streams
            .iter()
            .filter(|stream| matches!(stream.kind(), media::Type::Video | media::Type::Audio))
            .collect();
        if kept.is_empty() {
            return Err("the camera sends neither pictures nor sound".into());
        }
        let tracks: Vec<StreamInfo> = kept.iter().map(|stream| (*stream).clone()).collect();
        let sinks = self
            .broadcast
            .publish(&tracks)
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
        self.set(SourceState::Live, None);
        {
            let mut shared = self.shared.lock().unwrap_or_else(|p| p.into_inner());
            shared.connections += 1;
        }
        tracing::info!(path = %self.name, "live");
        let ended = loop {
            if self.stopped() {
                pipeline.stop();
                return Ok(());
            }
            match pipeline.bus().recv_timeout(Duration::from_millis(200)) {
                Ok(BusEvent::Error { error, .. }) => break format!("the camera failed: {error}"),
                Ok(BusEvent::Finished) => break "the camera ended its stream".to_owned(),
                Ok(_) => {}
                Err(_) => {}
            }
        };
        pipeline.stop();
        Err(ended)
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// Waits `wait`, or less where relay-pp stops: whether it waited it all.
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
        let mut shared = self.shared.lock().unwrap_or_else(|p| p.into_inner());
        if shared.state != state {
            shared.since = SystemTime::now();
        }
        shared.state = state;
        shared.error = error;
    }
}
