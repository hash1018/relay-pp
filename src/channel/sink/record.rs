//! A channel's recorder: the sink that writes its broadcast to disk, as HLS
//! — see `crate::recordings` for what it writes where.
//!
//! Each recording is a pipeline of its own, `BroadcastSource` → `HlsMuxer`,
//! on a thread of its own as every media-pp pipeline: a disk slow to take a
//! write holds up that camera's recording and no other. A task watches it:
//! it begins one when the camera goes live, ends it when the camera drops —
//! so a recording is never one connection running into the next, whose
//! picture may differ — begins another at each whole chunk of UTC time, and
//! begins again a little after one fails.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use media_pp::bus::BusEvent;
use media_pp::elements::{
    BroadcastSource, HlsMode, HlsMuxer, HlsOptions, HlsSegmentFormat, MuxerTrackError,
};
use media_pp::pipeline::Pipeline;
use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::channel::hub::{Fed, Feed, Subscription};
use crate::channel::task::Task;
use crate::config::RecordConfig;
use crate::recordings::{self, Chunk};

/// How long after a recording failed another is begun.
const AGAIN_AFTER: Duration = Duration::from_secs(5);

/// What a recorder is doing, at one moment.
#[derive(Default, Clone)]
pub(crate) struct RecordStatus {
    /// The playlist of the recording being written, as it is served.
    pub(crate) recording: Option<String>,
    /// Why the last recording failed, until one is written again.
    pub(crate) error: Option<String>,
}

pub(crate) struct Recorder {
    status: Arc<Mutex<RecordStatus>>,
    task: Task,
}

/// What the recorder's task works with.
struct Recording {
    name: String,
    /// The channel's directory under the recordings' root.
    channel: String,
    root: PathBuf,
    segment: Duration,
    chunk: Duration,
    status: Arc<Mutex<RecordStatus>>,
    /// What it records, held for as long as it does; asked for a run
    /// described anew — see [`Stopped::PictureChanged`].
    subscription: Subscription,
}

/// A recording being written.
struct Writing {
    pipeline: Arc<Pipeline>,
    chunk: Chunk,
    /// When the next chunk of UTC time begins, and with it the next
    /// recording.
    ends: Instant,
}

impl Recorder {
    /// Starts recording on `runtime` what `subscription` is fed, whenever a
    /// run of it is live — the channel's broadcast, or one made for it.
    pub(crate) fn start(
        name: &str,
        subscription: Subscription,
        config: &RecordConfig,
        runtime: &Handle,
    ) -> Self {
        let status = Arc::new(Mutex::new(RecordStatus::default()));
        let recording = Arc::new(Recording {
            name: name.to_owned(),
            channel: name.to_owned(),
            root: config.dir.clone(),
            segment: config.segment,
            chunk: config.chunk,
            status: Arc::clone(&status),
            subscription,
        });
        let task = Task::new();
        let fed = recording.subscription.fed();
        task.spawn(runtime, |halt| recording.run(fed, halt));
        Self { status, task }
    }

    pub(crate) fn status(&self) -> RecordStatus {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Tells it to stop, without waiting: the recording under way is
    /// finished, its playlist ended.
    pub(crate) fn halt(&self) {
        self.task.halt();
    }

    /// Halts it and waits for the recording under way to be finished.
    pub(crate) async fn wait(&self) {
        self.task.wait().await;
    }
}

impl Recording {
    /// The run to record, where `fed` says one is live; where it says what
    /// is to be recorded cannot be made, why, in the status.
    fn live_of(&self, fed: &Fed) -> Option<Feed> {
        match fed {
            Fed::Live(feed) => Some(feed.clone()),
            Fed::Failed(reason) => {
                self.status.lock().unwrap_or_else(|p| p.into_inner()).error =
                    Some(reason.to_string());
                None
            }
            Fed::Idle => None,
        }
    }

    async fn run(self: Arc<Self>, mut fed: watch::Receiver<Fed>, mut halt: watch::Receiver<bool>) {
        let mut live = self.live_of(&fed.borrow_and_update());
        let mut writing = None;
        // When to try again after a recording failed.
        let mut again: Option<Instant> = None;
        if let Some(feed) = &live {
            writing = self.begin(feed, &mut again).await;
        }
        loop {
            let watched = writing.as_ref().map(|w: &Writing| Arc::clone(&w.pipeline));
            let ends = writing.as_ref().map(|w| w.ends);
            tokio::select! {
                () = halted(&mut halt) => break,
                changed = fed.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let now = self.live_of(&fed.borrow_and_update());
                    if now != live {
                        live = now;
                        self.end(writing.take()).await;
                        again = None;
                        if let Some(feed) = &live {
                            writing = self.begin(feed, &mut again).await;
                        }
                    }
                }
                () = sleep_until(ends) => {
                    self.end(writing.take()).await;
                    if let Some(feed) = &live {
                        writing = self.begin(feed, &mut again).await;
                    }
                }
                stopped = watch_bus(watched) => {
                    let reason = match &stopped {
                        Stopped::Failed(reason) | Stopped::PictureChanged(reason) => reason.clone(),
                    };
                    tracing::warn!(channel = %self.name, "recording ended: {reason}");
                    self.status.lock().unwrap_or_else(|p| p.into_inner()).error = Some(reason);
                    self.end(writing.take()).await;
                    match stopped {
                        // Begun again on this run, it would be given the
                        // picture it began as and refuse the same way: the
                        // next run describes it anew, and the recording
                        // begins with it.
                        Stopped::PictureChanged(_) => self.subscription.renew(),
                        Stopped::Failed(_) => again = Some(Instant::now() + AGAIN_AFTER),
                    }
                }
                () = sleep_until(again.filter(|_| writing.is_none())) => {
                    again = None;
                    if let Some(feed) = &live {
                        writing = self.begin(feed, &mut again).await;
                    }
                }
            }
        }
        self.end(writing.take()).await;
    }

