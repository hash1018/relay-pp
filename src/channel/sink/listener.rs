//! A channel's player sinks: the channel registered on one of the server's
//! listeners, which every channel shares — one port each, the channel
//! picked by the name in the URL — reading the broadcast its hub gives it.
//!
//! Registered when a player first asks for the channel by that protocol,
//! and kept until the channel goes. Where what it reads changes — a
//! conversion begins, or is no longer needed — a task of its own moves the
//! registration to the new broadcast; where it cannot be made, the channel
//! is taken off the listener until it can. Players already playing go on
//! with what they had until it ends.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use media_pp::elements::{
    Broadcast, RtmpServerHandle, RtspPathMode, RtspServerHandle, WebRtcPathMode, WebRtcServerHandle,
};
use tokio::runtime::Handle;
use tokio::sync::watch;

use super::Output;
use crate::channel::hub::{Fed, Hub, Refusal, Subscription};
use crate::channel::task::Task;

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

/// What the sink and its task share: the listener, and what the channel is
/// registered on it reading.
struct Registration {
    channel: String,
    listener: Listener,
    state: Mutex<Registered>,
}

#[derive(Default)]
struct Registered {
    /// The name of the broadcast registered, while one is.
    reads: Option<Arc<str>>,
    /// Taken off for good: nothing registers it again.
    detached: bool,
}

enum Listener {
    Rtsp(RtspServerHandle),
    WebRtc(WebRtcServerHandle),
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
            Output::Record => return None,
        };
        let registration = Arc::new(Registration {
            channel: channel.to_owned(),
            listener,
            state: Mutex::default(),
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
    /// it is — and for the channel to be registered reading it; or says why
    /// it cannot be. What it reads is started again first, where it had
    /// stopped for want of players.
    pub(crate) async fn ready(&self, within: Duration) -> Result<(), Unready> {
        self.subscription.wake();
        let mut fed = self.subscription.fed();
        let made = tokio::time::timeout(within, fed.wait_for(|fed| *fed != Fed::Idle))
            .await
            .ok()
            .and_then(Result::ok)
            .map(|fed| fed.clone());
        match made {
            Some(fed @ Fed::Live(_)) => self.registration.follow(&fed).map_err(Unready::Listener),
            Some(Fed::Failed(refusal)) => Err(Unready::Refused(refusal)),
            _ => Err(Unready::NotYet),
        }
    }

    /// Takes the channel off the listener for good: no player opens it from
    /// here. Those playing it go on until their broadcast closes.
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
                _ = halt.wait_for(|halted| *halted) => break,
            }
        }
    }

    /// Has the channel registered reading what `fed` says, unless it is:
    /// the broadcast it says, where it is live; nothing, where what it
    /// needs cannot be made; as it was, where the source is not live.
    fn follow(&self, fed: &Fed) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.detached {
            return Ok(());
        }
        match fed {
            Fed::Live(feed) => {
                let name = feed.broadcast.name();
                if state.reads.as_deref() == Some(&*name) {
                    return Ok(());
                }
                if state.reads.take().is_some() {
                    self.remove();
                }
                self.add(&feed.broadcast)?;
                tracing::info!(channel = %self.channel, listener = self.kind(), broadcast = %name, "streamed");
                state.reads = Some(name);
            }
            Fed::Failed(_) => {
                if state.reads.take().is_some() {
                    self.remove();
                }
            }
            Fed::Idle => {}
        }
        Ok(())
    }

    fn detach(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.detached = true;
        if state.reads.take().is_some() {
            self.remove();
        }
    }

    fn add(&self, broadcast: &Broadcast) -> Result<(), String> {
        match &self.listener {
            Listener::Rtsp(listener) => listener
                .add_path(&self.channel, broadcast, RtspPathMode::Play)
                .map_err(|error| error.to_string()),
            Listener::WebRtc(listener) => listener
                .add_path(&self.channel, broadcast, WebRtcPathMode::Play)
                .map_err(|error| error.to_string()),
        }
    }

    fn remove(&self) {
        match &self.listener {
            Listener::Rtsp(listener) => {
                listener.remove_path(&self.channel);
            }
            Listener::WebRtc(listener) => {
                listener.remove_path(&self.channel);
            }
        }
    }

    fn kind(&self) -> &'static str {
        match self.listener {
            Listener::Rtsp(_) => "rtsp",
            Listener::WebRtc(_) => "webrtc",
        }
    }
}
