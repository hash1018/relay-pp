//! A channel's source: what takes its stream in and writes it into the
//! channel's hub, through its [`Publisher`].
//!
//! The source here is a camera pulled over RTSP, connected again whenever
//! it drops. Each connection is made by one of two clients: `retina`'s, a
//! task on the runtime ([`retina`]), where it takes the camera — most do —
//! and FFmpeg's, media-pp's `RtspSource` in a pipeline of its own
//! ([`ffmpeg`]), where it does not. Both publish each connection's tracks
//! to the hub and write them into what it gives back, which outlives every
//! connection; what reads the hub never knows which.
//!
//! What watches a connection — waits on it, waits before connecting again,
//! hears a halt — is the source's task, holding no thread while it waits.

mod ffmpeg;
mod retina;

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use media_pp::elements::RtspTransport;
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;
use tokio::sync::watch;

use super::hub::Publisher;
use super::task::Task;
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

/// Which RTSP client a channel's camera is pulled with — the body of
/// `POST /api/v1/channels`' `client`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Client {
    /// `retina`'s where it takes the camera, FFmpeg's where it does not:
    /// `rtsps://`, RTP over UDP, a codec other than H.264, H.265, AAC and
    /// G.711, a camera that gives no `rtptime` to line its tracks up by, or
    /// pictures with B-frames.
    #[default]
    Auto,
    /// FFmpeg's, always — for a camera `retina` takes but takes wrongly.
    Ffmpeg,
}

/// Which client made a connection, as the status names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Puller {
    Retina,
    Ffmpeg,
}

impl Puller {
    fn name(self) -> &'static str {
        match self {
            Puller::Retina => "retina",
            Puller::Ffmpeg => "ffmpeg",
        }
    }
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
    /// The client of the connection live, or the last.
    pub(in crate::channel) client: Option<&'static str>,
}

/// A camera pulled by a task, into a hub.
pub(in crate::channel) struct Source {
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
    /// The client asked for.
    client: Client,
    /// Set once `retina` has said it cannot take this camera: FFmpeg's
    /// client pulls it from then on, without asking again.
    ffmpeg_only: AtomicBool,
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
    client: Option<Puller>,
}

impl Source {
    /// A source for the channel called `name`, writing through `publisher`,
    /// nothing opened yet.
    pub(in crate::channel) fn new(
        name: &str,
        config: &ChannelConfig,
        publisher: Publisher,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                name: name.to_owned(),
                url: config.source.clone(),
                transport: match config.transport {
                    Transport::Tcp => RtspTransport::Tcp,
                    Transport::Udp => RtspTransport::Udp,
                },
                client: config.client,
                ffmpeg_only: AtomicBool::new(false),
                publisher,
                state: Mutex::new(State {
                    state: SourceState::Connecting,
                    since: SystemTime::now(),
                    error: None,
                    connections: 0,
                    client: None,
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
            client: state.client.map(Puller::name),
        }
    }

    /// Tells its task to stop, without waiting: the task lets go of an open
    /// under way at once — FFmpeg's finishes on the blocking pool and is
    /// dropped — and stops the connection.
    pub(in crate::channel) fn halt(&self) {
        self.task.halt();
    }

    /// Halts it and waits for its task to end — no longer than a
    /// connection takes to stop, which for FFmpeg's of a camera gone silent
    /// is its timeout.
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

    /// One connection, by `retina`'s client where it may take the camera,
    /// and by FFmpeg's where it may not or says it cannot.
    async fn pull(self: &Arc<Self>, halt: &mut watch::Receiver<bool>) -> Ended {
        let retina_may = self.client == Client::Auto
            && self.transport == RtspTransport::Tcp
            && !self.ffmpeg_only.load(Ordering::Acquire);
        if retina_may {
            match retina::pull(self, halt).await {
                retina::Pulled::Ended(ended) => return ended,
                retina::Pulled::Unsupported(why) => {
                    tracing::info!(
                        channel = %self.name,
                        "retina does not take this camera ({why}); FFmpeg's client pulls it"
                    );
                    self.ffmpeg_only.store(true, Ordering::Release);
                    // What retina may have had live has ended: nothing waits
                    // on it while FFmpeg's client opens the camera.
                    self.publisher.ended();
                    self.set(SourceState::Connecting, None);
                }
            }
        }
        ffmpeg::pull(self, halt).await
    }

    fn set(&self, state: SourceState, error: Option<String>) {
        let mut current = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if current.state != state {
            current.since = SystemTime::now();
        }
        current.state = state;
        current.error = error;
    }

    /// A connection by `client` is live, its first run published.
    fn went_live(&self, client: Puller) {
        self.set(SourceState::Live, None);
        {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state.connections += 1;
            state.client = Some(client);
        }
        tracing::info!(channel = %self.name, client = client.name(), "live");
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
