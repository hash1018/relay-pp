//! A channel whose stream is pushed to relay-pp: by OBS or an encoder over
//! RTMP or RTSP, or a browser over WHIP.
//!
//! A server every channel shares takes the publisher — media-pp's RTMP,
//! RTSP or WebRTC server, which does the protocol — and hands it out as a
//! source of what it sends, through the `Publishers` the channel took on it.
//! The source is wired into the channel's hub as a camera pulled is, a
//! pipeline of its own: a publisher arriving begins a run, and leaving ends
//! it. What reads the hub never knows a camera from a publisher. One
//! publisher at a time: a second is refused by the server while the first
//! publishes.

use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use media_pp::bus::BusEvent;
use media_pp::element::IntoSource;
use media_pp::elements::{Publishers, RtmpSource, RtspSource, StreamInfo, WebRtcSource};
use media_pp::pipeline::Pipeline;
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;
use tokio::sync::watch;

use super::{SourceState, SourceStatus, halted};
use crate::channel::hub::Publisher;
use crate::channel::sink::Listeners;
use crate::channel::task::Task;

/// What RTMP publishers publish under: `rtmp://<host>:1935/live/<channel>`
/// — the server `rtmp://<host>:1935/live` and the stream key `<channel>`,
/// as OBS asks for them.
pub(crate) const RTMP_APP: &str = "live/";

/// Which protocol a channel takes its publisher over — the body of
/// `POST /api/v1/channels`' `publish`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Publish {
    /// `ANNOUNCE` and `RECORD` to `rtsp://<host>:8554/<channel>`.
    Rtsp,
    /// `rtmp://<host>:1935/live/<channel>`.
    Rtmp,
    /// A WHIP offer to `http://<host>:8888/<channel>/whip`.
    Whip,
}

impl Publish {
    fn name(self) -> &'static str {
        match self {
            Publish::Rtsp => "rtsp",
            Publish::Rtmp => "rtmp",
            Publish::Whip => "whip",
        }
    }
}

/// The WebRTC server's path a WHIP publisher of `channel` publishes to:
/// apart from `channel`'s own, which its WebRTC players play, and no
/// channel's, a channel's name holding no `/`.
pub(crate) fn whip_path(channel: &str) -> String {
    format!("{channel}/whip")
}

/// A channel's publisher, taken over one protocol.
pub(in crate::channel) struct PublishSource {
    inner: Arc<Inner>,
    task: Task,
}

/// What the server hands a channel's publishers out through, by protocol.
enum Taken {
    Rtsp(Publishers<RtspSource>),
    Rtmp(Publishers<RtmpSource>),
    Whip(Publishers<WebRtcSource>),
}

struct Inner {
    name: String,
    protocol: Publish,
    publisher: Publisher,
    /// Where its publishers come from, until its task takes it: let go of
    /// as the source halts, which takes the channel off its server.
    taken: Mutex<Option<Taken>>,
    state: Mutex<State>,
}

struct State {
    state: SourceState,
    since: SystemTime,
    publications: u64,
}

impl PublishSource {
    /// A source for the channel called `name` taking a publisher over
    /// `protocol`, on that server from now on.
    pub(in crate::channel) fn new(
        name: &str,
        protocol: Publish,
        publisher: Publisher,
        listeners: &Listeners,
    ) -> Result<Self, String> {
        let taken = match protocol {
            Publish::Rtsp => listeners
                .rtsp
                .take_publishers(name)
                .map(Taken::Rtsp)
                .map_err(|error| error.to_string()),
            Publish::Rtmp => listeners
                .rtmp
                .take_publishers(&format!("{RTMP_APP}{name}"))
                .map(Taken::Rtmp)
                .map_err(|error| error.to_string()),
            Publish::Whip => listeners
                .webrtc
                .take_publishers(&whip_path(name))
                .map(Taken::Whip)
                .map_err(|error| error.to_string()),
        }
        .map_err(|error| format!("cannot take a publisher over {}: {error}", protocol.name()))?;
        Ok(Self {
            inner: Arc::new(Inner {
                name: name.to_owned(),
                protocol,
                publisher,
                taken: Mutex::new(Some(taken)),
                state: Mutex::new(State {
                    state: SourceState::AwaitingPublisher,
                    since: SystemTime::now(),
                    publications: 0,
                }),
            }),
            task: Task::new(),
        })
    }

