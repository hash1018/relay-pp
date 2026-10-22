//! A channel's source: what takes its stream in and writes it into the
//! channel's `Broadcast`.
//!
//! The source here is a camera pulled over RTSP. A media-pp source is used
//! once — a camera that drops is opened again as a new one, in a pipeline of
//! its own — so this keeps how to open it, and the broadcast, which outlives
//! every one of them.
//!
//! Each connection's pipeline runs on its own thread, as media-pp runs
//! every pipeline: that is the camera's data. What watches it — opens the
//! camera, waits on the pipeline's bus, waits before connecting again, and
//! hears a halt — is a task on the server's runtime, holding no thread while
//! it waits; the open, which blocks, goes to the runtime's blocking pool.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use media_pp::bus::BusEvent;
use media_pp::elements::{
    Broadcast, BroadcastOptions, RtspOptions, RtspSource, RtspTransport, StreamInfo,
};
use media_pp::ffmpeg::media;
use media_pp::pipeline::Pipeline;
use serde::Serialize;
use tokio::runtime::Handle;
use tokio::sync::watch;

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

/// A camera pulled by a task, into a broadcast.
pub(super) struct Source {
    /// What the source's task shares with it.
    inner: Arc<Inner>,
    /// Set once, by [`Source::halt`]; the task watches it.
    halt: watch::Sender<bool>,
    /// Whether the task has ended — set as it goes, however it goes, which
    /// [`Source::wait`] waits for. True before [`Source::run`].
    done: Arc<Done>,
}

/// The half of a source its task works with.
struct Inner {
    name: String,
    /// The camera's URL, credentials and all: what is opened.
    url: String,
    transport: RtspTransport,
    broadcast: Broadcast,
    state: Mutex<State>,
}

struct State {
    state: SourceState,
    since: SystemTime,
    error: Option<String>,
    connections: u64,
}

/// A task having ended, for a thread to wait on.
struct Done {
    done: Mutex<bool>,
    ended: Condvar,
}

/// Owned by the task: says it is done as it drops — when the task returns,
/// and when a runtime shutting down drops it unfinished.
struct Finishing(Arc<Done>);

impl Drop for Finishing {
    fn drop(&mut self) {
        *self.0.done.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.0.ended.notify_all();
    }
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
            }),
            halt: watch::Sender::new(false),
            done: Arc::new(Done {
                done: Mutex::new(true),
                ended: Condvar::new(),
            }),
        }
    }

    /// What it writes into.
    pub(super) fn broadcast(&self) -> &Broadcast {
        &self.inner.broadcast
    }

    /// Starts pulling the camera on `runtime`, connecting again whenever it
    /// fails.
    pub(super) fn run(&self, runtime: &Handle) {
        *self.done.done.lock().unwrap_or_else(|p| p.into_inner()) = false;
        let finishing = Finishing(Arc::clone(&self.done));
        let inner = Arc::clone(&self.inner);
        let halt = self.halt.subscribe();
        runtime.spawn(async move {
            let _finishing = finishing;
            inner.run(halt).await;
        });
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

    /// Tells its task to stop, without waiting: the task lets go of an open
    /// under way at once — it finishes on the blocking pool and is dropped
    /// — and stops a connection's pipeline.
    pub(super) fn halt(&self) {
        self.halt.send_replace(true);
    }

    /// Halts it and waits for its task to end — no longer than a
    /// connection's pipeline takes to stop, which for a camera gone silent
    /// is its timeout. Blocks the thread: never call it on the runtime's.
    pub(super) fn wait(&self) {
        self.halt();
        let done = self.done.done.lock().unwrap_or_else(|p| p.into_inner());
        drop(
            self.done
                .ended
                .wait_while(done, |done| !*done)
                .unwrap_or_else(|p| p.into_inner()),
        );
    }
}

impl Drop for Source {
    /// Halts it only: a source is dropped wherever its channel's last
    /// handle goes, a request on the runtime among them, which must not
    /// wait. The task ends by itself, and its broadcast closes as the task
    /// lets go of it.
    fn drop(&mut self) {
        self.halt();
    }
}

/// How a connection ended, as far as the task's loop cares.
enum Ended {
    /// Halted: the task ends.
    Halted,
    /// Failed, and why: the camera is connected again after a wait.
    Failed(String),
}

