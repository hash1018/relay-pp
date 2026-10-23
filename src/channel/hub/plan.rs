//! What a hub converts, and for which sinks: worked out from the tracks its
//! source sends and what each sink reading it carries, nothing run.
//!
//! A sink carries some tracks and not others — what its server or muxer
//! sends, and what its players play — which it says as a [`Profile`]. Each
//! profile reads one broadcast of tracks it carries: the source's own where
//! it carries every track of it, or one made for it, where each track it
//! cannot carry is converted. A conversion is of one track to one codec,
//! and is made once however many profiles read it; profiles that are to
//! read the same tracks read one broadcast.

use std::collections::{BTreeMap, BTreeSet};

use media_pp::elements::StreamInfo;
use media_pp::ffmpeg::media;

use super::Refusal;
use crate::config::{ConvertConfig, SoundConversion, VideoConversion};

/// What a kind of sink carries, and what it is given in place of a track it
/// does not: pictures as H.264, sound as [`Profile::sound`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Profile {
    /// What it goes by, in a log and the status. Two profiles of one name
    /// are one profile.
    pub(crate) name: &'static str,
    /// Whether it carries a track as it is.
    pub(crate) carries: fn(&StreamInfo) -> bool,
    /// What sound it does not carry is converted to.
    pub(crate) sound: Target,
}

/// A codec a track is converted to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Target {
    /// With no B-frames, which a browser needs.
    H264,
    Opus,
    Aac,
}

impl Target {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Target::H264 => "h264",
            Target::Opus => "opus",
            Target::Aac => "aac",
        }
    }
}

/// Where a track of a broadcast comes from: the source's own track of that
/// index, or that track converted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum TrackFrom {
    Original(usize),
    Converted(usize, Target),
}

/// What a hub runs for the profiles reading it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Each conversion, of a source track to a codec, once.
    pub(crate) conversions: BTreeSet<(usize, Target)>,
    /// The broadcasts read, by the tracks each has, and the profiles reading
    /// each. One of every track of the source, in order and unconverted, is
    /// the source's own broadcast.
    pub(crate) broadcasts: BTreeMap<Vec<TrackFrom>, BTreeSet<&'static str>>,
    /// Each profile that reads nothing, and why.
    pub(crate) refused: BTreeMap<&'static str, Refusal>,
}

impl Plan {
    /// The plan for a source sending `tracks`, in its broadcast's order, read
    /// by `profiles`, converting what `convert` allows: sound not converted
    /// is left out of what a profile that cannot carry it reads, and a
    /// profile whose pictures may not be converted is refused. A track
    /// neither pictures nor sound no profile reads.
    pub(crate) fn of(tracks: &[StreamInfo], profiles: &[Profile], convert: ConvertConfig) -> Self {
        let mut plan = Plan::default();
        'profiles: for profile in profiles {
            let mut read = Vec::new();
            let mut conversions = Vec::new();
            for (index, track) in tracks.iter().enumerate() {
                let kind = track.kind();
                if !matches!(kind, media::Type::Video | media::Type::Audio) {
                    continue;
                }
                if (profile.carries)(track) {
                    read.push(TrackFrom::Original(index));
                    continue;
                }
                let target = if kind == media::Type::Video {
                    if convert.video == VideoConversion::Off {
                        plan.refused.insert(
                            profile.name,
                            Refusal::NotConverted {
                                sink: profile.name,
                                codec: format!("{:?}", track.parameters().id()),
                            },
                        );
                        continue 'profiles;
                    }
                    Target::H264
                } else {
                    if convert.sound == SoundConversion::Off {
                        continue;
                    }
                    profile.sound
                };
                conversions.push((index, target));
                read.push(TrackFrom::Converted(index, target));
            }
            plan.conversions.extend(conversions);
            plan.broadcasts
                .entry(read)
                .or_default()
                .insert(profile.name);
        }
        plan
    }
}

#[cfg(test)]
mod tests {
    use media_pp::ffmpeg::Rational;
    use media_pp::ffmpeg::codec::Id;

    use super::*;

    fn video(codec: Id) -> StreamInfo {
        StreamInfo::video(codec, &[], (320, 240), Rational::new(1, 90_000)).unwrap()
    }

    fn audio(codec: Id) -> StreamInfo {
        StreamInfo::audio(codec, &[], 48_000, 1, Rational::new(1, 48_000)).unwrap()
    }

