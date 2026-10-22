//! A channel: one [`Source`](source::Source) writing a stream into a
//! `Broadcast`, and the [`Sink`](sink::Sink)s handing it out — see
//! `docs/design.md`, "The model".
//!
//! Neither is media-pp's trait of the same name. A relay-pp source is
//! whatever takes a stream in, over as many media-pp pipelines as its input
//! takes connections; a sink is whatever reads the broadcast.

mod sink;
mod source;

use std::time::SystemTime;

use serde::Serialize;

use crate::config::{ChannelConfig, redact};
pub(crate) use sink::Listeners;
use sink::Sink;
use source::Source;
pub use source::SourceState;

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

/// Why a channel could not be started.
#[derive(Debug, thiserror::Error)]
#[error("channel {name}: {error}")]
pub struct ChannelError {
    name: String,
    error: String,
}

/// A channel being served.
pub struct Channel {
    name: String,
    source: Source,
    sinks: Vec<Sink>,
}

impl Channel {
    /// Attaches a channel's sinks to every listener and starts its source.
    /// Nothing is left attached, and nothing opened, where one sink cannot
    /// be attached.
    pub(crate) fn start(
        name: &str,
        config: &ChannelConfig,
        listeners: &Listeners,
    ) -> Result<Self, ChannelError> {
        let source = Source::new(name, config);
        let sinks = Sink::attach_all(name, source.broadcast(), listeners).map_err(|error| {
            ChannelError {
                name: name.to_owned(),
                error,
            }
        })?;
        source.run();
        tracing::info!(channel = %name, source = %redact(&config.source), "serving");
        Ok(Self {
            name: name.to_owned(),
            source,
            sinks,
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

    /// Detaches every sink, then stops the source.
    pub(crate) fn stop(&self) {
        for sink in &self.sinks {
            sink.detach(&self.name);
        }
        self.source.stop();
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        self.stop();
    }
}