impl Inner {
    /// The source's task: pulls the camera, and connects again after a wait
    /// whenever it fails, until it is halted.
    async fn run(self: Arc<Self>, mut halt: watch::Receiver<bool>) {
        let mut wait = FIRST_WAIT;
        // Why it last failed, kept while it connects again.
        let mut last = None;
        loop {
            self.set(SourceState::Connecting, last.clone());
            let began = Instant::now();
            let reason = match Arc::clone(&self).pull(&mut halt).await {
                Ended::Halted => break,
                Ended::Failed(reason) => reason,
            };
            if began.elapsed() >= STAYED_UP {
                wait = FIRST_WAIT;
            }
            // Spread over ±20%, so cameras a network outage dropped together
            // are not all opened again in the same instant.
            let spread = wait.mul_f64(0.8 + 0.4 * unit_random());
            tracing::warn!(
                channel = %self.name,
                "{reason}; connecting again in {:.1}s",
                spread.as_secs_f64()
            );
            self.set(SourceState::Waiting, Some(reason.clone()));
            last = Some(reason);
            tokio::select! {
                () = tokio::time::sleep(spread) => {}
                _ = halt.wait_for(|halted| *halted) => break,
            }
            wait = (wait * 2).min(LONGEST_WAIT);
        }
        self.broadcast.close();
    }

    /// Opens the camera: a media-pp source, and the streams of it a
    /// broadcast carries — pictures and sound, not a camera's data or
    /// metadata streams. Blocks, up to the camera's timeout.
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

    /// Pulls the camera until it fails or the source is halted.
    ///
    /// Each connection is a pipeline of its own ending in the broadcast,
    /// published again: the broadcast is what outlives it, and publishing
    /// again is what tells its readers that one run ended and another began.
    async fn pull(self: Arc<Self>, halt: &mut watch::Receiver<bool>) -> Ended {
        let opening = tokio::task::spawn_blocking({
            let inner = Arc::clone(&self);
            move || inner.open()
        });
        let opened = tokio::select! {
            opened = opening => opened,
            // The open goes on to its end on the blocking pool, and what it
            // opens is dropped there.
            _ = halt.wait_for(|halted| *halted) => return Ended::Halted,
        };
        let (source, streams) = match opened {
            Ok(Ok(opened)) => opened,
            Ok(Err(reason)) => return Ended::Failed(reason),
            Err(_) => return Ended::Failed("opening the camera panicked".into()),
        };
        let pipeline = match self.start(source, &streams) {
            Ok(pipeline) => pipeline,
            Err(reason) => return Ended::Failed(reason),
        };
        self.went_live();

        let ended = tokio::select! {
            reason = watch_bus(&pipeline) => Ended::Failed(reason),
            _ = halt.wait_for(|halted| *halted) => Ended::Halted,
        };
        // Stopping joins the pipeline's thread, which a camera gone silent
        // holds up to its timeout: not on the runtime's own threads.
        let _ = tokio::task::spawn_blocking(move || pipeline.stop()).await;
        ended
    }

    /// Publishes `streams` again and starts a pipeline writing `source`
    /// into them.
    fn start(&self, source: RtspSource, streams: &[StreamInfo]) -> Result<Arc<Pipeline>, String> {
        let sinks = self
            .broadcast
            .publish(streams)
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
        Ok(pipeline)
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

/// Why a connection's pipeline ended, as its bus says — which it does only
/// for a failure or the camera ending its stream, since nothing else stops
/// it while this waits.
async fn watch_bus(pipeline: &Pipeline) -> String {
    loop {
        match pipeline.bus().recv_async().await {
            Some(BusEvent::Error { error, .. }) => return format!("the camera failed: {error}"),
            Some(BusEvent::Finished) => return "the camera ended its stream".to_owned(),
            Some(_) => {}
            None => return "the camera's pipeline ended".to_owned(),
        }
    }
}

/// A number in `[0, 1)`, different every call: `RandomState` is seeded
/// afresh each time, which is all a spread of reconnections needs.
fn unit_random() -> f64 {
    (RandomState::new().hash_one(0u8) >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spread_is_within_its_range_and_not_the_same_each_time() {
        let draws: Vec<f64> = (0..32).map(|_| unit_random()).collect();
        assert!(draws.iter().all(|r| (0.0..1.0).contains(r)), "{draws:?}");
        assert!(draws.windows(2).any(|pair| pair[0] != pair[1]), "{draws:?}");
    }
}
