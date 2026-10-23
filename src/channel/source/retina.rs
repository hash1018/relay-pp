//! Pulling a camera with `retina`, a Rust RTSP client, on the runtime: no
//! thread of the connection's own, and a fraction of FFmpeg's processor
//! time — FFmpeg's demuxer reads RTSP over TCP a few bytes at a time, and
//! that, not media-pp, was most of what pulling a camera cost
//! (`docs/benchmarks.md`).
//!
//! What it hands the broadcast is what FFmpeg's client would: H.264 and
//! H.265 in Annex B, keyframes carrying their parameter sets, raw AAC, with
//! the same configuration in each track's description. It takes what most
//! cameras send — `rtsp://` over TCP, H.264 or H.265, AAC or G.711 — with
//! no B-frames, and says so where a camera sends anything else, for
//! FFmpeg's to take it.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use media_pp::buffer::MediaBuffer;
use media_pp::element::RawSink;
use media_pp::elements::{BroadcastSink, StreamInfo};
use media_pp::ffmpeg::{self, Rational};
use retina::client::{
    Credentials, Demuxed, PlayOptions, Session, SessionOptions, SetupOptions, TcpTransportOptions,
    Transport,
};
use retina::codec::{CodecItem, FrameFormat, ParameterSetInsertion, ParametersRef};
use tokio::sync::watch;

use super::{CAMERA_TIMEOUT, Ended, Inner, Puller, halted};

/// How long a camera has, once playing, to have described every track it
/// is to be published with — from its description, or its first keyframe.
const DESCRIBED_WITHIN: Duration = Duration::from_secs(10);
/// How many frames are kept while a camera is still describing its tracks.
const KEPT_WHILE_DESCRIBING: usize = 1024;

/// How a pull by `retina` came out.
pub(super) enum Pulled {
    /// It ran, and ended so.
    Ended(Ended),
    /// It cannot take this camera, for the reason given: FFmpeg's client
    /// is to, and is from now on.
    Unsupported(String),
}

/// Pulls the camera until it fails, is halted — or turns out to send what
/// `retina` cannot take: a codec, said in its description, or pictures out
/// of the order they are shown in, seen once they come.
pub(super) async fn pull(inner: &Arc<Inner>, halt: &mut watch::Receiver<bool>) -> Pulled {
    let (url, creds) = match split_credentials(&inner.url) {
        Ok(split) => split,
        Err(reason) => return Pulled::Unsupported(reason),
    };
    let options = SessionOptions::default()
        .creds(creds)
        .user_agent("relay-pp".to_owned());
    let described = tokio::select! {
        described = tokio::time::timeout(CAMERA_TIMEOUT, Session::describe(url, options)) => described,
        () = halted(halt) => return Pulled::Ended(Ended::Halted),
    };
    let mut session = match described {
        Ok(Ok(session)) => session,
        Ok(Err(error)) => return failed(format!("cannot open the camera: {error}")),
        Err(_) => return failed(no_answer("describe")),
    };

    // Pictures and sound, each in a codec taken here; a data or metadata
    // track is left out, as FFmpeg's pull leaves it out.
    let mut kept = Vec::new();
    for (index, stream) in session.streams().iter().enumerate() {
        if !matches!(stream.media(), "video" | "audio") {
            continue;
        }
        if codec_of(stream.media(), stream.encoding_name()).is_none() {
            return Pulled::Unsupported(format!(
                "a {} track in {}",
                stream.media(),
                stream.encoding_name()
            ));
        }
        kept.push(index);
    }
    if kept.is_empty() {
        return failed("the camera sends neither pictures nor sound".to_owned());
    }
    let format = FrameFormat {
        h26x_framing: retina::codec::h26x::Framing::AnnexB,
        parameter_set_insertion: ParameterSetInsertion::EachKeyFrame,
        aac_framing: retina::codec::aac::Framing::Raw,
    };
    for &index in &kept {
        let setup = SetupOptions::default()
            .transport(Transport::Tcp(TcpTransportOptions::default()))
            .frame_format(format);
        match tokio::time::timeout(CAMERA_TIMEOUT, session.setup(index, setup)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return failed(format!("cannot set the camera up: {error}")),
            Err(_) => return failed(no_answer("set up")),
        }
    }
    let playing = match tokio::time::timeout(CAMERA_TIMEOUT, session.play(PlayOptions::default()))
        .await
    {
        Ok(Ok(playing)) => playing,
        // Its tracks' times cannot be lined up from what it said: FFmpeg's
        // client lines them up by the sender reports instead.
        Ok(Err(error)) if error.to_string().contains("rtptime") => {
            return Pulled::Unsupported(format!("no rtptime to line its tracks up by: {error}"));
        }
        Ok(Err(error)) => return failed(format!("cannot play the camera: {error}")),
        Err(_) => return failed(no_answer("play")),
    };
    let mut demuxed = match playing.demuxed() {
        Ok(demuxed) => demuxed,
        Err(error) => return failed(format!("cannot play the camera: {error}")),
    };

    let mut connection = Connection {
        inner,
        kept,
        sinks: Vec::new(),
        picture_ticks: Vec::new(),
        last: Vec::new(),
    };
    connection.run(&mut demuxed, halt).await
}

