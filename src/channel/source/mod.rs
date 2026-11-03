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
//!
//! A camera pulled on demand ([`Demand`]) is opened once as the channel is
//! made, to say at once whether it answers, and connected only while the
//! channel says something wants it: let go of once nothing has for its
//! `close_after`, and not connected again after a failure unless something
//! still does — a camera that is off is not tried for ever for nobody.

mod publish;
mod rtsp;

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use media_pp::elements::RtspTransport;
use serde::Serialize;
use tokio::runtime::Handle;
use tokio::sync::{Notify, watch};

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
/// How often an on-demand camera looks at whether it is wanted, besides
/// being told that something is coming.
const WANT_CHECK: Duration = Duration::from_secs(1);

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
    /// Pulled on demand, and nothing wants it: not connected.
    Idle,
}

/// What a camera pulled on demand is told by its channel.
pub(in crate::channel) struct Demand {
    /// Whether anything wants the camera now: a player, a recording, HLS,
    /// or a request waiting for it to be live.
    pub(in crate::channel) wanted: Box<dyn Fn() -> bool + Send + Sync>,
    /// Rung as a request begins to wait for it, so that it connects without
    /// waiting for its next look.
    pub(in crate::channel) coming: Arc<Notify>,
    /// How long it stays connected once nothing wants it.
    pub(in crate::channel) close_after: Duration,
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
    /// through `publisher`: a camera, nothing opened yet — connected only
    /// as `demand` says, where it is given — or a publisher, its server
    /// already taking one.
    pub(in crate::channel) fn new(
        name: &str,
        config: &ChannelConfig,
        publisher: Publisher,
        listeners: &Listeners,
        demand: Option<Demand>,
    ) -> Result<Self, String> {
        match (&config.source, config.publish) {
            (Some(url), None) => Ok(Source::Pull(PullSource::new(
                name, url, config, publisher, demand,
            ))),
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
    /// Where it is pulled on demand: what says whether it is wanted.
    demand: Option<Demand>,
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
    fn new(
        name: &str,
        url: &str,
        config: &ChannelConfig,
        publisher: Publisher,
        demand: Option<Demand>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                name: name.to_owned(),
                url: url.to_owned(),
                transport: match config.transport {
                    Transport::Tcp => RtspTransport::Tcp,
                    Transport::Udp => RtspTransport::Udp,
                },
                publisher,
                demand,
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
    /// fails — on demand, opening it once and connecting while it is
    /// wanted.
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
    /// Let go of: nothing has wanted it for its `close_after`.
    Unwanted,
}

impl Inner {
    /// The source's task: pulls the camera, and connects again after a wait
    /// whenever it fails, until it is halted. On demand, only while it is
    /// wanted.
    async fn run(self: Arc<Self>, mut halt: watch::Receiver<bool>) {
        let mut wait = FIRST_WAIT;
        // Why it last failed, kept while it connects again.
        let mut last = None;
        // When it was last wanted, on demand.
        let mut wanted = Instant::now();
        if let Some(demand) = &self.demand {
            self.set(SourceState::Connecting, None);
            let answered = tokio::select! {
                answered = rtsp::probe(&self) => answered,
                () = halted(&mut halt) => return,
            };
            match &answered {
                Ok(()) => {
                    tracing::info!(channel = %self.name, "the camera answers; connected when wanted");
                }
                Err(reason) => {
                    tracing::warn!(channel = %self.name, "{reason}; tried again when wanted");
                }
            }
            last = answered.err();
            if !(demand.wanted)() && self.idle(last.clone(), &mut halt).await {
                return;
            }
            wanted = Instant::now();
        }
        loop {
            self.set(SourceState::Connecting, last.clone());
            let began = Instant::now();
            let ended = self.pull(&mut halt, &mut wanted).await;
            self.publisher.ended();
            let reason = match ended {
                Ended::Halted => break,
                Ended::Failed(reason) => reason,
                Ended::Reconnect => {
                    tracing::info!(channel = %self.name, "connecting again, as asked");
                    continue;
                }
                Ended::Unwanted => {
                    tracing::info!(channel = %self.name, "nothing wants it; let go of");
                    wait = FIRST_WAIT;
                    last = None;
                    if self.idle(None, &mut halt).await {
                        break;
                    }
                    wanted = Instant::now();
                    continue;
                }
            };
            if began.elapsed() >= STAYED_UP {
                wait = FIRST_WAIT;
            }
            last = Some(reason.clone());
            // On demand, a camera nothing has wanted for its `close_after` is
            // not tried again until something does.
            if let Some(demand) = &self.demand
                && !(demand.wanted)()
                && wanted.elapsed() >= demand.close_after
            {
                tracing::warn!(channel = %self.name, "{reason}; tried again when wanted");
                wait = FIRST_WAIT;
                if self.idle(last.clone(), &mut halt).await {
                    break;
                }
                wanted = Instant::now();
                continue;
            }
            // Spread over ±20%, so cameras a network outage dropped together
            // are not all opened again in the same instant.
            let spread = wait.mul_f64(0.8 + 0.4 * unit_random());
            tracing::warn!(
                channel = %self.name,
                "{reason}; connecting again in {:.1}s",
                spread.as_secs_f64()
            );
            self.set(SourceState::Waiting, Some(reason));
            tokio::select! {
                () = tokio::time::sleep(spread) => {}
                () = halted(&mut halt) => break,
                () = self.unwanted(&mut wanted) => {
                    tracing::info!(channel = %self.name, "nothing wants it; not tried again until something does");
                    wait = FIRST_WAIT;
                    if self.idle(last.clone(), &mut halt).await {
                        break;
                    }
                    wanted = Instant::now();
                    continue;
                }
            }
            wait = (wait * 2).min(LONGEST_WAIT);
        }
    }

    /// One connection.
    async fn pull(
        self: &Arc<Self>,
        halt: &mut watch::Receiver<bool>,
        wanted: &mut Instant,
    ) -> Ended {
        rtsp::pull(self, halt, wanted).await
    }

    /// Idle, saying why it last failed, until something wants it — false
    /// then — or it is halted — true.
    async fn idle(&self, error: Option<String>, halt: &mut watch::Receiver<bool>) -> bool {
        let Some(demand) = &self.demand else {
            return false;
        };
        self.set(SourceState::Idle, error);
        loop {
            // Asked for before it is looked at, so a ring between the two is
            // not missed.
            let coming = demand.coming.notified();
            if (demand.wanted)() {
                return false;
            }
            tokio::select! {
                () = coming => {}
                () = tokio::time::sleep(WANT_CHECK) => {}
                () = halted(halt) => return true,
            }
        }
    }

    /// Waits until nothing has wanted it for its `close_after`, counted from
    /// `wanted` — moved on each time it is — or for ever where it is not
    /// pulled on demand.
    async fn unwanted(&self, wanted: &mut Instant) {
        let Some(demand) = &self.demand else {
            return std::future::pending().await;
        };
        loop {
            if (demand.wanted)() {
                *wanted = Instant::now();
            } else if wanted.elapsed() >= demand.close_after {
                return;
            }
            tokio::time::sleep(WANT_CHECK).await;
        }
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
