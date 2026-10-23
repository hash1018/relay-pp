//! A channel's player sinks: the channel registered on one of the server's
//! listeners, which every channel shares — one port each, the channel
//! picked by the name in the URL. Registered when a player first asks for
//! the channel by that protocol, and kept until the channel goes; a WebRTC
//! registration moves to another broadcast where what WebRTC players read
//! changes — see `super::convert`.

use std::sync::Arc;

use media_pp::elements::{
    Broadcast, RtspPathMode, RtspServerHandle, WebRtcPathMode, WebRtcServerHandle,
};

/// The listeners every channel's sinks are registered on.
#[derive(Clone)]
pub(crate) struct Listeners {
    pub(crate) rtsp: RtspServerHandle,
    pub(crate) webrtc: WebRtcServerHandle,
}

/// Which listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Protocol {
    Rtsp,
    WebRtc,
}

impl Protocol {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Protocol::Rtsp => "rtsp",
            Protocol::WebRtc => "webrtc",
        }
    }
}

/// A channel registered on one listener, reading one broadcast.
pub(super) struct Sink {
    listener: Listener,
    /// The name of the broadcast it reads.
    reads: Arc<str>,
}

enum Listener {
    Rtsp(RtspServerHandle),
    WebRtc(WebRtcServerHandle),
}

impl Sink {
    /// Registers the channel called `name`, reading `broadcast`, on
    /// `protocol`'s listener.
    pub(super) fn attach(
        protocol: Protocol,
        name: &str,
        broadcast: &Broadcast,
        listeners: &Listeners,
    ) -> Result<Sink, String> {
        let listener = match protocol {
            Protocol::Rtsp => listeners
                .rtsp
                .add_path(name, broadcast, RtspPathMode::Play)
                .map(|()| Listener::Rtsp(listeners.rtsp.clone()))
                .map_err(|error| error.to_string()),
            Protocol::WebRtc => listeners
                .webrtc
                .add_path(name, broadcast, WebRtcPathMode::Play)
                .map(|()| Listener::WebRtc(listeners.webrtc.clone()))
                .map_err(|error| error.to_string()),
        }?;
        Ok(Sink {
            listener,
            reads: broadcast.name(),
        })
    }

    pub(super) fn protocol(&self) -> Protocol {
        match self.listener {
            Listener::Rtsp(_) => Protocol::Rtsp,
            Listener::WebRtc(_) => Protocol::WebRtc,
        }
    }

    /// Whether it reads `broadcast`.
    pub(super) fn reads(&self, broadcast: &Broadcast) -> bool {
        self.reads == broadcast.name()
    }

    /// Takes the channel off the listener: no player opens it from here.
    /// Those playing it go on until its broadcast closes.
    pub(super) fn detach(&self, name: &str) {
        match &self.listener {
            Listener::Rtsp(listener) => {
                listener.remove_path(name);
            }
            Listener::WebRtc(listener) => {
                listener.remove_path(name);
            }
        }
    }
}
