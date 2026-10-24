//! A channel's player sinks: the channel served on one of the server's
//! listeners, which every channel shares — one port each, the channel
//! picked by the name in the URL — what its hub gives it written into the
//! listener's path of the channel.
//!
//! Served when a player first asks for the channel by that protocol, and
//! kept until the channel goes. What the hub gives it is written into the
//! path by a forwarder of its own — a pipeline with no thread of its own,
//! polled on the runtime, of a reader of the broadcast into the path's
//! muxer — a run of the path each time the broadcast begins anew with
//! other tracks, and each time players come to a path that had none. With
//! no player it reads nothing: the path describes the last run it wrote,
//! which is what a player is told it plays, and the forwarder begins
//! another as the player is counted. So what it reads counts as read only
//! while the path is played, which is what the hub stops a conversion by.
//! Where what it reads changes — a conversion begins, or is no longer
//! needed — the forwarder is moved to the new broadcast, and its players go
//! on where the tracks allow; where it cannot be made, the channel is taken
//! off the listener until it can.

use std::future::poll_fn;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use media_pp::element::BoxSink;
use media_pp::elements::{
    Broadcast, BroadcastError, BroadcastSource, RtmpServerHandle, RtspServer, RtspServerHandle,
    RtspServerPath, StreamInfo, WebRtcServer, WebRtcServerHandle, WebRtcServerPath,
};
use media_pp::pipeline::{Driven, DrivenPipeline};
use tokio::runtime::Handle;
use tokio::sync::watch;

use super::Output;
use super::follow::{halted, wait_or_halted};
use crate::channel::hub::{Fed, Hub, Refusal, Subscription};
use crate::channel::task::Task;

/// How long a forwarder waits before it looks again for what it reads to
/// be published, or tries again to open the path's muxer.
const RETRY: Duration = Duration::from_millis(50);

/// The listeners every channel shares: its players' — RTSP's and
/// WebRTC's — and its publisher's, RTSP's, RTMP's or WebRTC's.
#[derive(Clone)]
pub(crate) struct Listeners {
    pub(crate) rtsp: RtspServerHandle,
    pub(crate) webrtc: WebRtcServerHandle,
    pub(crate) rtmp: RtmpServerHandle,
}

/// Why players cannot reach a channel over a listener yet.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Unready {
    /// What they read is not made yet: the source just gone live, or a
    /// conversion still beginning.
    #[error("not ready yet")]
    NotYet,
    /// What they read is not made, and why.
    #[error(transparent)]
    Refused(Refusal),
    /// The listener would not take the channel.
    #[error("the listener would not take it: {0}")]
    Listener(String),
}

/// A channel on one listener.
pub(crate) struct ListenerSink {
    registration: Arc<Registration>,
    subscription: Subscription,
    task: Task,
}

/// What the sink and its task share: the listener, and the channel's path
/// on it while it is served.
struct Registration {
    channel: String,
    listener: Listener,
    runtime: Handle,
    state: Mutex<Registered>,
    /// The broadcast whose run the path serves now, and each change of it.
    serving: watch::Sender<Option<Arc<str>>>,
}

#[derive(Default)]
struct Registered {
    served: Option<Served>,
    /// Taken off for good: nothing serves it again.
    detached: bool,
}

/// The channel's path on the listener, and what writes into it.
struct Served {
    path: Arc<ServedPath>,
    /// The name of the broadcast its forwarder reads.
    reads: Arc<str>,
    forwarder: Task,
}

impl Drop for Served {
    fn drop(&mut self) {
        self.forwarder.halt();
    }
}

enum Listener {
    Rtsp(RtspServerHandle),
    WebRtc(WebRtcServerHandle),
}

/// A listener's path of the channel.
enum ServedPath {
    Rtsp(RtspServerPath),
    WebRtc(WebRtcServerPath),
}

impl ServedPath {
    /// How many players play it now.
    fn players(&self) -> usize {
        match self {
            ServedPath::Rtsp(path) => path.players(),
            ServedPath::WebRtc(path) => path.players(),
        }
    }

    /// Waits until other than `from` players play it: how many do.
    async fn players_change(&self, from: usize) -> usize {
        match self {
            ServedPath::Rtsp(path) => path.players_change(from).await,
            ServedPath::WebRtc(path) => path.players_change(from).await,
        }
    }

