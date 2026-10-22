//! A channel: one [`Source`](source::Source) writing a stream into a
//! `Broadcast`, and the [`Sink`](sink::Sink)s handing it out — see
//! `docs/design.md`, "The model".
//!
//! Neither is media-pp's trait of the same name. A relay-pp source is
//! whatever takes a stream in, over as many media-pp pipelines as its input
//! takes connections; a sink is whatever reads the broadcast.

mod sink;
mod source;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

pub(crate) use sink::Listeners;
use sink::Sink;
use source::Source;
pub use source::SourceState;

/// What a channel is made from: the body of `POST /api/v1/channels`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelConfig {
    /// What players ask for: `rtsp://host/<name>`, `/<name>/whep`.
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
    /// Its name is not one a URL carries as it is.
    #[error(
        "channel {0:?}: a name is letters, digits, `-`, `_`, `.` and `~`, in parts between `/`"
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
    /// A sink could not be attached to its listener.
    #[error("channel {channel}: {error}")]
    Attach {
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
    /// The sinks it has: `"rtsp"`, `"webrtc"`.
    pub sinks: Vec<&'static str>,
}

/// A channel being served.
pub struct Channel {
    name: String,
    source: Source,
    sinks: Vec<Sink>,
    /// Set by the first [`Channel::halt`]. The sinks are detached by name,
    /// so a second detach would take a channel of the same name made since
    /// off its listeners.
    halted: AtomicBool,
}

impl Channel {
    /// Attaches a channel's sinks to every listener and starts its source.
    /// Nothing is left attached, and nothing opened, where it cannot be
    /// made.
    pub(crate) fn start(
        config: &ChannelConfig,
        listeners: &Listeners,
    ) -> Result<Self, ChannelError> {
        config.check()?;
        let name = &config.name;
        let source = Source::new(name, config);
        let sinks = Sink::attach_all(name, source.broadcast(), listeners).map_err(|error| {
            ChannelError::Attach {
                channel: name.clone(),
                error,
            }
        })?;
        source.run();
        tracing::info!(channel = %name, source = %redact(&config.source), "serving");
        Ok(Self {
            name: name.clone(),
            source,
            sinks,
            halted: AtomicBool::new(false),
        })
    }

    /// What it is doing now.
    pub fn status(&self) -> ChannelStatus {
        let source = self.source.status();
        let stats = self.source.broadcast().stats();
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
            sinks: self.sinks.iter().map(Sink::kind).collect(),
        }
    }

    /// Detaches every sink and tells the source to stop, without waiting for
    /// it: once this returns, no player reaches the channel and its name can
    /// be attached again. Once: a second call does nothing.
    pub(crate) fn halt(&self) {
        if self.halted.swap(true, Ordering::AcqRel) {
            return;
        }
        for sink in &self.sinks {
            sink.detach(&self.name);
        }
        self.source.halt();
        tracing::info!(channel = %self.name, "removed");
    }

    /// Halts it, and waits for its source to stop — within the camera's
    /// timeout at most.
    pub(crate) fn stop(&self) {
        self.halt();
        self.source.wait();
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Whether `name` goes into a URL as it is: parts of letters, digits, `-`,
/// `_`, `.` and `~`, between single `/`.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('/').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~'))
        })
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
        for name in ["front door", "/cam", "cam//1", "cam?1", ""] {
            assert!(
                matches!(
                    config(name, "rtsp://10.0.0.1/").check(),
                    Err(ChannelError::Name(_))
                ),
                "{name:?} goes into no URL as it is"
            );
        }
        assert!(config("floor2/cam-3_a.b~c", "rtsp://h/").check().is_ok());
        assert!(config("cam1", "rtsps://h/").check().is_ok());
    }

    #[test]
    fn a_request_body_is_read_with_tcp_by_default() {
        let config: ChannelConfig =
            serde_json::from_str(r#"{"name":"cam1","source":"rtsp://h/"}"#).unwrap();
        assert_eq!(config.transport, Transport::Tcp);
        let config: ChannelConfig =
            serde_json::from_str(r#"{"name":"cam1","source":"rtsp://h/","transport":"udp"}"#)
                .unwrap();
        assert_eq!(config.transport, Transport::Udp);
        assert!(
            serde_json::from_str::<ChannelConfig>(r#"{"name":"c","source":"rtsp://h/","x":1}"#)
                .is_err()
        );
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