/// A pull under way: which of the session's streams are the broadcast's
/// tracks, and the sinks of the run they are written into.
struct Connection<'a> {
    inner: &'a Arc<Inner>,
    /// The session's stream of each track, in the tracks' order.
    kept: Vec<usize>,
    sinks: Vec<BroadcastSink>,
    /// A picture's duration in each track's clock, where its camera says
    /// its rate.
    picture_ticks: Vec<Option<i64>>,
    /// The last timestamp written of each track.
    last: Vec<Option<i64>>,
}

impl Connection<'_> {
    async fn run(&mut self, demuxed: &mut Demuxed, halt: &mut watch::Receiver<bool>) -> Pulled {
        // Until every track is described, what arrives is kept, to be
        // written once the tracks are published.
        let mut early = Vec::new();
        let deadline = tokio::time::Instant::now() + DESCRIBED_WITHIN;
        while !self.described(demuxed) {
            if early.len() >= KEPT_WHILE_DESCRIBING || tokio::time::Instant::now() >= deadline {
                return failed("the camera did not describe its tracks".to_owned());
            }
            match self.next(demuxed, halt).await {
                Ok(item) => early.push(item),
                Err(ended) => return Pulled::Ended(ended),
            }
        }
        if let Err(reason) = self.publish(demuxed) {
            return failed(reason);
        }
        self.inner.went_live(Puller::Retina);
        for item in early {
            if let Err(stopped) = self.write(demuxed, item) {
                return stopped.into();
            }
        }
        loop {
            let item = match self.next(demuxed, halt).await {
                Ok(item) => item,
                Err(ended) => return Pulled::Ended(ended),
            };
            if let CodecItem::VideoFrame(frame) = &item
                && frame.has_new_parameters()
            {
                // The picture changed shape: a run of its own, described
                // anew, rather than one its readers' descriptions no longer
                // hold for.
                if let Err(reason) = self.publish(demuxed) {
                    return failed(reason);
                }
                self.inner.new_run();
                tracing::info!(channel = %self.inner.name, "the camera's pictures changed; published anew");
            }
            if let Err(stopped) = self.write(demuxed, item) {
                return stopped.into();
            }
        }
    }

    /// The next item, or how the connection ended: a camera silent for its
    /// timeout, a halt, a reconnection asked for.
    async fn next(
        &self,
        demuxed: &mut Demuxed,
        halt: &mut watch::Receiver<bool>,
    ) -> Result<CodecItem, Ended> {
        tokio::select! {
            item = tokio::time::timeout(CAMERA_TIMEOUT, demuxed.next()) => match item {
                Ok(Some(Ok(item))) => Ok(item),
                Ok(Some(Err(error))) => Err(Ended::Failed(format!("the camera failed: {error}"))),
                Ok(None) => Err(Ended::Failed("the camera ended its stream".to_owned())),
                Err(_) => Err(Ended::Failed(format!(
                    "the camera sent nothing for {}s",
                    CAMERA_TIMEOUT.as_secs()
                ))),
            },
            () = halted(halt) => Err(Ended::Halted),
            () = self.inner.reconnect.notified() => Err(Ended::Reconnect),
        }
    }

    fn described(&self, demuxed: &Demuxed) -> bool {
        self.kept
            .iter()
            .all(|&index| demuxed.streams()[index].parameters().is_some())
    }

    /// Publishes the tracks as the session now describes them: a run.
    fn publish(&mut self, demuxed: &Demuxed) -> Result<(), String> {
        let tracks = self
            .kept
            .iter()
            .map(|&index| track(&demuxed.streams()[index]))
            .collect::<Result<Vec<_>, _>>()?;
        self.sinks = self
            .inner
            .broadcast
            .publish(&tracks)
            .map_err(|error| format!("cannot publish: {error}"))?;
        self.picture_ticks = self
            .kept
            .iter()
            .map(|&index| picture_ticks(&demuxed.streams()[index]))
            .collect();
        self.last = vec![None; self.kept.len()];
        Ok(())
    }

    /// Writes a frame into its track's sink, as FFmpeg's client would hand
    /// it on; anything else the session gives — sender reports, a camera's
    /// metadata — goes nowhere.
    fn write(&mut self, demuxed: &Demuxed, item: CodecItem) -> Result<(), Stopped> {
        // What a frame lasts: a sound's its samples; a picture's what its
        // camera's rate says, or the time since the last one.
        let (stream, data, timestamp, key, samples) = match &item {
            CodecItem::VideoFrame(frame) => (
                frame.stream_id(),
                frame.data(),
                frame.timestamp(),
                frame.is_random_access_point(),
                None,
            ),
            CodecItem::AudioFrame(frame) => (
                frame.stream_id(),
                frame.data(),
                frame.timestamp(),
                true,
                Some(i64::from(frame.frame_length().get())),
            ),
            _ => return Ok(()),
        };
        let Some(track) = self.kept.iter().position(|&index| index == stream) else {
            return Ok(());
        };
        let clock = demuxed.streams()[stream].clock_rate_hz();
        let mut packet = ffmpeg::Packet::copy(data);
        let pts = timestamp.elapsed();
        // Pictures sent out of the order they are shown in — B-frames: RTP
        // gives each its time shown and no other, so the order they are
        // decoded in cannot be stamped here. FFmpeg's client works it out.
        if samples.is_none() && self.last[track].is_some_and(|last| pts < last) {
            return Err(Stopped::Reorders);
        }
        let since_last = self.last[track]
            .map(|last| pts - last)
            .filter(|ticks| *ticks > 0);
        self.last[track] = Some(pts);
        let duration = samples
            .or(self.picture_ticks[track])
            .or(since_last)
            .unwrap_or(0);
        packet.set_pts(Some(pts));
        packet.set_dts(Some(pts));
        packet.set_duration(duration);
        packet.set_time_base(Rational::new(1, clock as i32));
        if key {
            packet.set_flags(ffmpeg::packet::Flags::KEY);
        }
        self.sinks[track]
            .consume(MediaBuffer::packet(packet))
            .map_err(|error| Stopped::Failed(format!("cannot write into the broadcast: {error}")))
    }
}

