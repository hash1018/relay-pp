//! A channel's HLS live: the sink that writes its broadcast as a live HLS
//! playlist and its segments, into a directory of its own, for the HTTP
//! port to serve — see `docs/status.md`, "Players".
//!
//! It is written only while asked for: begun by the first request for the
//! playlist, and ended once no request has come — for the playlist or a
//! segment — for the server's `[hls] idle`, its directory removed. Like a
//! recording, each run of the camera is a pipeline of its own,
//! `BroadcastSource` → `HlsMuxer`, on its own thread; unlike one, nothing
//! of it is kept: a sliding window of short segments, each deleted as it
//! leaves the playlist.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use media_pp::elements::{BroadcastSource, HlsMode, HlsMuxer, HlsOptions, HlsSegmentFormat};
use media_pp::pipeline::Pipeline;
use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::time::Instant;

use super::follow::{halted, sleep_until, watch_bus};
use crate::channel::hub::{Fed, Feed, Refusal, Subscription};
use crate::channel::task::Task;

/// The playlist's name in its directory, and in its URL.
pub(crate) const PLAYLIST: &str = "index.m3u8";

/// How long a segment is, cut at the next keyframe after it: what a player
/// starts behind the camera by, three of them as players go.
const SEGMENT: Duration = Duration::from_secs(1);

/// How many segments the playlist lists.
const WINDOW: usize = 6;

/// How long after its pipeline failed another is begun.
const AGAIN_AFTER: Duration = Duration::from_secs(2);

/// How often it looks whether it is still asked for.
const IDLE_CHECK: Duration = Duration::from_millis(500);

/// Numbers the directories of every HLS live in this process.
static NEXT: AtomicU64 = AtomicU64::new(0);

/// Why players cannot read a channel's HLS yet, or at all.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NotWritten {
    /// No playlist yet: the camera's next keyframe, and a segment after
    /// it, not come yet.
    #[error("not ready yet")]
    NotYet,
    /// What it reads is not made, and why.
    #[error(transparent)]
    Refused(Refusal),
    /// Its pipeline failed, and is begun again shortly.
    #[error("{0}")]
    Failed(String),
}

pub(crate) struct HlsLive {
    shared: Arc<Shared>,
    task: Task,
}

/// What a live and its task share.
struct Shared {
    name: String,
    dir: PathBuf,
    /// When it was last asked for.
    asked: Mutex<Instant>,
    /// How long it goes unasked for before it ends.
    idle: Duration,
    /// Set as its task ends: a request after it begins another.
    over: AtomicBool,
    /// Why its last pipeline failed, until one writes again.
    error: Mutex<Option<String>>,
    /// What it writes, held while it does; asked for a run described anew
    /// where pictures change shape, as a recording asks.
    subscription: Subscription,
}

