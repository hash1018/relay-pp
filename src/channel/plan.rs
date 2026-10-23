//! What a channel converts, and for which outputs: worked out from the
//! tracks its source sends and the outputs that read it, nothing run.
//!
//! An output carries some codecs and not others — RTSP whatever came in
//! over RTP, WebRTC what a browser plays, a recording what HLS players
//! play. Each reads one broadcast of tracks it all carries: the channel's
//! own where it carries every track, or one made for it, where each track
//! it cannot carry is converted. A conversion is of one track to one codec,
//! and is made once however many outputs read it; outputs that are to read
//! the same tracks read one broadcast.

use std::collections::{BTreeMap, BTreeSet};

use media_pp::elements::StreamInfo;
use media_pp::ffmpeg::codec::Id;
use media_pp::ffmpeg::media;

/// What reads a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Output {
    Rtsp,
    WebRtc,
    /// Its recording.
    Hls,
}

/// A track as an output sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Track {
    pub(crate) kind: Kind,
    pub(crate) codec: Id,
    /// Its pictures come out of the order they are shown in: B-frames.
    pub(crate) reorders: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Kind {
    Video,
    Audio,
}

/// A codec a track is converted to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Target {
    /// With no B-frames, which a browser needs.
    H264,
    Opus,
    Aac,
}

/// Where a track of a broadcast comes from: the source's own track of that
/// index, or that track converted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum TrackFrom {
    Original(usize),
    Converted(usize, Target),
}

/// What a channel runs for the outputs reading it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Each conversion, of a source track to a codec, once.
    pub(crate) conversions: BTreeSet<(usize, Target)>,
    /// The broadcasts read, by the tracks each has, and the outputs reading
    /// each. One of every track of the source, in order and unconverted, is
    /// the source's own broadcast.
    pub(crate) broadcasts: BTreeMap<Vec<TrackFrom>, BTreeSet<Output>>,
}

impl Track {
    /// What a source's track description says of it; `None` for one that
    /// is neither pictures nor sound, which no output reads.
    pub(crate) fn of(info: &StreamInfo) -> Option<Self> {
        let kind = match info.kind() {
            media::Type::Video => Kind::Video,
            media::Type::Audio => Kind::Audio,
            _ => return None,
        };
        Some(Self {
            kind,
            codec: info.parameters().id(),
            reorders: info.reorders(),
        })
    }
}

impl Output {
    /// Whether it carries `track` as it is.
    pub(crate) fn carries(self, track: &Track) -> bool {
        match (self, track.kind) {
            // What came in over RTP goes out over RTP.
            (Output::Rtsp, _) => true,
            (Output::WebRtc, Kind::Video) => {
                // H.265 only some browsers play: converted for every one.
                matches!(track.codec, Id::H264 | Id::VP8 | Id::VP9 | Id::AV1) && !track.reorders
            }
            (Output::WebRtc, Kind::Audio) => {
                matches!(track.codec, Id::OPUS | Id::PCM_MULAW | Id::PCM_ALAW)
            }
            (Output::Hls, Kind::Video) => matches!(track.codec, Id::H264 | Id::HEVC),
            (Output::Hls, Kind::Audio) => track.codec == Id::AAC,
        }
    }

    /// What a track of `kind` it does not carry is converted to.
    pub(crate) fn target(self, kind: Kind) -> Target {
        match (self, kind) {
            (_, Kind::Video) => Target::H264,
            (Output::Hls, Kind::Audio) => Target::Aac,
            (_, Kind::Audio) => Target::Opus,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Output::Rtsp => "rtsp",
            Output::WebRtc => "webrtc",
            Output::Hls => "record",
        }
    }
}

