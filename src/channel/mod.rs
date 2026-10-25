//! A channel: one [`Source`](source::Source) taking a stream in, one
//! [`Hub`](hub::Hub) it writes into, and the sinks reading the hub — see
//! `docs/design.md`, "The model".
//!
//! A channel is made with its source alone, which it pulls from then on.
//! Its sinks are attached and detached while it runs: RTSP and WebRTC
//! players, through the listeners every channel shares, as a player first
//! asks for the channel ([`ListenerSink`](sink::ListenerSink)), a
//! recording ([`Recorder`](sink::Recorder)), and a live HLS playlist while
//! players ask for it ([`HlsLive`](sink::HlsLive)). Each sink says what it
//! carries, and the hub gives it that — the source's own broadcast, or one
//! converted for it — so that neither source nor sink knows the other.
//!
//! None of source, hub nor sink is media-pp's trait of the same name.

mod first;
mod hub;
mod sink;
mod source;
mod task;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;

use crate::config::{ConvertChoice, ConvertConfig, RecordConfig};
use hub::Hub;
pub(crate) use hub::Refusal;
pub(crate) use sink::{HLS_PLAYLIST, Listeners, Output, Recorder, is_media_file};
use sink::{HlsLive, ListenerSink, RecordStatus, Unready};
use source::Source;
pub use source::{Publish, SourceState};
pub(crate) use source::{RTMP_APP, whip_path};

/// What a channel is made from: the body of `POST /api/v1/channels`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelConfig {
    /// What it goes by: `rtsp://host/<name>`, `/<name>/whep`, the API's
    /// `/api/v1/channels/<name>`.
    pub name: String,
    /// Where the stream is pulled from, for as long as the channel is
    /// there: an `rtsp://` or `rtsps://` URL. Or else `publish`.
    #[serde(default)]
    pub source: Option<String>,
    /// The protocol a publisher pushes the stream over — `"rtsp"`, `"rtmp"`
    /// or `"whip"` — where it is pushed rather than pulled.
    #[serde(default)]
    pub publish: Option<Publish>,
    /// How the stream is pulled: over the RTSP connection (`"tcp"`, the
    /// default, which loses nothing) or `"udp"`.
    #[serde(default)]
    pub transport: Transport,
    /// What is converted for a way out that cannot carry a track, over
    /// the server's `[convert]`: `{"sound": "off", "video": "any"}`, each
    /// left out the server's.
    #[serde(default)]
    pub convert: ConvertChoice,
}

/// How a camera is pulled.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// RTP interleaved on the RTSP connection.
    #[default]
    Tcp,
    /// RTP on UDP ports of its own.
    Udp,
}

impl ChannelConfig {
    /// Whether a channel can be made from it.
    fn check(&self) -> Result<(), ChannelError> {
        if !valid_name(&self.name) {
            return Err(ChannelError::Name(self.name.clone()));
        }
        match (&self.source, self.publish) {
            (Some(source), None) => {
                let scheme = source.split("://").next().unwrap_or_default();
                if !source.contains("://") || !matches!(scheme, "rtsp" | "rtsps") {
                    return Err(ChannelError::Source {
                        channel: self.name.clone(),
                        url: redact(source),
                    });
                }
                Ok(())
            }
            (None, Some(_)) => Ok(()),
            _ => Err(ChannelError::Input(self.name.clone())),
        }
    }
}

/// Why a channel could not be made.
#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    /// Its name is not one a URL carries as one part of its path.
    #[error(
        "channel {0:?}: a name is letters, digits, `-`, `_`, `.` and `~`, and not `api`, `assets`, `playback` or `recordings`"
    )]
    Name(String),
    /// Its source is not one relay-pp takes yet.
    #[error("channel {channel}: source {url:?} is not an rtsp:// or rtsps:// URL")]
    Source {
        /// The channel.
        channel: String,
        /// Its source, with any credentials cut out.
        url: String,
    },
    /// It names both a source to pull and a protocol to take a publisher
    /// over, or neither.
    #[error("channel {0}: a `source` to pull, or a protocol to `publish` over — one of them")]
    Input(String),
    /// Its source could not be made: a server would not take its
    /// publisher.
    #[error("channel {channel}: {error}")]
    Intake {
        /// The channel.
        channel: String,
        /// Why.
        error: String,
    },
    /// There is a channel of that name already.
    #[error("channel {0} exists")]
    Exists(String),
}

