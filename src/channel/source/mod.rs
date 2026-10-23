//! A channel's source: what takes its stream in and writes it into the
//! channel's hub, through its [`Publisher`] — a camera pulled
//! ([`PullSource`]), or a publisher taken ([`publish`]).
//!
//! A camera is pulled over RTSP, connected again whenever it drops: each
//! connection media-pp's `RtspSource` in a pipeline of its own ([`rtsp`]),
//! its tracks published to the hub and written into what it gives back,
//! which outlives every connection.
//!
//! What watches a connection — waits on it, waits before connecting again,
//! hears a halt — is the source's task, holding no thread while it waits.

mod publish;
mod rtsp;

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use media_pp::elements::RtspTransport;
use serde::Serialize;
use tokio::runtime::Handle;
use tokio::sync::watch;

use super::hub::Publisher;
use super::sink::Listeners;
use super::task::Task;
use super::{ChannelConfig, Transport, redact};
pub use publish::Publish;
use publish::PublishSource;
pub(crate) use publish::{RTMP_APP, whip_path};

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
    /// No publisher is publishing into it.
    AwaitingPublisher,
}

/// What a source is doing, at one moment.
pub(in crate::channel) struct SourceStatus {
    /// Its URL, with any credentials cut out.
    pub(in crate::channel) url: String,
    pub(in crate::channel) state: SourceState,
    pub(in crate::channel) since: SystemTime,
    /// Why it last failed, while it is not live.
    pub(in crate::channel) error: Option<String>,
    /// How many times it has gone live.
    pub(in crate::channel) connections: u64,
    /// How its stream comes in: `"rtsp"` for a camera pulled, a
    /// publisher's protocol for one pushed — once it has.
    pub(in crate::channel) client: Option<&'static str>,
}

/// A channel's source.
pub(in crate::channel) enum Source {
    Pull(PullSource),
    Publish(PublishSource),
}

impl Source {
    /// The source `config` says for the channel called `name`, writing
    /// through `publisher`: a camera, nothing opened yet, or a publisher,
    /// its server already taking one.
    pub(in crate::channel) fn new(
        name: &str,
        config: &ChannelConfig,
        publisher: Publisher,
        listeners: &Listeners,
    ) -> Result<Self, String> {
        match (&config.source, config.publish) {
            (Some(url), None) => Ok(Source::Pull(PullSource::new(name, url, config, publisher))),
            (None, Some(protocol)) => {
                PublishSource::new(name, protocol, publisher, listeners).map(Source::Publish)
            }
            _ => Err("a source to pull, or a protocol to take a publisher over".to_owned()),
        }
    }

    pub(in crate::channel) fn run(&self, runtime: &Handle) {
        match self {
            Source::Pull(source) => source.run(runtime),
            Source::Publish(source) => source.run(runtime),
        }
    }

    pub(in crate::channel) fn status(&self) -> SourceStatus {
        match self {
            Source::Pull(source) => source.status(),
            Source::Publish(source) => source.status(),
        }
    }

    pub(in crate::channel) fn halt(&self) {
        match self {
            Source::Pull(source) => source.halt(),
            Source::Publish(source) => source.halt(),
        }
    }

    pub(in crate::channel) async fn wait(&self) {
        match self {
            Source::Pull(source) => source.wait().await,
            Source::Publish(source) => source.wait().await,
        }
    }
}

/// A camera pulled by a task, into a hub.
pub(in crate::channel) struct PullSource {
    /// What the source's task shares with it.
    inner: Arc<Inner>,
    task: Task,
}

/// The half of a source its task works with.
struct Inner {
    name: String,
    /// The camera's URL, credentials and all: what is opened.
    url: String,
    transport: RtspTransport,
    /// What each connection's tracks are published through: a run each, and
    /// another where a camera changes its pictures partway.
    publisher: Publisher,
    state: Mutex<State>,
}

struct State {
    state: SourceState,
    since: SystemTime,
    error: Option<String>,
    connections: u64,
}

impl PullSource {
    /// A source for the channel called `name` pulling the camera at `url`,
    /// writing through `publisher`, nothing opened yet.
    fn new(name: &str, url: &str, config: &ChannelConfig, publisher: Publisher) -> Self {
        Self {
            inner: Arc::new(Inner {
                name: name.to_owned(),
                url: url.to_owned(),
                transport: match config.transport {
                    Transport::Tcp => RtspTransport::Tcp,
                    Transport::Udp => RtspTransport::Udp,
                },
                publisher,
                state: Mutex::new(State {
                    state: SourceState::Connecting,
                    since: SystemTime::now(),
                    error: None,
                    connections: 0,
                }),
            }),
            task: Task::new(),
        }
    }

    /// Starts pulling the camera on `runtime`, connecting again whenever it
    /// fails.
    pub(in crate::channel) fn run(&self, runtime: &Handle) {
        let inner = Arc::clone(&self.inner);
        self.task.spawn(runtime, |halt| inner.run(halt));
    }

    pub(in crate::channel) fn status(&self) -> SourceStatus {
        let state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
        SourceStatus {
            url: redact(&self.inner.url),
            state: state.state,
            since: state.since,
            error: state.error.clone(),
            connections: state.connections,
            client: (state.connections > 0).then_some("rtsp"),
        }
    }

    /// Tells its task to stop, without waiting: the task lets go of an open
    /// under way at once — it finishes on the blocking pool and is dropped
    /// — and stops the connection.
    pub(in crate::channel) fn halt(&self) {
        self.task.halt();
    }

    /// Halts it and waits for its task to end — no longer than a
    /// connection takes to stop: its threads joined, and a `TEARDOWN`
    /// written.
    pub(in crate::channel) async fn wait(&self) {
        self.task.wait().await;
    }
}

/// How a connection ended, as far as the task's loop cares.
enum Ended {
    /// Halted: the task ends.
    Halted,
    /// Failed, and why: the camera is connected again after a wait.
    Failed(String),
    /// Ended to be connected again at once.
    Reconnect,
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
            let ended = self.pull(&mut halt).await;
            self.publisher.ended();
            let reason = match ended {
                Ended::Halted => break,
                Ended::Failed(reason) => reason,
                Ended::Reconnect => {
                    tracing::info!(channel = %self.name, "connecting again, as asked");
                    continue;
                }
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
                () = halted(&mut halt) => break,
            }
            wait = (wait * 2).min(LONGEST_WAIT);
        }
    }

    /// One connection.
    async fn pull(self: &Arc<Self>, halt: &mut watch::Receiver<bool>) -> Ended {
        rtsp::pull(self, halt).await
    }

    fn set(&self, state: SourceState, error: Option<String>) {
        let mut current = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if current.state != state {
            current.since = SystemTime::now();
        }
        current.state = state;
        current.error = error;
    }

    /// A connection is live, its first run published.
    fn went_live(&self) {
        self.set(SourceState::Live, None);
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .connections += 1;
        tracing::info!(channel = %self.name, "live");
    }
}

/// Waits for `halt` to say halted. What `wait_for` gives back holds the
/// watch's lock, which a `select!` would keep across its other arms' awaits.
async fn halted(halt: &mut watch::Receiver<bool>) {
    let _ = halt.wait_for(|halted| *halted).await;
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
