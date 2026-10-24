//! A channel's hub: what its source writes into, and what its sinks read
//! from, each in a form it carries.
//!
//! The source publishes its tracks here at each connection — a run — and
//! writes them into the broadcast it is given back, the hub's own. A sink
//! subscribes with what it carries ([`Profile`]) and is told which
//! broadcast to read ([`Fed`]): the source's own where it carries every
//! track of it, and else one the hub makes for it, converting the tracks it
//! does not carry ([`plan`]). Neither knows what the other is: a source
//! never knows what is converted, and a sink never knows what the source
//! sends.
//!
//! What converts is one pipeline for the hub, reading the source's
//! broadcast once ([`pipeline`]): each track into a `Tee`, on to a
//! converted track's decoder and encoder — once however many sinks read it —
//! and on to the broadcasts that have it. A subscription made or let go of
//! adds or takes away its branches while the pipeline runs, so the other
//! sinks' broadcasts go on undisturbed; a new run of the source builds it
//! anew. It runs only while a sink reads a broadcast made for it: a channel
//! whose sinks carry its tracks costs nothing here. A sink that reads only
//! while it has players ([`Demand::WhileRead`]) has its conversions stopped
//! once nothing has read them for [`IDLE`], and started again when it says
//! a player is coming ([`Subscription::wake`]).

mod media;
mod pipeline;
pub(crate) mod plan;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use media_pp::bus::BusEvent;
use media_pp::elements::{Broadcast, BroadcastOptions, BroadcastSink, BroadcastStats, StreamInfo};
use media_pp::pipeline::Pipeline;
use tokio::runtime::Handle;
use tokio::sync::{Notify, mpsc, watch};

use super::task::Task;
use crate::config::ConvertConfig;
use pipeline::Running;
use plan::{Plan, TrackFrom};
pub(crate) use plan::{Profile, Target};

/// How long a broadcast made for sinks that read only while they have
/// players goes unread, and unwoken, before its conversions stop.
const IDLE: Duration = Duration::from_secs(30);
/// How often that is looked at.
const IDLE_CHECK: Duration = Duration::from_secs(5);

/// What a sink reads: a broadcast, and which run of it.
#[derive(Clone)]
pub(crate) struct Feed {
    pub(crate) broadcast: Broadcast,
    pub(crate) run: u64,
}

impl PartialEq for Feed {
    fn eq(&self, other: &Self) -> bool {
        self.broadcast.name() == other.broadcast.name() && self.run == other.run
    }
}

/// Where a sink's feed stands.
#[derive(Clone, PartialEq)]
pub(crate) enum Fed {
    /// The source not live, or the sink's conversions stopped while it has
    /// no players.
    Idle,
    Live(Feed),
    /// What the sink needs cannot be made, or is not to be: why.
    Failed(Refusal),
}

/// Why what a sink needs is not made.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Refusal {
    /// Its pictures are in a codec it does not carry, on a channel that
    /// converts none: so for as long as the channel is.
    #[error("its pictures, in {codec}, are not carried by {sink} and are not to be converted")]
    NotConverted { sink: &'static str, codec: String },
    /// No GPU encoder opened for pictures a channel converts on the GPU
    /// alone — none there, or each of its sessions taken: one may be free
    /// later.
    #[error("no GPU encoder is free to convert its pictures: {0}")]
    NoGpu(String),
    /// A conversion could not be made.
    #[error("cannot convert: {0}")]
    Failed(String),
}

/// How long a subscription keeps the conversions it needs running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Demand {
    /// For as long as it is held: a recording, or players that reach the
    /// broadcast without asking first.
    Always,
    /// While what is made for it is read, and again from each
    /// [`Subscription::wake`]: players that ask before they play.
    WhileRead,
}