    /// Opens a run of the path with what of `streams` its listener carries,
    /// and answers each one's output and sink.
    fn open(&self, streams: &[StreamInfo]) -> Result<Vec<(usize, BoxSink)>, String> {
        let opened = match self {
            ServedPath::Rtsp(path) => {
                let mut muxer = path.muxer();
                let carried: Vec<&StreamInfo> = streams
                    .iter()
                    .filter(|stream| RtspServer::carries(stream))
                    .collect();
                muxed(&carried, |name, stream| muxer.add_stream(name, stream))
                    .and_then(|tracks| Ok((tracks, muxer.open()?)))
            }
            ServedPath::WebRtc(path) => {
                let mut muxer = path.muxer();
                let carried: Vec<&StreamInfo> = streams
                    .iter()
                    .filter(|stream| WebRtcServer::carries(stream))
                    .collect();
                muxed(&carried, |name, stream| muxer.add_stream(name, stream))
                    .and_then(|tracks| Ok((tracks, muxer.open()?)))
            }
        };
        let (tracks, mut sinks) = opened.map_err(|error| error.to_string())?;
        tracks
            .into_iter()
            .map(|(index, track)| {
                Ok((index, sinks.take(track).map_err(|error| error.to_string())?))
            })
            .collect()
    }
}

/// Each of `streams` added by `add`, with its output.
fn muxed<T>(
    streams: &[&StreamInfo],
    mut add: impl FnMut(String, &StreamInfo) -> media_pp::Result<T>,
) -> media_pp::Result<Vec<(usize, T)>> {
    streams
        .iter()
        .map(|stream| {
            Ok((
                stream.index(),
                add(format!("track-{}", stream.index()), stream)?,
            ))
        })
        .collect()
}

impl ListenerSink {
    /// The channel called `channel` on `output`'s listener — RTSP's or
    /// WebRTC's — reading what `hub` gives it, followed on `runtime`;
    /// `None` for an output no listener serves.
    pub(crate) fn attach(
        output: Output,
        channel: &str,
        hub: &Hub,
        listeners: &Listeners,
        runtime: &Handle,
    ) -> Option<Self> {
        let listener = match output {
            Output::Rtsp => Listener::Rtsp(listeners.rtsp.clone()),
            Output::WebRtc => Listener::WebRtc(listeners.webrtc.clone()),
            Output::Record | Output::Hls => return None,
        };
        let registration = Arc::new(Registration {
            channel: channel.to_owned(),
            listener,
            runtime: runtime.clone(),
            state: Mutex::default(),
            serving: watch::Sender::new(None),
        });
        let subscription = hub.subscribe(output.profile(), output.demand());
        let task = Task::new();
        let following = Arc::clone(&registration);
        let fed = subscription.fed();
        task.spawn(runtime, |halt| following.follow_each(fed, halt));
        Some(Self {
            registration,
            subscription,
            task,
        })
    }

    /// Waits, up to `within`, for what it reads to be made — at once where
    /// it is — and for the channel's path to be serving it; or says why it
    /// cannot be. What it reads is started again first, where it had
    /// stopped for want of players.
    pub(crate) async fn ready(&self, within: Duration) -> Result<(), Unready> {
        let until = tokio::time::Instant::now() + within;
        self.subscription.wake();
        let mut fed = self.subscription.fed();
        let made = tokio::time::timeout_at(until, fed.wait_for(|fed| *fed != Fed::Idle))
            .await
            .ok()
            .and_then(Result::ok)
            .map(|fed| fed.clone());
        let name = match made {
            Some(Fed::Live(feed)) => {
                self.registration
                    .follow(&Fed::Live(feed.clone()))
                    .map_err(Unready::Listener)?;
                feed.broadcast.name()
            }
            Some(Fed::Failed(refusal)) => return Err(Unready::Refused(refusal)),
            _ => return Err(Unready::NotYet),
        };
        let mut serving = self.registration.serving.subscribe();
        tokio::time::timeout_at(
            until,
            serving.wait_for(|serving| serving.as_deref() == Some(&*name)),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .map(|_| ())
        .ok_or(Unready::NotYet)
    }

    /// How many players play the channel over the listener now.
    pub(crate) fn players(&self) -> usize {
        self.registration.players()
    }

    /// Takes the channel off the listener for good: no player opens it from
    /// here, and those playing it end.
    pub(crate) fn detach(&self) {
        self.task.halt();
        self.registration.detach();
    }
}

impl Registration {
    /// Follows `fed` until halted.
    async fn follow_each(
        self: Arc<Self>,
        mut fed: watch::Receiver<Fed>,
        mut halt: watch::Receiver<bool>,
    ) {
        loop {
            let now = fed.borrow_and_update().clone();
            if let Err(reason) = self.follow(&now) {
                tracing::warn!(channel = %self.channel, "{reason}");
            }
            tokio::select! {
                changed = fed.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                () = halted(&mut halt) => break,
            }
        }
    }