/// Why something could not be attached to a channel, or detached from it.
#[derive(Debug, thiserror::Error)]
pub enum AttachError {
    /// There is no channel of that name — or it is being removed.
    #[error("no channel {0}")]
    NoChannel(String),
    /// It is attached already.
    #[error("channel {0} has it already")]
    Attached(String),
    /// It is not attached.
    #[error("channel {0} does not have it")]
    NotAttached(String),
    /// A listener would not take the channel.
    #[error("channel {channel}: {error}")]
    Listener {
        /// The channel.
        channel: String,
        /// Why.
        error: String,
    },
}

/// Why players cannot play a channel yet, or at all.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PlayError {
    /// There is no channel of that name — or it is being removed.
    #[error("no channel {0}")]
    NoChannel(String),
    /// Its camera is not live: connecting, or waiting to connect again.
    #[error("channel {channel}: {reason}")]
    NotLive { channel: String, reason: String },
    /// Its camera is live, and what the players read is not made yet.
    #[error("channel {channel}: not ready yet")]
    NotReady { channel: String },
    /// What the players read is not made, and why.
    #[error("channel {channel}: {refusal}")]
    Refused { channel: String, refusal: Refusal },
    /// The players' listener would not take the channel.
    #[error("channel {channel}: {error}")]
    Listener { channel: String, error: String },
    /// Its HLS could not be written, and is begun again shortly.
    #[error("channel {channel}: {error}")]
    Hls { channel: String, error: String },
}

impl Unready {
    /// What a player of `channel` is told: `failed` says how the sink
    /// failed, which is answered as its kind is.
    fn into_play_error(
        self,
        channel: String,
        failed: impl FnOnce(String, String) -> PlayError,
    ) -> PlayError {
        match self {
            Unready::NotYet => PlayError::NotReady { channel },
            Unready::Refused(refusal) => PlayError::Refused { channel, refusal },
            Unready::Failed(error) => failed(channel, error),
        }
    }
}

/// A channel's status, as the API gives it.
#[derive(Debug, Clone, Serialize)]
pub struct ChannelStatus {
    /// The channel's name.
    pub name: String,
    /// Its source's URL, with any credentials cut out.
    pub source: String,
    /// What the source is doing.
    pub state: SourceState,
    /// Since when, in milliseconds since the Unix epoch.
    pub since_ms: u64,
    /// Why the source last failed, while it is not live.
    pub error: Option<String>,
    /// How many times the source has gone live.
    pub connections: u64,
    /// How its stream comes in: `"rtsp"` for a camera pulled, or the
    /// protocol its publisher pushes over — once one has come in.
    pub client: Option<&'static str>,
    /// Readers of its broadcast: what serves it to each listener's players
    /// while it has any, its recorder, its HLS.
    pub readers: usize,
    /// Its RTSP players.
    pub rtsp_players: usize,
    /// Its WebRTC players.
    pub webrtc_players: usize,
    /// What its broadcast has been written, over every connection.
    pub bytes: u64,
    /// What reads it: `"rtsp"` and `"webrtc"` once a player has asked for
    /// it by that protocol, `"record"` while it is recorded, `"hls"` while
    /// its HLS is written.
    pub sinks: Vec<&'static str>,
    /// The playlist of the recording being written, while one is.
    pub recording: Option<String>,
    /// Why its last recording failed, until one is written again.
    pub record_error: Option<String>,
    /// What is converted for it, its own say over the server's.
    pub convert: ConvertConfig,
    /// Each track converted, and to what, while it is: `"1:opus"`, track 1
    /// converted to Opus for WebRTC players.
    pub conversions: Vec<String>,
    /// How many times its pictures were refused a conversion for want of a
    /// free GPU encoder.
    pub gpu_refused: u64,
}

/// A channel being served.
pub struct Channel {
    name: String,
    source: Source,
    /// What the source writes into and the sinks read, converted for them.
    hub: Hub,
    /// What it may convert.
    convert: ConvertConfig,
    /// What it is attached to, behind one lock — so nothing is attached to
    /// a channel once it is halted.
    attached: Mutex<Attached>,
    /// How a recorder of it records, and where its tasks run.
    record: RecordConfig,
    /// How long its HLS is written unasked for.
    hls_idle: Duration,
    runtime: Handle,
    /// Set by the first [`Channel::halt`], under `attached`'s lock. Its
    /// listener sinks are detached by name, so a second detach would take
    /// a channel of the same name made since off its listeners.
    halted: AtomicBool,
}