/// Why a frame could not be written.
enum Stopped {
    Failed(String),
    /// The camera sends pictures out of the order they are shown in.
    Reorders,
}

impl From<Stopped> for Pulled {
    fn from(stopped: Stopped) -> Self {
        match stopped {
            Stopped::Failed(reason) => failed(reason),
            Stopped::Reorders => Pulled::Unsupported(
                "pictures sent out of the order they are shown in (B-frames)".to_owned(),
            ),
        }
    }
}

/// A track's description, as FFmpeg's client would give it of the same
/// stream.
fn track(stream: &retina::client::Stream) -> Result<StreamInfo, String> {
    let codec = codec_of(stream.media(), stream.encoding_name())
        .ok_or_else(|| format!("no codec for {}", stream.encoding_name()))?;
    let unit = Rational::new(1, stream.clock_rate_hz() as i32);
    match stream.parameters() {
        Some(ParametersRef::Video(video)) => {
            let info = StreamInfo::video(codec, video.extra_data(), video.pixel_dimensions(), unit)
                .map_err(|error| error.to_string())?;
            // A picture's duration, as a ratio: its inverse is the rate.
            Ok(match video.frame_rate() {
                Some((num, den)) if num > 0 => {
                    info.with_frame_rate(Rational::new(den as i32, num as i32))
                }
                _ => info,
            })
        }
        Some(ParametersRef::Audio(audio)) => StreamInfo::audio(
            codec,
            audio.extra_data(),
            audio.clock_rate(),
            audio.channels().get(),
            unit,
        )
        .map_err(|error| error.to_string()),
        _ => Err(format!("{} is not described", stream.encoding_name())),
    }
}

