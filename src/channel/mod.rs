//! A channel: one [`Source`](source::Source) writing a stream into a
//! `Broadcast`, and what reads it — see `docs/design.md`, "The model".
//!
//! A channel is made with its source alone, which it pulls from then on.
//! What reads it is attached and detached while it runs: streaming — the
//! channel registered on the RTSP and WebRTC listeners, as
//! [`Sink`](sink::Sink)s — and recording, by a
//! [`Recorder`](record::Recorder).
//!
//! Neither source nor sink is media-pp's trait of the same name. A relay-pp
//! source is whatever takes a stream in, over as many media-pp pipelines as
//! its input takes connections; a sink is whatever reads the broadcast.

mod record;
mod sink;
mod source;
mod task;

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;

use crate::config::RecordConfig;
use record::RecordStatus;
pub(crate) use record::Recorder;
use sink::Sink;
pub(crate) use sink::{Listeners, Protocol};
use source::Source;
pub use source::SourceState;

/// What a channel is made from: the body of `POST /api/v1/channels`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelConfig {
    /// What it goes by: `rtsp://host/<name>`, `/<name>/whep`, the API's
    /// `/api/v1/channels/<name>`.
    pub name: String,
    /// Where the stream comes from: an `rtsp://` or `rtsps://` URL, pulled
    /// for as long as the channel is there.
    pub source: String,
    /// How the stream is pulled: over the RTSP connection (`"tcp"`, the
    /// default, which loses nothing) or `"udp"`.
    #[serde(default)]
    pub transport: Transport,
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
        let scheme = self.source.split("://").next().unwrap_or_default();
        if !self.source.contains("://") || !matches!(scheme, "rtsp" | "rtsps") {
            return Err(ChannelError::Source {
                channel: self.name.clone(),
                url: redact(&self.source),
            });
        }
        Ok(())
    }
}

/// Why a channel could not be made.
#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    /// Its name is not one a URL carries as one part of its path.
    #[error(
        "channel {0:?}: a name is letters, digits, `-`, `_`, `.` and `~`, and not `api`, `playback` or `recordings`"
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
    /// Readers of its broadcast: players and recorders.
    pub readers: usize,
    /// What its broadcast has been written, over every connection.
    pub bytes: u64,
    /// What reads it: `"rtsp"` and `"webrtc"` once a player has asked for
    /// it by that protocol, `"record"` while it is recorded.
    pub sinks: Vec<&'static str>,
    /// The playlist of the recording being written, while one is.
    pub recording: Option<String>,
    /// Why its last recording failed, until one is written again.
    pub record_error: Option<String>,
}

/// A channel being served.
pub struct Channel {
    name: String,
    source: Source,
    /// What it is attached to, behind one lock — so nothing is attached to
    /// a channel once it is halted.
    attached: Mutex<Attached>,
    /// How a recorder of it records, and where its tasks run.
    record: RecordConfig,
    runtime: Handle,
    /// Set by the first [`Channel::halt`], under `attached`'s lock. Its
    /// streaming sinks are detached by name, so a second detach would take
    /// a channel of the same name made since off its listeners.
    halted: AtomicBool,
}

#[derive(Default)]
struct Attached {
    /// Its registrations on the players' listeners, each made as a player
    /// first asks for it by that protocol.
    stream: Vec<Sink>,
    /// Its recorder, while it is recorded.
    recorder: Option<Recorder>,
}

impl Channel {
    /// Makes a channel of its source alone, and starts pulling it on
    /// `runtime`.
    pub(crate) fn start(
        config: &ChannelConfig,
        record: &RecordConfig,
        runtime: &Handle,
    ) -> Result<Self, ChannelError> {
        config.check()?;
        let channel = Self {
            name: config.name.clone(),
            source: Source::new(&config.name, config),
            attached: Mutex::default(),
            record: record.clone(),
            runtime: runtime.clone(),
            halted: AtomicBool::new(false),
        };
        channel.source.run(runtime);
        tracing::info!(channel = %config.name, source = %redact(&config.source), "made");
        Ok(channel)
    }

    /// What it is doing now.
    pub fn status(&self) -> ChannelStatus {
        let source = self.source.status();
        let stats = self.source.broadcast().stats();
        let (sinks, record) = {
            let attached = self.attached();
            let mut sinks: Vec<&'static str> = attached
                .stream
                .iter()
                .map(|sink| sink.protocol().name())
                .collect();
            let record = match &attached.recorder {
                Some(recorder) => {
                    sinks.push("record");
                    recorder.status()
                }
                None => RecordStatus::default(),
            };
            (sinks, record)
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
            readers: stats.readers,
            bytes: stats.bytes,
            sinks,
            recording: record.recording,
            record_error: record.error,
        }
    }

    /// Registers it on `protocol`'s listener, unless it is already: from
    /// then on players open it there, from the newest keyframe it holds,
    /// until the channel goes.
    pub(crate) fn stream(
        &self,
        protocol: Protocol,
        listeners: &Listeners,
    ) -> Result<(), AttachError> {
        let mut attached = self.attached_unless_halted()?;
        if attached
            .stream
            .iter()
            .any(|sink| sink.protocol() == protocol)
        {
            return Ok(());
        }
        let sink = Sink::attach(protocol, &self.name, self.source.broadcast(), listeners).map_err(
            |error| AttachError::Listener {
                channel: self.name.clone(),
                error,
            },
        )?;
        attached.stream.push(sink);
        tracing::info!(channel = %self.name, protocol = protocol.name(), "streamed");
        Ok(())
    }

    /// Waits, up to `within`, for its source to be live — at once where it
    /// is — or says what it is doing instead.
    pub(crate) async fn live(&self, within: Duration) -> Result<(), String> {
        let mut runs = self.source.runs();
        let live = tokio::time::timeout(within, async {
            runs.wait_for(Option::is_some).await.is_ok()
        })
        .await;
        if live == Ok(true) {
            return Ok(());
        }
        let status = self.source.status();
        Err(match (status.state, status.error) {
            (SourceState::Waiting, Some(error)) => {
                format!("channel {}: waiting to connect again: {error}", self.name)
            }
            _ => format!("channel {}: its camera is not live yet", self.name),
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
            self.source.broadcast(),
            self.source.runs(),
            self.source.reconnect(),
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
        for sink in std::mem::take(&mut attached.stream) {
            sink.detach(&self.name);
        }
        if let Some(recorder) = &attached.recorder {
            recorder.halt();
        }
        drop(attached);
        self.source.halt();
        tracing::info!(channel = %self.name, "removed");
    }

    /// Halts it, and waits for its recording to be finished and its
    /// source's task to end — no longer than a connection's pipeline takes
    /// to stop. Blocks the thread: never call it on the runtime's.
    pub(crate) fn stop(&self) {
        self.halt();
        let recorder = self.attached().recorder.take();
        if let Some(recorder) = recorder {
            recorder.wait();
        }
        self.source.wait();
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
const RESERVED: [&str; 3] = ["api", "playback", "recordings"];

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
            source: source.to_owned(),
            transport: Transport::Tcp,
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