#[derive(Default)]
struct Attached {
    /// Its registrations on the players' listeners, each made as a player
    /// first asks for it by that protocol.
    listened: BTreeMap<Output, Arc<ListenerSink>>,
    /// Its recorder, while it is recorded.
    recorder: Option<Recorder>,
    /// Its HLS, from a player first asking for it — which may have ended
    /// since, unasked for.
    hls: Option<Arc<HlsLive>>,
}

impl Channel {
    /// Makes a channel of its source alone, and starts pulling it on
    /// `runtime` — or taking its publisher, on the one of `listeners` its
    /// protocol is served by.
    pub(crate) fn start(
        config: &ChannelConfig,
        record: &RecordConfig,
        hls_idle: Duration,
        convert: ConvertConfig,
        listeners: &Listeners,
        runtime: &Handle,
    ) -> Result<Self, ChannelError> {
        config.check()?;
        let convert = config.convert.over(convert);
        let hub = Hub::start(&config.name, convert, runtime);
        let source =
            Source::new(&config.name, config, hub.publisher(), listeners).map_err(|error| {
                ChannelError::Intake {
                    channel: config.name.clone(),
                    error,
                }
            })?;
        let channel = Self {
            name: config.name.clone(),
            source,
            hub,
            convert,
            attached: Mutex::default(),
            record: record.clone(),
            hls_idle,
            runtime: runtime.clone(),
            halted: AtomicBool::new(false),
        };
        channel.source.run(runtime);
        tracing::info!(channel = %config.name, source = %channel.source.status().url, "made");
        Ok(channel)
    }