/// A picture's duration in `stream`'s clock, where its camera says its rate.
fn picture_ticks(stream: &retina::client::Stream) -> Option<i64> {
    let Some(ParametersRef::Video(video)) = stream.parameters() else {
        return None;
    };
    // Seconds a picture, as a ratio.
    let (num, den) = video.frame_rate()?;
    (den > 0).then(|| i64::from(stream.clock_rate_hz()) * i64::from(num) / i64::from(den))
}

/// The codec of a stream `retina` takes, by its RTP payload format.
fn codec_of(media: &str, encoding: &str) -> Option<ffmpeg::codec::Id> {
    match (media, encoding) {
        ("video", "h264") => Some(ffmpeg::codec::Id::H264),
        ("video", "h265") => Some(ffmpeg::codec::Id::HEVC),
        ("audio", "mpeg4-generic") => Some(ffmpeg::codec::Id::AAC),
        ("audio", "pcmu") => Some(ffmpeg::codec::Id::PCM_MULAW),
        ("audio", "pcma") => Some(ffmpeg::codec::Id::PCM_ALAW),
        _ => None,
    }
}

/// `url` without its user name and password, and them as credentials:
/// `retina` takes them apart from the URL. What it cannot take — `rtsps://`,
/// a URL it cannot read — is said so.
fn split_credentials(url: &str) -> Result<(url::Url, Option<Credentials>), String> {
    let mut url = url::Url::parse(url).map_err(|error| format!("a URL it cannot read: {error}"))?;
    if url.scheme() != "rtsp" {
        return Err(format!("{}://", url.scheme()));
    }
    let creds = (!url.username().is_empty()).then(|| Credentials {
        username: decode(url.username()),
        password: decode(url.password().unwrap_or_default()),
    });
    let _ = url.set_username("");
    let _ = url.set_password(None);
    Ok((url, creds))
}

fn decode(text: &str) -> String {
    url::form_urlencoded::parse(format!("x={text}").as_bytes())
        .next()
        .map_or_else(|| text.to_owned(), |(_, value)| value.into_owned())
}

fn failed(reason: String) -> Pulled {
    Pulled::Ended(Ended::Failed(reason))
}

fn no_answer(what: &str) -> String {
    format!(
        "cannot {what} the camera: no answer in {}s",
        CAMERA_TIMEOUT.as_secs()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_taken_out_of_the_url() {
        let (url, creds) = split_credentials("rtsp://admin:p%40ss@10.0.0.1:554/live").unwrap();
        assert_eq!(url.as_str(), "rtsp://10.0.0.1:554/live");
        let creds = creds.unwrap();
        assert_eq!(
            (creds.username.as_str(), creds.password.as_str()),
            ("admin", "p@ss")
        );
        let (_, none) = split_credentials("rtsp://10.0.0.1/live").unwrap();
        assert!(none.is_none());
        assert!(split_credentials("rtsps://10.0.0.1/live").is_err());
    }

    #[test]
    fn only_what_is_taken_here_has_a_codec() {
        assert_eq!(codec_of("video", "h264"), Some(ffmpeg::codec::Id::H264));
        assert_eq!(
            codec_of("audio", "mpeg4-generic"),
            Some(ffmpeg::codec::Id::AAC)
        );
        assert_eq!(codec_of("audio", "opus"), None);
        assert_eq!(codec_of("video", "mp4v-es"), None);
    }
}
