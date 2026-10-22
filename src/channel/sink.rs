//! A channel's sinks: what hands its broadcast out.
//!
//! Each is the channel registered on one of the server's listeners, which
//! every channel shares: one port each, the channel picked by the name in
//! the URL.

use media_pp::elements::{
    Broadcast, RtspPathMode, RtspServerHandle, WebRtcPathMode, WebRtcServerHandle,
};

/// The listeners every channel's sinks are registered on.
#[derive(Clone)]
pub(crate) struct Listeners {
    pub(crate) rtsp: RtspServerHandle,
    pub(crate) webrtc: WebRtcServerHandle,
}

/// One of a channel's sinks.
pub(super) enum Sink {
    /// The channel registered on the RTSP listener.
    Rtsp(RtspServerHandle),
    /// The channel registered on the WebRTC listener.
    WebRtc(WebRtcServerHandle),
}

impl Sink {
    /// Every sink a channel has on `listeners`, reading `broadcast`. Where
    /// one cannot be attached, those already attached are detached again.
    pub(super) fn attach_all(
        name: &str,
        broadcast: &Broadcast,
        listeners: &Listeners,
    ) -> Result<Vec<Sink>, String> {
        let mut sinks = Vec::new();
        let attached = listeners
            .rtsp
            .add_path(name, broadcast, RtspPathMode::Play)
            .map(|()| sinks.push(Sink::Rtsp(listeners.rtsp.clone())))
            .map_err(|error| error.to_string())
            .and_then(|()| {
                listeners
                    .webrtc
                    .add_path(name, broadcast, WebRtcPathMode::Play)
                    .map(|()| sinks.push(Sink::WebRtc(listeners.webrtc.clone())))
                    .map_err(|error| error.to_string())
            });
        match attached {
            Ok(()) => Ok(sinks),
            Err(error) => {
                for sink in &sinks {
                    sink.detach(name);
                }
                Err(error)
            }
        }
    }

    pub(super) fn kind(&self) -> &'static str {
        match self {
            Sink::Rtsp(_) => "rtsp",
            Sink::WebRtc(_) => "webrtc",
        }
    }

    /// Takes the channel off the listener: no player opens it from here.
    pub(super) fn detach(&self, name: &str) {
        match self {
            Sink::Rtsp(listener) => {
                listener.remove_path(name);
            }
            Sink::WebRtc(listener) => {
                listener.remove_path(name);
            }
        }
    }
}
