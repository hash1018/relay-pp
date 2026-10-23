//! A channel whose stream is pushed to relay-pp: by OBS or an encoder over
//! RTMP or RTSP, or a browser over WHIP.
//!
//! A server every channel shares takes the publisher — media-pp's RTMP,
//! RTSP or WebRTC server, which does the protocol — and writes what it
//! sends into a broadcast of this source's own, its *intake*, registered on
//! the server as where a publisher of the channel goes. The source reads
//! the intake and writes what it reads into the channel's hub, as a camera
//! pulled is: a publisher arriving begins a run, and leaving ends it. What
//! reads the hub never knows a camera from a publisher. One publisher at a
//! time: a second is refused by the server while the first publishes.

use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use media_pp::buffer::MediaBuffer;
use media_pp::element::RawSink;
use media_pp::elements::{
    Broadcast, BroadcastItem, BroadcastOptions, BroadcastSink, WebRtcPathMode,
};
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

struct Inner {
    name: String,
    protocol: Publish,
    /// What the server writes a publisher's stream into.
    intake: Broadcast,
    publisher: Publisher,
    listeners: Listeners,
    state: Mutex<State>,
}

struct State {
    state: SourceState,
    since: SystemTime,
    publications: u64,
}

impl PublishSource {
    /// A source for the channel called `name` taking a publisher over
    /// `protocol`, its intake registered on that server from now on.
    pub(in crate::channel) fn new(
        name: &str,
        protocol: Publish,
        publisher: Publisher,
        listeners: &Listeners,
    ) -> Result<Self, String> {
        let intake = Broadcast::new(format!("{name}@intake"), BroadcastOptions::default());
        let registered = match protocol {
            Publish::Rtsp => listeners
                .rtsp
                .add_publish_path(name, &intake)
                .map_err(|error| error.to_string()),
            Publish::Rtmp => listeners
                .rtmp
                .add_path(&format!("{RTMP_APP}{name}"), &intake)
                .map_err(|error| error.to_string()),
            Publish::Whip => listeners
                .webrtc
                .add_path(&whip_path(name), &intake, WebRtcPathMode::Publish)
                .map_err(|error| error.to_string()),
        };
        registered.map_err(|error| {
            format!("cannot take a publisher over {}: {error}", protocol.name())
        })?;
        Ok(Self {
            inner: Arc::new(Inner {
                name: name.to_owned(),
                protocol,
                intake,
                publisher,
                listeners: listeners.clone(),
                state: Mutex::new(State {
                    state: SourceState::AwaitingPublisher,
                    since: SystemTime::now(),
                    publications: 0,
                }),
            }),
            task: Task::new(),
        })
    }

    /// Starts reading the intake on `runtime`.
    pub(in crate::channel) fn run(&self, runtime: &Handle) {
        let inner = Arc::clone(&self.inner);
        self.task.spawn(runtime, |halt| inner.run(halt));
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
    /// here, and one publishing goes on until it ends — closes the intake,
    /// and tells the task to stop.
    pub(in crate::channel) fn halt(&self) {
        let inner = &self.inner;
        match inner.protocol {
            Publish::Rtsp => {
                inner.listeners.rtsp.remove_publish_path(&inner.name);
            }
            Publish::Rtmp => {
                inner
                    .listeners
                    .rtmp
                    .remove_path(&format!("{RTMP_APP}{}", inner.name));
            }
            Publish::Whip => {
                inner.listeners.webrtc.remove_path(&whip_path(&inner.name));
            }
        }
        inner.intake.close();
        self.task.halt();
    }

    pub(in crate::channel) async fn wait(&self) {
        self.task.wait().await;
    }
}

impl Inner {
    /// The source's task: what the intake is written, written into the hub,
    /// each publisher a run, until halted.
    async fn run(self: Arc<Self>, mut halt: watch::Receiver<bool>) {
        let mut reader = self.intake.subscribe();
        let mut sinks: Vec<BroadcastSink> = Vec::new();
        loop {
            let item = tokio::select! {
                item = reader.recv() => item,
                () = halted(&mut halt) => break,
            };
            match item {
                Some(BroadcastItem::Started(tracks)) => match self.publisher.publish(&tracks) {
                    Ok(published) => {
                        sinks = published;
                        self.set(SourceState::Live, true);
                        tracing::info!(channel = %self.name, protocol = self.protocol.name(), "a publisher began");
                    }
                    Err(reason) => {
                        sinks.clear();
                        tracing::warn!(channel = %self.name, "a publisher's stream cannot be taken: {reason}");
                    }
                },
                Some(BroadcastItem::Packet { track, packet, .. }) => {
                    if let Some(sink) = sinks.get_mut(track) {
                        // A packet the hub refuses — its run superseded — is
                        // one the next run does without.
                        let _ = sink.consume(MediaBuffer::Packet(packet));
                    }
                }
                Some(BroadcastItem::Skipped) => {}
                Some(BroadcastItem::Ended) => {
                    sinks.clear();
                    self.publisher.ended();
                    self.set(SourceState::AwaitingPublisher, false);
                    tracing::info!(channel = %self.name, "the publisher left");
                }
                Some(BroadcastItem::Closed) | None => break,
            }
        }
        self.publisher.ended();
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