enum Command {
    Subscribe {
        id: u64,
        profile: Profile,
        demand: Demand,
    },
    Unsubscribe(u64),
    Wake(&'static str),
}

/// What the hub's task and its handles share.
struct Shared {
    name: String,
    /// The source's own broadcast.
    original: Broadcast,
    /// Which run of it is live, a count, while one is.
    runs: watch::Sender<Option<u64>>,
    run: AtomicU64,
    commands: mpsc::UnboundedSender<Command>,
    /// Each profile's feed, which each of its subscriptions watches.
    feeds: Mutex<BTreeMap<&'static str, watch::Sender<Fed>>>,
    /// What the pipeline converts now, as the status names it.
    converting: Mutex<Vec<String>>,
    /// How many times pictures to convert were refused for want of a free
    /// GPU encoder.
    gpu_refused: AtomicU64,
    next_subscription: AtomicU64,
    /// Rung where a sink cannot go on with the run live — see
    /// [`Subscription::renew`].
    renew: Notify,
}

/// A channel's hub, run by a task of its own.
pub(crate) struct Hub {
    shared: Arc<Shared>,
    task: Task,
}

/// What a source writes through: each run's tracks published, and the run
/// said to have ended.
#[derive(Clone)]
pub(crate) struct Publisher {
    shared: Arc<Shared>,
}

/// A sink's hold on what it reads: dropped, it lets go of it.
pub(crate) struct Subscription {
    id: u64,
    profile: &'static str,
    shared: Arc<Shared>,
    fed: watch::Receiver<Fed>,
}

impl Hub {
    /// A hub for the channel called `name`, converting what `convert`
    /// allows, its task on `runtime`.
    pub(crate) fn start(name: &str, convert: ConvertConfig, runtime: &Handle) -> Self {
        let (commands, received) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            name: name.to_owned(),
            original: Broadcast::new(name, BroadcastOptions::default()),
            runs: watch::Sender::new(None),
            run: AtomicU64::new(0),
            commands,
            feeds: Mutex::default(),
            converting: Mutex::default(),
            gpu_refused: AtomicU64::new(0),
            next_subscription: AtomicU64::new(0),
            renew: Notify::new(),
        });
        let hubbing = Hubbing {
            shared: Arc::clone(&shared),
            convert,
            subscriptions: BTreeMap::new(),
            sleeping: BTreeSet::new(),
            woken: BTreeMap::new(),
            idle_since: BTreeMap::new(),
            run: None,
            broadcasts: BTreeMap::new(),
            running: None,
        };
        let task = Task::new();
        let runs = shared.runs.subscribe();
        task.spawn(runtime, |halt| hubbing.run(runs, received, halt));
        Self { shared, task }
    }

    /// What the channel's source writes through.
    pub(crate) fn publisher(&self) -> Publisher {
        Publisher {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Which run of the source is live, while one is, and each change of it.
    pub(crate) fn runs(&self) -> watch::Receiver<Option<u64>> {
        self.shared.runs.subscribe()
    }

    /// What the source's own broadcast is read by and has been written.
    pub(crate) fn stats(&self) -> BroadcastStats {
        self.shared.original.stats()
    }

    /// Has a sink carrying `profile` read from now, for as long as `demand`
    /// says: its feed goes live once what it needs is made — at once where
    /// it carries the source's own broadcast.
    pub(crate) fn subscribe(&self, profile: Profile, demand: Demand) -> Subscription {
        let id = self
            .shared
            .next_subscription
            .fetch_add(1, Ordering::Relaxed);
        let fed = lock(&self.shared.feeds)
            .entry(profile.name)
            .or_insert_with(|| watch::Sender::new(Fed::Idle))
            .subscribe();
        let _ = self.shared.commands.send(Command::Subscribe {
            id,
            profile,
            demand,
        });
        Subscription {
            id,
            profile: profile.name,
            shared: Arc::clone(&self.shared),
            fed,
        }
    }

    /// How many times pictures to convert were refused for want of a free
    /// GPU encoder, since it started.
    pub(crate) fn gpu_refused(&self) -> u64 {
        self.shared.gpu_refused.load(Ordering::Relaxed)
    }

    /// Each track converted now, and to what: `"1:opus"`.
    pub(crate) fn conversions(&self) -> Vec<String> {
        lock(&self.shared.converting).clone()
    }

    /// Tells its task to stop, without waiting: what it made stops, and
    /// every broadcast it has is closed, ending what reads them.
    pub(crate) fn halt(&self) {
        self.task.halt();
    }

    /// Halts it and waits for its pipeline to stop.
    pub(crate) async fn wait(&self) {
        self.task.wait().await;
    }
}

impl Publisher {
    /// Publishes a run of `tracks`: what the source writes them into, in
    /// their order.
    pub(crate) fn publish(&self, tracks: &[StreamInfo]) -> Result<Vec<BroadcastSink>, String> {
        let sinks = self
            .shared
            .original
            .publish(tracks)
            .map_err(|error| format!("cannot publish: {error}"))?;
        let run = self.shared.run.fetch_add(1, Ordering::AcqRel) + 1;
        self.shared.runs.send_replace(Some(run));
        Ok(sinks)
    }

    /// The run published last has ended: the connection is gone.
    pub(crate) fn ended(&self) {
        self.shared.runs.send_replace(None);
    }

    /// Waits for a sink to say it cannot go on with the run live: what the
    /// source sends is to be described anew, and a run begun of it.
    pub(crate) async fn renewal(&self) {
        self.shared.renew.notified().await;
    }
}

impl Subscription {
    /// What to read, and each change of it.
    pub(crate) fn fed(&self) -> watch::Receiver<Fed> {
        self.fed.clone()
    }

    /// A player is coming: what is made for it is started again where it
    /// had stopped, and tried again where it could not be made.
    pub(crate) fn wake(&self) {
        let _ = self.shared.commands.send(Command::Wake(self.profile));
    }

    /// It cannot go on with the run live — a recording refused pictures
    /// that changed size partway, which the run's description no longer
    /// holds for — and the source is to begin another, described anew.
    /// Nothing where the source is not listening for it.
    pub(crate) fn renew(&self) {
        self.shared.renew.notify_waiters();
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let _ = self.shared.commands.send(Command::Unsubscribe(self.id));
    }
}

/// The hub's task's own state.
struct Hubbing {
    shared: Arc<Shared>,
    convert: ConvertConfig,
    subscriptions: BTreeMap<u64, (Profile, Demand)>,
    /// Profiles whose conversions stopped for want of players.
    sleeping: BTreeSet<&'static str>,
    /// When each profile was last woken, or subscribed.
    woken: BTreeMap<&'static str, Instant>,
    /// Since when each profile's made broadcast has gone unread.
    idle_since: BTreeMap<&'static str, Instant>,
    /// The source's run live, and its tracks.
    run: Option<(u64, Vec<StreamInfo>)>,
    /// The broadcasts made, by the tracks each has, and the runs each has
    /// been published in: kept across the source's runs, so a sink reads
    /// the same one throughout.
    broadcasts: BTreeMap<Vec<TrackFrom>, (Broadcast, u64)>,
    running: Option<Running>,
}

impl Hubbing {
    async fn run(
        mut self,
        mut runs: watch::Receiver<Option<u64>>,
        mut commands: mpsc::UnboundedReceiver<Command>,
        mut halt: watch::Receiver<bool>,
    ) {
        let mut idle = tokio::time::interval(IDLE_CHECK);
        let live = *runs.borrow_and_update();
        self.follow(live).await;
        loop {
            tokio::select! {
                () = halted(&mut halt) => break,
                changed = runs.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let live = *runs.borrow_and_update();
                    self.follow(live).await;
                }
                command = commands.recv() => {
                    let Some(command) = command else { break };
                    self.take(command);
                    self.apply().await;
                }
                event = bus_of(self.running.as_ref().map(|running| Arc::clone(&running.pipeline))) => {
                    match event {
                        Some(BusEvent::Error { error, .. }) => {
                            tracing::warn!(channel = %self.shared.name, "a conversion failed: {error}");
                        }
                        Some(_) => {}
                        // The pipeline ended by itself: what it wrote is
                        // made again.
                        None => {
                            self.running = None;
                            self.apply().await;
                        }
                    }
                }
                _ = idle.tick() => {
                    if self.put_idle_to_sleep() {
                        self.apply().await;
                    }
                }
            }
        }
        self.stop().await;
        for fed in lock(&self.shared.feeds).values() {
            fed.send_replace(Fed::Idle);
        }
        self.shared.original.close();
        for (broadcast, _) in self.broadcasts.values() {
            broadcast.close();
        }
    }

    fn take(&mut self, command: Command) {
        let now = Instant::now();
        match command {
            Command::Subscribe {
                id,
                profile,
                demand,
            } => {
                self.subscriptions.insert(id, (profile, demand));
                self.woken.insert(profile.name, now);
            }
            Command::Unsubscribe(id) => {
                if let Some((profile, _)) = self.subscriptions.remove(&id)
                    && !self.subscribed(profile.name)
                {
                    self.sleeping.remove(profile.name);
                    self.idle_since.remove(profile.name);
                    self.woken.remove(profile.name);
                }
            }
            Command::Wake(name) => {
                self.woken.insert(name, now);
                self.idle_since.remove(name);
                if self.sleeping.remove(name) {
                    tracing::info!(channel = %self.shared.name, profile = name, "a player is coming; converting again");
                }
            }
        }
    }

    fn subscribed(&self, name: &str) -> bool {
        self.subscriptions
            .values()
            .any(|(profile, _)| profile.name == name)
    }

    /// The profiles read now, each once: those subscribed and not asleep.
    fn profiles(&self) -> Vec<Profile> {
        let mut profiles: BTreeMap<&'static str, Profile> = BTreeMap::new();
        for (profile, _) in self.subscriptions.values() {
            if !self.sleeping.contains(profile.name) {
                profiles.entry(profile.name).or_insert(*profile);
            }
        }
        profiles.into_values().collect()
    }

    /// The source's run is now `live`: what was made for the last one
    /// stops, and what the sinks read is made anew for this one.
    async fn follow(&mut self, live: Option<u64>) {
        self.stop().await;
        self.run = live.and_then(|run| Some((run, self.shared.original.tracks()?)));
        self.apply().await;
    }

    /// Brings what runs, and what each sink reads, to what the profiles read
    /// now need of the source's tracks.
    async fn apply(&mut self) {
        let Some((run, infos)) = self.run.clone() else {
            for fed in lock(&self.shared.feeds).values() {
                fed.send_replace(Fed::Idle);
            }
            self.report();
            return;
        };
        let profiles = self.profiles();
        let plan = Plan::of(&infos, &profiles, self.convert);
        let own: Vec<TrackFrom> = (0..infos.len()).map(TrackFrom::Original).collect();
        let made: BTreeSet<&Vec<TrackFrom>> = plan
            .broadcasts
            .keys()
            .filter(|read| **read != own)
            .collect();

        // What nothing reads any more goes first, so a conversion it alone
        // needed stops.
        if let Some(running) = &mut self.running {
            let gone: Vec<Vec<TrackFrom>> = running
                .written
                .keys()
                .filter(|read| !made.contains(read))
                .cloned()
                .collect();
            for read in gone {
                running.unwrite(&read);
            }
            running.drop_unused(&plan);
        }
        if made.is_empty() {
            self.stop().await;
        }

        let mut failed = plan.refused.clone();
        for (read, names) in &plan.broadcasts {
            if *read == own {
                continue;
            }
            if let Err(reason) = self.write(read, &infos, run) {
                if matches!(reason, Refusal::NoGpu(_)) {
                    self.shared.gpu_refused.fetch_add(1, Ordering::Relaxed);
                }
                tracing::warn!(
                    channel = %self.shared.name,
                    "cannot convert for {}: {reason}",
                    names.iter().copied().collect::<Vec<_>>().join(", ")
                );
                for name in names {
                    failed.insert(name, reason.clone());
                }
            }
        }
        // Nothing could be made: no pipeline reading for nothing.
        if self
            .running
            .as_ref()
            .is_some_and(|running| running.written.is_empty())
        {
            self.stop().await;
        }

        let read: BTreeSet<&'static str> = profiles.iter().map(|profile| profile.name).collect();
        for (name, fed) in lock(&self.shared.feeds).iter() {
            let now = if !read.contains(name) {
                Fed::Idle
            } else if let Some(reason) = failed.get(name) {
                Fed::Failed(reason.clone())
            } else {
                match plan
                    .broadcasts
                    .iter()
                    .find(|(_, names)| names.contains(name))
                {
                    Some((tracks, _)) if *tracks == own => Fed::Live(Feed {
                        broadcast: self.shared.original.clone(),
                        run,
                    }),
                    Some((tracks, _)) => {
                        let (broadcast, published) = &self.broadcasts[tracks];
                        Fed::Live(Feed {
                            broadcast: broadcast.clone(),
                            run: *published,
                        })
                    }
                    None => Fed::Idle,
                }
            };
            fed.send_if_modified(|fed| {
                let changed = *fed != now;
                *fed = now;
                changed
            });
        }
        self.report();
    }

    /// Has the broadcast of `read`'s tracks written, by the pipeline — made
    /// first, where none runs — unless it is already.
    fn write(&mut self, read: &[TrackFrom], infos: &[StreamInfo], run: u64) -> Result<(), Refusal> {
        if self
            .running
            .as_ref()
            .is_some_and(|running| running.written.contains_key(read))
        {
            return Ok(());
        }
        if self.running.is_none() {
            self.running = Some(Running::start(&self.shared.name, &self.shared.original)?);
        }
        let running = self.running.as_mut().expect("just made");
        // Every conversion first: what each makes is part of what the
        // broadcast is published with.
        for track in read {
            if let TrackFrom::Converted(index, target) = *track {
                running.convert(
                    &self.shared.name,
                    index,
                    target,
                    &infos[index],
                    self.convert.video,
                )?;
            }
        }
        let tracks: Vec<StreamInfo> = read
            .iter()
            .map(|track| match *track {
                TrackFrom::Original(index) => infos[index].clone(),
                TrackFrom::Converted(index, target) => {
                    running.conversions[&(index, target)].info.clone()
                }
            })
            .collect();
        let name = &self.shared.name;
        let (broadcast, published) = self.broadcasts.entry(read.to_vec()).or_insert_with(|| {
            (
                Broadcast::new(made_name(name, read), BroadcastOptions::default()),
                0,
            )
        });
        let sinks = broadcast
            .publish(&tracks)
            .map_err(|error| Refusal::Failed(format!("cannot publish: {error}")))?;
        *published += 1;
        running.write(read, sinks)?;
        tracing::info!(channel = %name, broadcast = %broadcast.name(), run, "converting");
        Ok(())
    }

    /// Puts to sleep each profile read only while it has players whose made
    /// broadcast has gone unread, and unwoken, for [`IDLE`]; whether any
    /// was.
    fn put_idle_to_sleep(&mut self) -> bool {
        let now = Instant::now();
        let mut slept = false;
        let feeds = lock(&self.shared.feeds);
        for profile in self.profiles() {
            let on_demand = self
                .subscriptions
                .values()
                .filter(|(subscribed, _)| subscribed.name == profile.name)
                .all(|(_, demand)| *demand == Demand::WhileRead);
            let read = match feeds.get(profile.name).map(|fed| fed.borrow().clone()) {
                // Its own broadcast costs nothing to go on reading.
                Some(Fed::Live(feed))
                    if on_demand && feed.broadcast.name() != self.shared.original.name() =>
                {
                    feed.broadcast.stats().readers
                }
                _ => {
                    self.idle_since.remove(profile.name);
                    continue;
                }
            };
            if read > 0 {
                self.idle_since.remove(profile.name);
                continue;
            }
            let unread = *self.idle_since.entry(profile.name).or_insert(now);
            let since = self
                .woken
                .get(profile.name)
                .map_or(unread, |woken| (*woken).max(unread));
            if now - since >= IDLE {
                tracing::info!(
                    channel = %self.shared.name,
                    profile = profile.name,
                    "no player for {}s; its conversions stop",
                    IDLE.as_secs()
                );
                self.sleeping.insert(profile.name);
                self.idle_since.remove(profile.name);
                slept = true;
            }
        }
        slept
    }

    /// Says what the pipeline converts now, for the status.
    fn report(&self) {
        let now: Vec<String> = self
            .running
            .iter()
            .flat_map(|running| running.conversions.keys())
            .map(|(index, target)| format!("{index}:{}", target.name()))
            .collect();
        *lock(&self.shared.converting) = now;
    }

    /// Stops the pipeline, if one runs: off the runtime's threads, since a
    /// pipeline's stop joins its own.
    async fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            let pipeline = running.pipeline;
            let _ = tokio::task::spawn_blocking(move || pipeline.stop()).await;
        }
        self.report();
    }
}

/// A made broadcast's name, for logs: the channel's, and what is converted.
fn made_name(channel: &str, read: &[TrackFrom]) -> String {
    let converted: Vec<String> = read
        .iter()
        .filter_map(|track| match track {
            TrackFrom::Converted(index, target) => Some(format!("{index}{}", target.name())),
            TrackFrom::Original(_) => None,
        })
        .collect();
    format!("{channel}~{}", converted.join("-"))
}

/// The next event on `pipeline`'s bus — `None` once it has ended — or never,
/// where none runs.
async fn bus_of(pipeline: Option<Arc<Pipeline>>) -> Option<BusEvent> {
    match pipeline {
        Some(pipeline) => pipeline.bus().recv_async().await,
        None => std::future::pending().await,
    }
}

async fn halted(halt: &mut watch::Receiver<bool>) {
    let _ = halt.wait_for(|halted| *halted).await;
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}