    /// What it is doing now.
    pub fn status(&self) -> ChannelStatus {
        let source = self.source.status();
        let stats = self.hub.stats();
        let (sinks, record, rtsp_players, webrtc_players) = {
            let attached = self.attached();
            let mut sinks: Vec<&'static str> = attached
                .listened
                .keys()
                .map(|output| output.name())
                .collect();
            let players = |output| {
                attached
                    .listened
                    .get(&output)
                    .map_or(0, |sink| sink.players())
            };
            let (rtsp_players, webrtc_players) = (players(Output::Rtsp), players(Output::WebRtc));
            let record = match &attached.recorder {
                Some(recorder) => {
                    sinks.push("record");
                    recorder.status()
                }
                None => RecordStatus::default(),
            };
            if attached.hls.as_ref().is_some_and(|hls| !hls.is_over()) {
                sinks.push("hls");
            }
            (sinks, record, rtsp_players, webrtc_players)
        };
        ChannelStatus {
            name: self.name.clone(),
            source: source.url,
            state: source.state,
            since_ms: source
                .since
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |since| since.as_millis() as u64),
            error: source.error,
            connections: source.connections,
            client: source.client,
            readers: stats.readers,
            rtsp_players,
            webrtc_players,
            bytes: stats.bytes,
            sinks,
            recording: record.recording,
            record_error: record.error,
            convert: self.convert,
            conversions: self.hub.conversions(),
            gpu_refused: self.hub.gpu_refused(),
        }
    }

    /// Has players reach it over `output` — RTSP or WebRTC — and waits, up
    /// to `within`, for what they read to be made, which is at once where
    /// they carry the source's own broadcast: registered on that listener
    /// the first time, kept registered until the channel goes, and moved to
    /// whatever its hub gives it from then on. Or says why it cannot be.
    /// After [`Self::live`].
    pub(crate) async fn ready(
        &self,
        output: Output,
        listeners: &Listeners,
        within: Duration,
    ) -> Result<(), PlayError> {
        let channel = || self.name.clone();
        let sink = {
            let mut attached = self
                .attached_unless_halted()
                .map_err(|_| PlayError::NoChannel(channel()))?;
            match attached.listened.get(&output) {
                Some(sink) => Arc::clone(sink),
                None => {
                    let sink = ListenerSink::attach(
                        output,
                        &self.name,
                        &self.hub,
                        listeners,
                        &self.runtime,
                    )
                    .ok_or_else(|| PlayError::Listener {
                        channel: channel(),
                        error: format!("no listener serves {}", output.name()),
                    })?;
                    let sink = Arc::new(sink);
                    attached.listened.insert(output, Arc::clone(&sink));
                    sink
                }
            }
        };
        sink.ready(within).await.map_err(|unready| {
            unready.into_play_error(channel(), |channel, error| PlayError::Listener {
                channel,
                error,
            })
        })
    }

    /// Has its HLS written, where it was not, and waits up to `within` for
    /// its playlist — at once where it is written already — or says why it
    /// cannot be. Where its files are. After [`Self::live`].
    pub(crate) async fn hls(&self, within: Duration) -> Result<PathBuf, PlayError> {
        let channel = || self.name.clone();
        let hls = {
            let mut attached = self
                .attached_unless_halted()
                .map_err(|_| PlayError::NoChannel(channel()))?;
            match &attached.hls {
                Some(hls) if !hls.is_over() => Arc::clone(hls),
                _ => {
                    let hls = Arc::new(HlsLive::start(
                        &self.name,
                        self.hub
                            .subscribe(Output::Hls.profile(), Output::Hls.demand()),
                        self.hls_idle,
                        &self.runtime,
                    ));
                    attached.hls = Some(Arc::clone(&hls));
                    hls
                }
            }
        };
        let dir = hls.asked().to_owned();
        hls.ready(within).await.map_err(|unready| {
            unready.into_play_error(channel(), |channel, error| PlayError::Hls {
                channel,
                error,
            })
        })?;
        Ok(dir)
    }

    /// Where its HLS files are, while it is written — being asked for one
    /// of them counted as its being asked for.
    pub(crate) fn hls_files(&self) -> Option<PathBuf> {
        let attached = self.attached();
        let hls = attached.hls.as_ref().filter(|hls| !hls.is_over())?;
        Some(hls.asked().to_owned())
    }

    /// Waits, up to `within`, for its source to be live — at once where it
    /// is — or says what it is doing instead.
    pub(crate) async fn live(&self, within: Duration) -> Result<(), PlayError> {
        let mut runs = self.hub.runs();
        let live = tokio::time::timeout(within, async {
            runs.wait_for(Option::is_some).await.is_ok()
        })
        .await;
        if live == Ok(true) {
            return Ok(());
        }
        let status = self.source.status();
        Err(PlayError::NotLive {
            channel: self.name.clone(),
            reason: match (status.state, status.error) {
                (SourceState::Waiting, Some(error)) => {
                    format!("waiting to connect again: {error}")
                }
                (SourceState::AwaitingPublisher, _) => "no publisher publishes into it".to_owned(),
                _ => "its camera is not live yet".to_owned(),
            },
        })
    }

    /// Begins recording it: from the newest keyframe it holds where it is
    /// live, and whenever it goes live after.
    pub(crate) fn attach_record(&self) -> Result<(), AttachError> {
        let mut attached = self.attached_unless_halted()?;
        if attached.recorder.is_some() {
            return Err(AttachError::Attached(self.name.clone()));
        }
        attached.recorder = Some(Recorder::start(
            &self.name,
            self.hub
                .subscribe(Output::Record.profile(), Output::Record.demand()),
            &self.record,
            &self.runtime,
        ));
        tracing::info!(channel = %self.name, "recorded");
        Ok(())
    }

    /// Ends its recording: returns the recorder told to finish, for the
    /// caller to [`Recorder::wait`] for outside anything it holds.
    pub(crate) fn detach_record(&self) -> Result<Recorder, AttachError> {
        let mut attached = self.attached_unless_halted()?;
        let recorder = attached
            .recorder
            .take()
            .ok_or_else(|| AttachError::NotAttached(self.name.clone()))?;
        recorder.halt();
        tracing::info!(channel = %self.name, "no longer recorded");
        Ok(recorder)
    }

    /// Detaches everything, tells its recorder to finish and its source to
    /// stop, without waiting for either: once this returns, no player
    /// reaches the channel and its name can be used again. Once: a second
    /// call does nothing.
    pub(crate) fn halt(&self) {
        let mut attached = self.attached();
        if self.halted.swap(true, Ordering::AcqRel) {
            return;
        }
        for sink in std::mem::take(&mut attached.listened).into_values() {
            sink.halt();
        }
        if let Some(recorder) = &attached.recorder {
            recorder.halt();
        }
        if let Some(hls) = &attached.hls {
            hls.halt();
        }
        drop(attached);
        self.source.halt();
        self.hub.halt();
        tracing::info!(channel = %self.name, "removed");
    }

    /// Halts it, and waits for its recording to be finished and its
    /// source's task to end — no longer than a connection's pipeline takes
    /// to stop.
    pub(crate) async fn stop(&self) {
        self.halt();
        let (recorder, hls) = {
            let mut attached = self.attached();
            (attached.recorder.take(), attached.hls.take())
        };
        if let Some(recorder) = recorder {
            recorder.wait().await;
        }
        if let Some(hls) = hls {
            hls.wait().await;
        }
        self.source.wait().await;
        self.hub.wait().await;
    }

    fn attached(&self) -> std::sync::MutexGuard<'_, Attached> {
        self.attached.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// What it is attached to, to change — unless it is halted, which
    /// nothing is attached to again.
    fn attached_unless_halted(&self) -> Result<std::sync::MutexGuard<'_, Attached>, AttachError> {
        let attached = self.attached();
        if self.halted.load(Ordering::Acquire) {
            return Err(AttachError::NoChannel(self.name.clone()));
        }
        Ok(attached)
    }
}