    /// Has the channel's path serving what `fed` says, unless it is: the
    /// broadcast it says, where it is live; nothing, where what it needs
    /// cannot be made; as it was, where the source is not live.
    fn follow(&self, fed: &Fed) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.detached {
            return Ok(());
        }
        match fed {
            Fed::Live(feed) => {
                let name = feed.broadcast.name();
                if state
                    .served
                    .as_ref()
                    .is_some_and(|served| served.reads == name)
                {
                    return Ok(());
                }
                // The path goes on, its players with it: only what writes
                // into it changes.
                let path = match state.served.take() {
                    Some(served) => Arc::clone(&served.path),
                    None => Arc::new(self.path()?),
                };
                self.serving.send_replace(None);
                let forwarder = Task::new();
                forwarder.spawn(&self.runtime, {
                    let path = Arc::clone(&path);
                    let broadcast = feed.broadcast.clone();
                    let serving = self.serving.clone();
                    let channel = self.channel.clone();
                    let kind = self.kind();
                    move |halt| forward(channel, kind, path, broadcast, serving, halt)
                });
                tracing::info!(channel = %self.channel, listener = self.kind(), broadcast = %name, "streamed");
                state.served = Some(Served {
                    path,
                    reads: name,
                    forwarder,
                });
            }
            Fed::Failed(_) => {
                if state.served.take().is_some() {
                    self.serving.send_replace(None);
                }
            }
            Fed::Idle => {}
        }
        Ok(())
    }

    fn detach(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.detached = true;
        state.served = None;
        self.serving.send_replace(None);
    }

    /// The channel's path on the listener.
    fn path(&self) -> Result<ServedPath, String> {
        match &self.listener {
            Listener::Rtsp(listener) => listener
                .path(&self.channel)
                .map(ServedPath::Rtsp)
                .map_err(|error| error.to_string()),
            Listener::WebRtc(listener) => listener
                .path(&self.channel)
                .map(ServedPath::WebRtc)
                .map_err(|error| error.to_string()),
        }
    }

    fn players(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .served
            .as_ref()
            .map_or(0, |served| served.path.players())
    }

    fn kind(&self) -> &'static str {
        match self.listener {
            Listener::Rtsp(_) => "rtsp",
            Listener::WebRtc(_) => "webrtc",
        }
    }
}

/// Writes what `broadcast` carries into `path` while it is played, until
/// halted: a run of the path each time the broadcast begins with tracks the
/// last could not go on with, and each time players come to it again. The
/// first run is begun at once, unplayed, for the path to describe — nobody
/// plays a path that describes nothing. Says on `serving` which broadcast
/// the path describes a run of.
async fn forward(
    channel: String,
    kind: &'static str,
    path: Arc<ServedPath>,
    broadcast: Broadcast,
    serving: watch::Sender<Option<Arc<str>>>,
    mut halt: watch::Receiver<bool>,
) {
    let name = format!("{channel}-to-{kind}");
    let mut described = false;
    loop {
        if described {
            tokio::select! {
                _ = path.players_change(0) => {}
                () = halted(&mut halt) => return,
            }
        }
        let (source, streams) = match BroadcastSource::new(name.clone(), &broadcast) {
            Ok(opened) => opened,
            Err(BroadcastError::NotPublished(_)) => {
                if wait_or_halted(RETRY, &mut halt).await {
                    return;
                }
                continue;
            }
            Err(_) => return,
        };
        let sinks = match path.open(&streams) {
            Ok(sinks) => sinks,
            Err(reason) => {
                tracing::warn!(channel = %channel, listener = kind, "cannot serve the channel: {reason}");
                if wait_or_halted(Duration::from_secs(1), &mut halt).await {
                    return;
                }
                continue;
            }
        };
        let wired = DrivenPipeline::new(name.clone(), source, |source, ctx| {
            for (index, sink) in sinks {
                ctx.attach(source, index, ctx.branch().to(sink)?)?;
            }
            Ok(())
        });
        let mut pipeline = match wired {
            Ok((pipeline, ())) => pipeline,
            Err(error) => {
                tracing::warn!(channel = %channel, listener = kind, "cannot wire what serves the channel: {error}");
                return;
            }
        };
        serving.send_replace(Some(broadcast.name()));
        described = true;
        // Polled as what it waits on wakes it, and at the time it asks for.
        let finished = poll_fn(|cx| match pipeline.poll(cx) {
            Driven::Finished => Poll::Ready(()),
            Driven::Pending { at } => {
                if let Some(at) = at {
                    let waker = cx.waker().clone();
                    tokio::spawn(async move {
                        tokio::time::sleep_until(at.into()).await;
                        waker.wake();
                    });
                }
                Poll::Pending
            }
        });
        // The run ends with its last player: the pipeline goes as the loop
        // goes round.
        let unplayed = async {
            let mut players = path.players();
            while players > 0 {
                players = path.players_change(players).await;
            }
        };
        tokio::select! {
            () = finished => {}
            () = unplayed => {}
            () = halted(&mut halt) => return,
        }
    }
}