impl Plan {
    /// The plan for a source sending `tracks` — in its broadcast's order,
    /// `None` where no output reads one — read by `outputs`.
    pub(crate) fn of(tracks: &[Option<Track>], outputs: &BTreeSet<Output>) -> Self {
        let mut plan = Plan::default();
        for &output in outputs {
            let read: Vec<TrackFrom> = tracks
                .iter()
                .enumerate()
                .filter_map(|(index, track)| {
                    let track = track.as_ref()?;
                    Some(if output.carries(track) {
                        TrackFrom::Original(index)
                    } else {
                        let target = output.target(track.kind);
                        plan.conversions.insert((index, target));
                        TrackFrom::Converted(index, target)
                    })
                })
                .collect();
            plan.broadcasts.entry(read).or_default().insert(output);
        }
        plan
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(codec: Id) -> Option<Track> {
        Some(Track {
            kind: Kind::Video,
            codec,
            reorders: false,
        })
    }

    fn audio(codec: Id) -> Option<Track> {
        Some(Track {
            kind: Kind::Audio,
            codec,
            reorders: false,
        })
    }

    fn outputs(outputs: &[Output]) -> BTreeSet<Output> {
        outputs.iter().copied().collect()
    }

    use Output::{Hls, Rtsp, WebRtc};
    use TrackFrom::{Converted, Original};

    #[test]
    fn a_camera_every_output_carries_is_read_as_it_is() {
        let plan = Plan::of(
            &[video(Id::H264), audio(Id::PCM_MULAW)],
            &outputs(&[Rtsp, WebRtc]),
        );
        assert!(plan.conversions.is_empty());
        assert_eq!(
            plan.broadcasts,
            BTreeMap::from([(vec![Original(0), Original(1)], outputs(&[Rtsp, WebRtc]))])
        );
    }

    #[test]
    fn aac_is_converted_for_webrtc_alone() {
        let tracks = [video(Id::H264), audio(Id::AAC)];
        let plan = Plan::of(&tracks, &outputs(&[Rtsp, WebRtc, Hls]));
        assert_eq!(plan.conversions, BTreeSet::from([(1, Target::Opus)]));
        assert_eq!(
            plan.broadcasts,
            BTreeMap::from([
                (vec![Original(0), Original(1)], outputs(&[Rtsp, Hls])),
                (
                    vec![Original(0), Converted(1, Target::Opus)],
                    outputs(&[WebRtc])
                ),
            ])
        );
    }

    #[test]
    fn g711_is_converted_for_a_recording_alone() {
        let plan = Plan::of(
            &[video(Id::H264), audio(Id::PCM_ALAW)],
            &outputs(&[WebRtc, Hls]),
        );
        assert_eq!(plan.conversions, BTreeSet::from([(1, Target::Aac)]));
        assert_eq!(plan.broadcasts.len(), 2);
    }

    #[test]
    fn a_conversion_two_outputs_need_is_made_once() {
        // MJPEG neither WebRTC nor a recording carries; each wants its own
        // sound.
        let plan = Plan::of(
            &[video(Id::MJPEG), audio(Id::MP3)],
            &outputs(&[WebRtc, Hls]),
        );
        assert_eq!(
            plan.conversions,
            BTreeSet::from([(0, Target::H264), (1, Target::Opus), (1, Target::Aac)])
        );
        assert_eq!(
            plan.broadcasts,
            BTreeMap::from([
                (
                    vec![Converted(0, Target::H264), Converted(1, Target::Opus)],
                    outputs(&[WebRtc])
                ),
                (
                    vec![Converted(0, Target::H264), Converted(1, Target::Aac)],
                    outputs(&[Hls])
                ),
            ])
        );
    }

    #[test]
    fn b_frames_are_converted_for_webrtc_and_kept_for_a_recording() {
        let tracks = [Some(Track {
            kind: Kind::Video,
            codec: Id::H264,
            reorders: true,
        })];
        let plan = Plan::of(&tracks, &outputs(&[WebRtc, Hls]));
        assert_eq!(plan.conversions, BTreeSet::from([(0, Target::H264)]));
        assert_eq!(
            plan.broadcasts[&vec![Original(0)]],
            outputs(&[Hls]),
            "a recording keeps them"
        );
    }

    #[test]
    fn h265_is_converted_for_webrtc() {
        let plan = Plan::of(&[video(Id::HEVC)], &outputs(&[Rtsp, WebRtc, Hls]));
        assert_eq!(plan.conversions, BTreeSet::from([(0, Target::H264)]));
        assert_eq!(plan.broadcasts[&vec![Original(0)]], outputs(&[Rtsp, Hls]));
    }

    #[test]
    fn what_no_output_reads_is_left_out() {
        let plan = Plan::of(&[video(Id::H264), None], &outputs(&[WebRtc]));
        assert_eq!(
            plan.broadcasts,
            BTreeMap::from([(vec![Original(0)], outputs(&[WebRtc]))])
        );
    }

    #[test]
    fn nothing_read_runs_nothing() {
        let plan = Plan::of(&[video(Id::MJPEG)], &BTreeSet::new());
        assert_eq!(plan, Plan::default());
    }
}