    /// Begins a recording, or says why it cannot and when to try again.
    async fn begin(self: &Arc<Self>, feed: &Feed, again: &mut Option<Instant>) -> Option<Writing> {
        let recording = Arc::clone(self);
        let feed = feed.clone();
        let begun = tokio::task::spawn_blocking(move || recording.begin_now(&feed)).await;
        let mut status = self.status.lock().unwrap_or_else(|p| p.into_inner());
        match begun {
            Ok(Ok(writing)) => {
                tracing::info!(channel = %self.name, chunk = %writing.chunk.url, "recording");
                status.recording = Some(format!("{}/{}", writing.chunk.url, recordings::PLAYLIST));
                status.error = None;
                Some(writing)
            }
            Ok(Err(reason)) => {
                tracing::warn!(channel = %self.name, "cannot record: {reason}");
                status.error = Some(reason);
                *again = Some(Instant::now() + AGAIN_AFTER);
                None
            }
            Err(_) => {
                status.error = Some("beginning a recording panicked".into());
                *again = Some(Instant::now() + AGAIN_AFTER);
                None
            }
        }
    }

    /// The directory, the muxer and the pipeline of a new recording.
    fn begin_now(&self, feed: &Feed) -> Result<Writing, String> {
        let now = SystemTime::now();
        let chunk = recordings::new_chunk(&self.root, &self.channel, now).map_err(|error| {
            format!(
                "cannot make a directory in {}: {error}",
                self.root.display()
            )
        })?;
        let made = self.writing_into(feed, &chunk, now);
        if made.is_err() {
            let _ = std::fs::remove_dir_all(&chunk.path);
        }
        made
    }

    fn writing_into(&self, feed: &Feed, chunk: &Chunk, now: SystemTime) -> Result<Writing, String> {
        let (source, tracks) =
            BroadcastSource::new(format!("{}-record", self.name), &feed.broadcast)
                .map_err(|error| error.to_string())?;
        let mut options = HlsOptions::new(
            chunk.path.join(recordings::PLAYLIST),
            chunk.path.join(recordings::MEDIA),
        );
        options.segment_duration = self.segment;
        options.mode = HlsMode::Event;
        options.segment_format = HlsSegmentFormat::Fmp4;
        options.single_file = true;
        let mut muxer = HlsMuxer::create(options).map_err(|error| error.to_string())?;
        let mut tracked = Vec::new();
        for (index, track) in tracks.iter().enumerate() {
            let added = muxer
                .add_stream(format!("track{index}"), track.clone())
                .map_err(|error| error.to_string())?;
            tracked.push(added);
        }
        let mut sinks = muxer.open().map_err(|error| error.to_string())?;
        let (pipeline, ()) =
            Pipeline::new(format!("{}-record", self.name), source, |source, ctx| {
                for (index, track) in tracked.into_iter().enumerate() {
                    ctx.attach(source, index, ctx.branch().to(sinks.take(track)?)?)?;
                }
                Ok(())
            })
            .map_err(|error| error.to_string())?;
        pipeline.run().map_err(|error| error.to_string())?;
        Ok(Writing {
            pipeline,
            chunk: chunk.clone(),
            ends: Instant::now() + until_next(now, self.chunk),
        })
    }

    /// Finishes a recording: what it has read is written, and its playlist
    /// ended.
    async fn end(&self, writing: Option<Writing>) {
        let Some(writing) = writing else {
            return;
        };
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .recording = None;
        let pipeline = writing.pipeline;
        let _ = tokio::task::spawn_blocking(move || pipeline.finish()).await;
        tracing::info!(channel = %self.name, chunk = %writing.chunk.url, "recorded");
    }
}

/// How long from `now` until the next whole multiple of `chunk` since the
/// Unix epoch.
fn until_next(now: SystemTime, chunk: Duration) -> Duration {
    let since = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let chunk = chunk.as_millis().max(1);
    Duration::from_millis((chunk - since % chunk) as u64)
}

/// Waits for `halt` to say halted. What `wait_for` gives back holds the
/// watch's lock, which a `select!` would keep across its other arms' awaits.
async fn halted(halt: &mut watch::Receiver<bool>) {
    let _ = halt.wait_for(|halted| *halted).await;
}

/// Sleeps until `at`, or for ever where there is none.
async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Why a recording's pipeline ended, as its bus says; never, where there is
/// none.
async fn watch_bus(pipeline: Option<Arc<Pipeline>>) -> Stopped {
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

/// Why a recording stopped.
enum Stopped {
    /// It failed — a full disk, say — and is begun again after a while.
    Failed(String),
    /// The camera's pictures changed shape — size, profile — partway
    /// through its connection, which the recording, begun as the connection
    /// began, refuses.
    PictureChanged(String),
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

    #[test]
    fn a_chunk_ends_at_the_next_whole_multiple_of_its_length() {
        let at = |seconds| UNIX_EPOCH + Duration::from_secs(seconds);
        let hour = Duration::from_secs(3600);
        assert_eq!(until_next(at(1_792_764_000), hour), hour);
        assert_eq!(
            until_next(at(1_792_764_000 + 3599), hour),
            Duration::from_secs(1)
        );
        assert_eq!(
            until_next(
                at(1_792_764_000) + Duration::from_millis(1500),
                Duration::from_secs(4)
            ),
            Duration::from_millis(2500)
        );
    }

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