impl Drop for Channel {
    /// Halts it only: the last handle to a channel may go in a request on
    /// the runtime, which must not wait. Its tasks end by themselves.
    fn drop(&mut self) {
        self.halt();
    }
}

/// The first parts of the HTTP port's own paths, which a channel's
/// `/<name>/whep` would be taken for.
const RESERVED: [&str; 4] = ["api", "assets", "playback", "recordings"];

/// Whether `name` goes into a URL as one part of its path — letters,
/// digits, `-`, `_`, `.` and `~`, and not `.` or `..` alone — and is no
/// path of the HTTP port's own.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !RESERVED.contains(&name)
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~'))
}

/// `url` with any user name and password cut out, for a log line or an
/// answer.
pub(crate) fn redact(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
        None => url.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(name: &str, source: &str) -> ChannelConfig {
        ChannelConfig {
            name: name.to_owned(),
            source: Some(source.to_owned()),
            publish: None,
            transport: Transport::Tcp,
            convert: ConvertChoice::default(),
        }
    }

    #[test]
    fn what_cannot_be_made_is_refused_saying_what_and_no_secret() {
        let error = config("cam1", "http://user:secret@10.0.0.21/")
            .check()
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("cam1") && !error.contains("secret"),
            "{error}"
        );
        for name in [
            "front door",
            "floor2/cam3",
            "/cam",
            "cam?1",
            "",
            ".",
            "..",
            "api",
        ] {
            assert!(
                matches!(
                    config(name, "rtsp://10.0.0.1/").check(),
                    Err(ChannelError::Name(_))
                ),
                "{name:?} is no one part of a URL's path"
            );
        }
        assert!(config("floor2-cam-3_a.b~c", "rtsp://h/").check().is_ok());
        assert!(config("cam1", "rtsps://h/").check().is_ok());
    }

    #[test]
    fn a_request_body_is_its_source_alone_with_tcp_by_default() {
        let config: ChannelConfig =
            serde_json::from_str(r#"{"name":"cam1","source":"rtsp://h/"}"#).unwrap();
        assert_eq!(config.transport, Transport::Tcp);
        let config: ChannelConfig =
            serde_json::from_str(r#"{"name":"cam1","source":"rtsp://h/","transport":"udp"}"#)
                .unwrap();
        assert_eq!(config.transport, Transport::Udp);
        for body in [
            r#"{"name":"c","source":"rtsp://h/","x":1}"#,
            r#"{"name":"c","source":"rtsp://h/","record":true}"#,
        ] {
            assert!(
                serde_json::from_str::<ChannelConfig>(body).is_err(),
                "{body}"
            );
        }
    }

    #[test]
    fn credentials_are_cut_out_of_a_url() {
        assert_eq!(
            redact("rtsp://admin:p@ss@10.0.0.1:554/live"),
            "rtsp://10.0.0.1:554/live"
        );
        assert_eq!(redact("rtsp://10.0.0.1/live"), "rtsp://10.0.0.1/live");
        assert_eq!(redact("not a url"), "not a url");
    }
}
