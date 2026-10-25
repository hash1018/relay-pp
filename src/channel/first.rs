//! Says when each track's first packet passes a point of a pipeline, and
//! stamped when: where one track of a recording begins later than another,
//! the log shows at which point the gap opened — what the converting
//! pipeline read, what it wrote into a made broadcast, what a recording
//! read from it.

use std::sync::Arc;

use media_pp::buffer::MediaBuffer;
use media_pp::element::{Filter, Output};

/// Hands every packet on, and logs the first of each run — the first after
/// a start or a `Flush` — with its timestamp in seconds and whether it is a
/// keyframe. The log's own time says when it passed.
pub(crate) struct FirstPacket {
    channel: Arc<str>,
    point: &'static str,
    track: usize,
    told: bool,
}

impl FirstPacket {
    pub(crate) fn new(channel: &str, point: &'static str, track: usize) -> Self {
        Self {
            channel: channel.into(),
            point,
            track,
            told: false,
        }
    }
}

impl Filter for FirstPacket {
    fn transform(&mut self, buf: MediaBuffer, out: &mut Output) -> media_pp::Result<()> {
        if !self.told {
            self.told = true;
            let at = buf
                .pts()
                .zip(buf.time_base())
                .map(|(pts, unit)| pts as f64 * f64::from(unit));
            let key = match &buf {
                MediaBuffer::Packet(packet) => packet.is_key(),
                _ => true,
            };
            tracing::info!(
                channel = %self.channel,
                point = self.point,
                track = self.track,
                at = ?at,
                key,
                "first packet"
            );
        }
        out.push(buf);
        Ok(())
    }

    fn reset(&mut self) {
        self.told = false;
    }
}