    /// Starts taking publishers on `runtime`.
    pub(in crate::channel) fn run(&self, runtime: &Handle) {
        let Some(taken) = self
            .inner
            .taken
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        else {
            return;
        };
        let inner = Arc::clone(&self.inner);
        self.task.spawn(runtime, |halt| inner.run(taken, halt));
    }

    pub(in crate::channel) fn status(&self) -> SourceStatus {
        let state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
        SourceStatus {
            url: match self.inner.protocol {
                Publish::Rtsp => format!("rtsp publisher: {}", self.inner.name),
                Publish::Rtmp => format!("rtmp publisher: {RTMP_APP}{}", self.inner.name),
                Publish::Whip => format!("whip publisher: /{}/whip", self.inner.name),
            },
            state: state.state,
            since: state.since,
            error: None,
            connections: state.publications,
            client: Some(self.inner.protocol.name()),
        }
    }

    /// Takes the channel off its server — no publisher begins on it from
    /// here — and tells the task to stop, which ends one publishing.
    pub(in crate::channel) fn halt(&self) {
        drop(
            self.inner
                .taken
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take(),
        );
        self.task.halt();
    }

    pub(in crate::channel) async fn wait(&self) {
        self.task.wait().await;
    }
}

impl Inner {
    /// The source's task: each publisher, as it begins, written into the
    /// hub — a run each — until halted, when what it takes them through is
    /// let go of.
    async fn run(self: Arc<Self>, taken: Taken, mut halt: watch::Receiver<bool>) {
        match taken {
            Taken::Rtsp(publishers) => self.take(publishers, &mut halt).await,
            Taken::Rtmp(publishers) => self.take(publishers, &mut halt).await,
            Taken::Whip(publishers) => self.take(publishers, &mut halt).await,
        }
        self.publisher.ended();
    }

    async fn take<S, M>(&self, publishers: Publishers<S>, halt: &mut watch::Receiver<bool>)
    where
        S: IntoSource<M>,
    {
        loop {
            let next = tokio::select! {
                next = publishers.next_async() => next,
                () = halted(halt) => return,
            };
            // The server has stopped.
            let Some((source, streams)) = next else {
                return;
            };
            let pipeline = match self.start(source, &streams) {
                Ok(pipeline) => pipeline,
                Err(reason) => {
                    tracing::warn!(channel = %self.name, "a publisher's stream cannot be taken: {reason}");
                    continue;
                }
            };
            self.set(SourceState::Live, true);
            tracing::info!(channel = %self.name, protocol = self.protocol.name(), "a publisher began");
            let left = tokio::select! {
                reason = watch_bus(&pipeline) => Some(reason),
                () = halted(halt) => None,
            };
            // Stopping joins the pipeline's threads: not on the runtime's
            // own.
            let _ = tokio::task::spawn_blocking(move || pipeline.stop()).await;
            self.publisher.ended();
            self.set(SourceState::AwaitingPublisher, false);
            let Some(reason) = left else {
                return;
            };
            tracing::info!(channel = %self.name, "the publisher left: {reason}");
        }
    }

    /// Publishes `streams` to the hub and starts a pipeline writing
    /// `source` into them: one run.
    fn start<S, M>(&self, source: S, streams: &[StreamInfo]) -> Result<Arc<Pipeline>, String>
    where
        S: IntoSource<M>,
    {
        let sinks = self.publisher.publish(streams)?;
        let (pipeline, ()) =
            Pipeline::new(format!("{}-publisher", self.name), source, |source, ctx| {
                for (stream, sink) in streams.iter().zip(sinks) {
                    ctx.attach(source, stream.index(), ctx.branch().to(sink)?)?;
                }
                Ok(())
            })
            .map_err(|error| format!("cannot wire the publisher: {error}"))?;
        pipeline
            .run()
            .map_err(|error| format!("cannot start the publisher: {error}"))?;
        Ok(pipeline)
    }

    fn set(&self, now: SourceState, began: bool) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.state != now {
            state.since = SystemTime::now();
        }
        state.state = now;
        if began {
            state.publications += 1;
        }
    }
}

/// Why a publisher's pipeline ended, as its bus says — which it does only
/// for a failure or the publisher leaving, which ends its source.
async fn watch_bus(pipeline: &Pipeline) -> String {
    loop {
        match pipeline.bus().recv_async().await {
            Some(BusEvent::Error { error, .. }) => return format!("it failed: {error}"),
            Some(BusEvent::Finished) => return "it ended its stream".to_owned(),
            Some(_) => {}
            None => return "its pipeline ended".to_owned(),
        }
    }
}