    // Two profiles as a browser's and a recording's might be: each takes
    // H.264, one Opus and G.711 sound, the other AAC.
    const BROWSER: Profile = Profile {
        name: "browser",
        carries: |track| {
            matches!(
                track.parameters().id(),
                Id::H264 | Id::OPUS | Id::PCM_MULAW | Id::PCM_ALAW
            )
        },
        sound: Target::Opus,
    };
    const FILE: Profile = Profile {
        name: "file",
        carries: |track| matches!(track.parameters().id(), Id::H264 | Id::HEVC | Id::AAC),
        sound: Target::Aac,
    };
    const EVERYTHING: Profile = Profile {
        name: "everything",
        carries: |_| true,
        sound: Target::Aac,
    };

    fn names(names: &[&'static str]) -> BTreeSet<&'static str> {
        names.iter().copied().collect()
    }

    use TrackFrom::{Converted, Original};

    #[test]
    fn a_source_every_profile_carries_is_read_as_it_is() {
        let plan = Plan::of(
            &[video(Id::H264), audio(Id::PCM_MULAW)],
            &[EVERYTHING, BROWSER],
            ConvertConfig::default(),
        );
        assert!(plan.conversions.is_empty());
        assert_eq!(
            plan.broadcasts,
            BTreeMap::from([(
                vec![Original(0), Original(1)],
                names(&["browser", "everything"])
            )])
        );
    }

    #[test]
    fn sound_is_converted_for_the_profile_that_cannot_carry_it_alone() {
        let plan = Plan::of(
            &[video(Id::H264), audio(Id::AAC)],
            &[EVERYTHING, BROWSER, FILE],
            ConvertConfig::default(),
        );
        assert_eq!(plan.conversions, BTreeSet::from([(1, Target::Opus)]));
        assert_eq!(
            plan.broadcasts,
            BTreeMap::from([
                (
                    vec![Original(0), Original(1)],
                    names(&["everything", "file"])
                ),
                (
                    vec![Original(0), Converted(1, Target::Opus)],
                    names(&["browser"])
                ),
            ])
        );
    }

    #[test]
    fn a_conversion_two_profiles_need_is_made_once() {
        // MJPEG neither carries; each wants its own sound.
        let plan = Plan::of(
            &[video(Id::MJPEG), audio(Id::MP3)],
            &[BROWSER, FILE],
            ConvertConfig::default(),
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
                    names(&["browser"])
                ),
                (
                    vec![Converted(0, Target::H264), Converted(1, Target::Aac)],
                    names(&["file"])
                ),
            ])
        );
    }

    #[test]
    fn profiles_reading_the_same_tracks_read_one_broadcast() {
        let plan = Plan::of(
            &[video(Id::MJPEG)],
            &[BROWSER, FILE],
            ConvertConfig::default(),
        );
        assert_eq!(plan.conversions, BTreeSet::from([(0, Target::H264)]));
        assert_eq!(
            plan.broadcasts,
            BTreeMap::from([(
                vec![Converted(0, Target::H264)],
                names(&["browser", "file"])
            )])
        );
    }

    #[test]
    fn nothing_read_runs_nothing() {
        let plan = Plan::of(&[video(Id::MJPEG)], &[], ConvertConfig::default());
        assert_eq!(plan, Plan::default());
    }

    #[test]
    fn sound_not_to_be_converted_is_left_out_where_it_is_not_carried() {
        let convert = ConvertConfig {
            sound: SoundConversion::Off,
            ..ConvertConfig::default()
        };
        let plan = Plan::of(
            &[video(Id::H264), audio(Id::AAC)],
            &[BROWSER, FILE],
            convert,
        );
        assert!(plan.conversions.is_empty());
        assert_eq!(
            plan.broadcasts,
            BTreeMap::from([
                (vec![Original(0)], names(&["browser"])),
                (vec![Original(0), Original(1)], names(&["file"])),
            ])
        );
    }

    #[test]
    fn a_profile_whose_pictures_are_not_to_be_converted_is_refused() {
        let convert = ConvertConfig {
            video: VideoConversion::Off,
            ..ConvertConfig::default()
        };
        let plan = Plan::of(
            &[video(Id::HEVC), audio(Id::AAC)],
            &[EVERYTHING, BROWSER, FILE],
            convert,
        );
        assert!(
            plan.conversions.is_empty(),
            "nor its sound for it: {plan:?}"
        );
        assert_eq!(
            plan.refused["browser"],
            Refusal::NotConverted {
                sink: "browser",
                codec: "HEVC".to_owned()
            }
        );
        assert_eq!(
            plan.broadcasts,
            BTreeMap::from([(
                vec![Original(0), Original(1)],
                names(&["everything", "file"])
            )])
        );
    }
}