impl HlsLive {
    /// Starts writing on `runtime` what `subscription` is fed, whenever a
    /// run of it is live, into a new directory under the system's
    /// temporary one.
    pub(crate) fn start(
        name: &str,
        subscription: Subscription,
        idle: Duration,
        runtime: &Handle,
    ) -> Self {
        let dir = std::env::temp_dir()
            .join(format!("relay-pp-{}", std::process::id()))
            .join(format!(
                "hls-{}-{name}",
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        let shared = Arc::new(Shared {
            name: name.to_owned(),
            dir,
            asked: Mutex::new(Instant::now()),
            idle,
            over: AtomicBool::new(false),
            error: Mutex::new(None),
            subscription,
        });
        let task = Task::new();
        let fed = shared.subscription.fed();
        let running = Arc::clone(&shared);
        task.spawn(runtime, |halt| running.run(fed, halt));
        Self { shared, task }
    }

    /// Where its files are, its being asked for counted.
    pub(crate) fn asked(&self) -> &Path {
        *lock(&self.shared.asked) = Instant::now();
        &self.shared.dir
    }

    /// Whether it has ended, for want of being asked for or halted.
    pub(crate) fn is_over(&self) -> bool {
        self.shared.over.load(Ordering::Acquire)
    }

    /// Waits, up to `within`, for its playlist to be written — at once
    /// where it is — or says why it is not.
    pub(crate) async fn ready(&self, within: Duration) -> Result<(), NotWritten> {
        let deadline = Instant::now() + within;
        let playlist = self.shared.dir.join(PLAYLIST);
        let mut fed = self.shared.subscription.fed();
        loop {
            if tokio::fs::try_exists(&playlist).await.unwrap_or(false) {
                return Ok(());
            }
            if let Fed::Failed(refusal) = &*fed.borrow_and_update() {
                return Err(NotWritten::Refused(refusal.clone()));
            }
            if Instant::now() >= deadline {
                return Err(match lock(&self.shared.error).clone() {
                    Some(error) => NotWritten::Failed(error),
                    None => NotWritten::NotYet,
                });
            }
            *lock(&self.shared.asked) = Instant::now();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Tells it to stop, without waiting.
    pub(crate) fn halt(&self) {
        self.task.halt();
    }

    /// Halts it and waits for its pipeline to stop and its files to go.
    pub(crate) async fn wait(&self) {
        self.task.wait().await;
    }
}

impl Shared {
    async fn run(self: Arc<Self>, mut fed: watch::Receiver<Fed>, mut halt: watch::Receiver<bool>) {
        let live_of = |fed: &Fed| match fed {
            Fed::Live(feed) => Some(feed.clone()),
            Fed::Idle | Fed::Failed(_) => None,
        };
        let mut live = live_of(&fed.borrow_and_update());
        let mut writing: Option<Arc<Pipeline>> = None;
        // How many pipelines it has begun: each names its segments apart
        // from the last's, which a player may still be reading.
        let mut begun = 0u64;
        let mut again: Option<Instant> = None;
        let mut idle = tokio::time::interval(IDLE_CHECK);
        if let Some(feed) = &live {
            writing = self.begin(feed, &mut begun, &mut again).await;
        }
        loop {
            let watched = writing.clone();
            tokio::select! {
                () = halted(&mut halt) => break,
                changed = fed.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let now = live_of(&fed.borrow_and_update());
                    if now != live {
                        live = now;
                        self.end(writing.take()).await;
                        again = None;
                        if let Some(feed) = &live {
                            writing = self.begin(feed, &mut begun, &mut again).await;
                        }
                    }
                }
                _ = idle.tick() => {
                    if lock(&self.asked).elapsed() >= self.idle {
                        tracing::info!(channel = %self.name, "HLS no longer asked for");
                        break;
                    }
                }
                stopped = watch_bus(watched) => {
                    let reason = stopped.reason();
                    tracing::warn!(channel = %self.name, "HLS ended: {reason}");
                    *lock(&self.error) = Some(reason.to_owned());
                    self.end(writing.take()).await;
                    again = stopped.follow_up(&self.subscription, AGAIN_AFTER);
                }
                () = sleep_until(again.filter(|_| writing.is_none())) => {
                    again = None;
                    if let Some(feed) = &live {
                        writing = self.begin(feed, &mut begun, &mut again).await;
                    }
                }
            }
        }
        self.over.store(true, Ordering::Release);
        self.end(writing.take()).await;
        let dir = self.dir.clone();
        let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(dir)).await;
    }

    /// Begins a pipeline writing `feed`, or says when to try again.
    async fn begin(
        self: &Arc<Self>,
        feed: &Feed,
        begun: &mut u64,
        again: &mut Option<Instant>,
    ) -> Option<Arc<Pipeline>> {
        let shared = Arc::clone(self);
        let feed = feed.clone();
        let index = *begun;
        *begun += 1;
        let made = tokio::task::spawn_blocking(move || shared.begin_now(&feed, index)).await;
        match made {
            Ok(Ok(pipeline)) => {
                tracing::info!(channel = %self.name, "HLS written");
                *lock(&self.error) = None;
                Some(pipeline)
            }
            Ok(Err(reason)) => {
                tracing::warn!(channel = %self.name, "cannot write HLS: {reason}");
                *lock(&self.error) = Some(reason);
                *again = Some(Instant::now() + AGAIN_AFTER);
                None
            }
            Err(_) => {
                *lock(&self.error) = Some("beginning HLS panicked".into());
                *again = Some(Instant::now() + AGAIN_AFTER);
                None
            }
        }
    }

    fn begin_now(&self, feed: &Feed, index: u64) -> Result<Arc<Pipeline>, String> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|error| format!("cannot make {}: {error}", self.dir.display()))?;
        let (source, tracks) = BroadcastSource::new(format!("{}-hls", self.name), &feed.broadcast)
            .map_err(|error| error.to_string())?;
        let mut options = HlsOptions::new(
            self.dir.join(PLAYLIST),
            self.dir.join(format!("{index}-%05d.m4s")),
        );
        options.segment_duration = SEGMENT;
        options.mode = HlsMode::Live {
            window_size: WINDOW,
            delete_old_segments: true,
        };
        options.segment_format = HlsSegmentFormat::Fmp4;
        options.init_filename = format!("{index}-init.mp4");
        let mut muxer = HlsMuxer::create(options).map_err(|error| error.to_string())?;
        let mut tracked = Vec::new();
        for (track_index, track) in tracks.iter().enumerate() {
            tracked.push(
                muxer
                    .add_stream(format!("track{track_index}"), track.clone())
                    .map_err(|error| error.to_string())?,
            );
        }
        let mut sinks = muxer.open().map_err(|error| error.to_string())?;
        let (pipeline, ()) = Pipeline::new(format!("{}-hls", self.name), source, |source, ctx| {
            for (track_index, track) in tracked.into_iter().enumerate() {
                ctx.attach(source, track_index, ctx.branch().to(sinks.take(track)?)?)?;
            }
            Ok(())
        })
        .map_err(|error| error.to_string())?;
        pipeline.run().map_err(|error| error.to_string())?;
        Ok(pipeline)
    }

    /// Stops a pipeline: nothing it holds is kept, nor its playlist ended —
    /// the next run's goes on in its place.
    async fn end(&self, pipeline: Option<Arc<Pipeline>>) {
        if let Some(pipeline) = pipeline {
            let _ = tokio::task::spawn_blocking(move || pipeline.stop()).await;
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

/// Whether `file` is one an HLS live writes: a segment or an
/// initialization section, by a name of its own making — no path.
pub(crate) fn is_media_file(file: &str) -> bool {
    (file.ends_with(".m4s") || file.ends_with(".mp4"))
        && !file.starts_with('.')
        && file
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_file_of_its_own_making_is_served() {
        for file in ["0-00001.m4s", "3-init.mp4"] {
            assert!(is_media_file(file), "{file}");
        }
        for file in [
            "../0-00001.m4s",
            "..\\x.m4s",
            "a/b.m4s",
            ".m4s",
            "index.m3u8",
            "x.m4s.exe",
            "",
        ] {
            assert!(!is_media_file(file), "{file:?}");
        }
    }
}
