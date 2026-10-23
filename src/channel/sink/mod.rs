//! A channel's sinks: what reads its hub, each in a form it carries.
//!
//! RTSP and WebRTC players are reached through a listener every channel
//! shares, the channel registered on it reading the broadcast its hub says
//! ([`listener`]); a recording is a recorder of the channel's own
//! ([`record`]). Each says what it carries as a [`Profile`] — what its
//! media-pp server or muxer sends, narrowed to what its players play — and
//! reads what the hub gives it for that: none knows what the channel's
//! source sends, nor whether what it reads is converted.

mod listener;
mod record;

use media_pp::elements::{HlsMuxer, HlsSegmentFormat, RtspServer, StreamInfo, WebRtcServer};
use media_pp::ffmpeg::codec::Id;
use media_pp::ffmpeg::media;

use super::hub::{Demand, Profile, Target};
pub(crate) use listener::{ListenerSink, Listeners};
pub(crate) use record::{RecordStatus, Recorder};

/// A kind of sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Output {
    /// RTSP players, through the RTSP listener.
    Rtsp,
    /// WebRTC players, through WHEP and the WebRTC listener.
    WebRtc,
    /// The channel's recording.
    Record,
}

impl Output {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Output::Rtsp => "rtsp",
            Output::WebRtc => "webrtc",
            Output::Record => "record",
        }
    }

    /// What it carries, and what it is given in place of sound it does not.
    pub(crate) fn profile(self) -> Profile {
        match self {
            Output::Rtsp => Profile {
                name: "rtsp",
                carries: rtsp_carries,
                sound: Target::Aac,
            },
            Output::WebRtc => Profile {
                name: "webrtc",
                carries: webrtc_carries,
                sound: Target::Opus,
            },
            Output::Record => Profile {
                name: "record",
                carries: record_carries,
                sound: Target::Aac,
            },
        }
    }

    /// For how long what it needs is made: for WebRTC players, who ask over
    /// WHEP before they play, while they play; for RTSP players, who reach
    /// the listener without asking, and a recording, throughout.
    pub(crate) fn demand(self) -> Demand {
        match self {
            Output::WebRtc => Demand::WhileRead,
            Output::Rtsp | Output::Record => Demand::Always,
        }
    }
}

/// What the RTSP server sends: any player takes what a camera sends.
fn rtsp_carries(track: &StreamInfo) -> bool {
    RtspServer::carries(track)
}

/// What the WebRTC server sends and a browser plays: no H.265, which most
/// do not, and no pictures out of the order they are shown in.
fn webrtc_carries(track: &StreamInfo) -> bool {
    let played = match track.kind() {
        media::Type::Video => {
            matches!(
                track.parameters().id(),
                Id::H264 | Id::VP8 | Id::VP9 | Id::AV1
            ) && !track.reorders()
        }
        _ => true,
    };
    played && WebRtcServer::carries(track)
}

/// What HLS of fMP4 holds and an HLS player plays: H.264 or H.265
/// pictures, AAC sound.
fn record_carries(track: &StreamInfo) -> bool {
    matches!(track.parameters().id(), Id::H264 | Id::HEVC | Id::AAC)
        && HlsMuxer::carries(track, HlsSegmentFormat::Fmp4)
}

#[cfg(test)]
mod tests {
    use media_pp::ffmpeg::Rational;

    use super::*;

    fn video(codec: Id) -> StreamInfo {
        StreamInfo::video(codec, &[], (320, 240), Rational::new(1, 90_000)).unwrap()
    }

    fn audio(codec: Id) -> StreamInfo {
        StreamInfo::audio(codec, &[], 48_000, 1, Rational::new(1, 48_000)).unwrap()
    }

    /// H.264 with B-frames, as probing a stream of them says.
    fn reordering() -> StreamInfo {
        let described = video(Id::H264);
        let mut parameters = described.parameters().clone();
        // SAFETY: a plain field of parameters this test owns.
        unsafe { (*parameters.as_mut_ptr()).video_delay = 2 };
        StreamInfo::new(parameters, described.time_base())
    }

    fn carried(output: Output, track: &StreamInfo) -> bool {
        (output.profile().carries)(track)
    }

    #[test]
    fn rtsp_carries_what_rtp_does() {
        for track in [
            video(Id::H264),
            video(Id::HEVC),
            audio(Id::AAC),
            audio(Id::OPUS),
            reordering(),
        ] {
            assert!(
                carried(Output::Rtsp, &track),
                "{:?}",
                track.parameters().id()
            );
        }
        for track in [video(Id::AV1), audio(Id::AC3)] {
            assert!(
                !carried(Output::Rtsp, &track),
                "{:?}",
                track.parameters().id()
            );
        }
    }

    #[test]
    fn webrtc_carries_what_a_browser_plays() {
        for track in [
            video(Id::H264),
            video(Id::VP8),
            audio(Id::OPUS),
            audio(Id::PCM_ALAW),
        ] {
            assert!(
                carried(Output::WebRtc, &track),
                "{:?}",
                track.parameters().id()
            );
        }
        for track in [video(Id::HEVC), audio(Id::AAC), video(Id::MJPEG)] {
            assert!(
                !carried(Output::WebRtc, &track),
                "{:?}",
                track.parameters().id()
            );
        }
        assert!(!carried(Output::WebRtc, &reordering()), "B-frames");
    }

    #[test]
    fn a_recording_carries_what_an_hls_player_plays() {
        for track in [
            video(Id::H264),
            video(Id::HEVC),
            audio(Id::AAC),
            reordering(),
        ] {
            assert!(
                carried(Output::Record, &track),
                "{:?}",
                track.parameters().id()
            );
        }
        for track in [audio(Id::OPUS), audio(Id::PCM_MULAW), video(Id::VP8)] {
            assert!(
                !carried(Output::Record, &track),
                "{:?}",
                track.parameters().id()
            );
        }
    }
}
