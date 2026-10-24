//! What a sink follows its hub with: waits that a halt cuts short, and why
//! a pipeline writing what the hub gives it ended, and what is done next.
//!
//! Each sink keeps its own loop — what begins a pipeline again differs: a
//! player coming, a chunk of time ending, a request for a playlist — and
//! takes from here what every loop waits on.

use std::sync::Arc;
use std::time::Duration;

use media_pp::bus::BusEvent;
use media_pp::elements::MuxerTrackError;
use media_pp::pipeline::Pipeline;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::channel::hub::Subscription;

/// Waits for `halt` to say halted. What `wait_for` gives back holds the
/// watch's lock, which a `select!` would keep across its other arms' awaits.
pub(super) async fn halted(halt: &mut watch::Receiver<bool>) {
    let _ = halt.wait_for(|halted| *halted).await;
}

/// Waits `wait`, or until halted: whether it was.
pub(super) async fn wait_or_halted(wait: Duration, halt: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        () = tokio::time::sleep(wait) => false,
        () = halted(halt) => true,
    }
}

/// Sleeps until `at`, or for ever where there is none.
pub(super) async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Why a pipeline writing what a sink reads ended, as its bus says; never,
/// where there is none.
pub(super) async fn watch_bus(pipeline: Option<Arc<Pipeline>>) -> Stopped {
    let Some(pipeline) = pipeline else {
        return std::future::pending().await;
    };
    loop {
        match pipeline.bus().recv_async().await {
            Some(BusEvent::Error { error, .. }) if picture_changed(&error) => {
                return Stopped::PictureChanged(format!(
                    "{error}; the camera is connected again to record what it sends now"
                ));
            }
            Some(BusEvent::Error { error, .. }) => return Stopped::Failed(error.to_string()),
            // The broadcast was closed, or published again with tracks it
            // could not go on with.
            Some(BusEvent::Finished) | None => {
                return Stopped::Failed("its broadcast ended".to_owned());
            }
            Some(_) => {}
        }
    }
}

/// Why a sink's pipeline stopped.
pub(super) enum Stopped {
    /// It failed — a full disk, say — and is begun again after a while.
    Failed(String),
    /// The camera's pictures changed shape — size, profile — partway
    /// through its connection, which a muxer, begun as the connection
    /// began, refuses.
    PictureChanged(String),
}

impl Stopped {
    /// Why, as the log and the status say it.
    pub(super) fn reason(&self) -> &str {
        match self {
            Stopped::Failed(reason) | Stopped::PictureChanged(reason) => reason,
        }
    }

    /// What follows, for what `subscription` reads: when to begin again,
    /// `again_after` from now, after a failure; nothing where the pictures
    /// changed. Begun again on this run, the pipeline would be given the
    /// picture it began as and refuse the same way: the next run describes
    /// it anew, which `subscription` asks for, and it begins with that.
    pub(super) fn follow_up(
        &self,
        subscription: &Subscription,
        again_after: Duration,
    ) -> Option<Instant> {
        match self {
            Stopped::PictureChanged(_) => {
                subscription.renew();
                None
            }
            Stopped::Failed(_) => Some(Instant::now() + again_after),
        }
    }
}

/// Whether `error` is a muxer refusing pictures that changed shape.
fn picture_changed(error: &media_pp::Error) -> bool {
    match error {
        media_pp::Error::Traced(traced) => picture_changed(&traced.source),
        media_pp::Error::MuxerTrackError(MuxerTrackError::FormatChanged { .. }) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A muxer refusing changed pictures is told apart, however many
    /// elements traced it on its way to the bus, from any other failure.
    #[test]
    fn a_picture_that_changed_is_told_from_other_failures() {
        let changed = || {
            media_pp::Error::from(MuxerTrackError::FormatChanged {
                track: "track0".into(),
                was: "320x240".into(),
                now: "640x480".into(),
            })
        };
        assert!(picture_changed(&changed()));
        let traced = changed().traced_at(media_pp::element::ElementType::Other, "muxer".into());
        assert!(picture_changed(&traced));
        assert!(!picture_changed(&media_pp::Error::Other(
            "disk full".into()
        )));
    }
}
