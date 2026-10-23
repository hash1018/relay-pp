//! A channel's conversions: the broadcasts made for outputs that cannot
//! carry what its source sends, and the pipeline that writes them.
//!
//! An output reads the source's own broadcast where it carries every track
//! of it, and one made for it where it does not ([`super::plan`]). What
//! makes those is one pipeline for the channel, reading the source's
//! broadcast once: each track goes into a `Tee`, on to a converted track's
//! decoder and encoder — once however many outputs read it — and on to the
//! broadcasts that have it. An output read or let go of adds or takes away
//! its branches while the pipeline runs, so the others' broadcasts go on
//! undisturbed; a new run of the source builds it anew.
//!
//! The pipeline runs only while an output reads a broadcast made for it: a
//! channel whose outputs carry its tracks costs nothing here. A WebRTC
//! output is read while players play it, and let go of once none has for
//! [`IDLE`]; a recording, from when it is attached until it is detached.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use media_pp::buffer::MediaBuffer;
use media_pp::bus::BusEvent;
use media_pp::element::{Context, Filter, Output as Pushed};
use media_pp::elements::{
    AudioCodec, Broadcast, BroadcastOptions, BroadcastSource, DecodeTarget, EncodeInput,
    StreamInfo, SwAudioEncoder, SwAudioEncoderOptions, SwDecoder, TeeHandle, VideoDecodeBin,
    VideoEncodeBin, VideoEncodeOptions,
};
use media_pp::ffmpeg::Rational;
use media_pp::graph::BranchId;
use media_pp::pipeline::Pipeline;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};

use super::plan::{Output, Plan, Target, Track, TrackFrom};
use super::task::Task;

/// How long a WebRTC output's broadcast goes unread, and unasked for,
/// before its conversions stop.
const IDLE: Duration = Duration::from_secs(30);
/// How often that is looked at.
const IDLE_CHECK: Duration = Duration::from_secs(5);
/// What a converted track's branch may hold before its decoder, as packets.
const CONVERT_QUEUE: usize = 64;
/// What a converted track's pictures may be held by after its decoder: the
/// encoder's own, as many as a GPU encoder keeps while it works.
const DOWNSTREAM_FRAMES: i32 = 4;

/// What an output reads: a broadcast, and which run of it.
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

/// Where an output's feed stands.
#[derive(Clone, PartialEq)]
pub(crate) enum Fed {
    /// Not read, or its source not live.
    Idle,
    Live(Feed),
    /// Read, and what it needs cannot be made: why.
    Failed(String),
}

enum Command {
    Want(Output),
    Unwant(Output),
}

/// A channel's conversions, run by a task of their own.
pub(crate) struct Converter {
    commands: mpsc::UnboundedSender<Command>,
    /// What its pipeline converts now, as the status names it.
    converting: Arc<Mutex<Vec<String>>>,
    webrtc: watch::Receiver<Fed>,
    hls: watch::Receiver<Fed>,
    task: Task,
}

impl Converter {
    /// Starts following `original` — the source's broadcast — and the
    /// runs `runs` says it is in, on `runtime`.
    pub(crate) fn start(
        name: &str,
        original: &Broadcast,
        runs: watch::Receiver<Option<u64>>,
        runtime: &Handle,
    ) -> Self {
        let (commands, received) = mpsc::unbounded_channel();
        let (webrtc_fed, webrtc) = watch::channel(Fed::Idle);
        let (hls_fed, hls) = watch::channel(Fed::Idle);
        let converting = Arc::new(Mutex::new(Vec::new()));
        let state = Converting {
            name: name.to_owned(),
            converting: Arc::clone(&converting),
            original: original.clone(),
            fed: BTreeMap::from([(Output::WebRtc, webrtc_fed), (Output::Hls, hls_fed)]),
            wanted: BTreeSet::new(),
            asked: None,
            idle_since: None,
            run: None,
            broadcasts: BTreeMap::new(),
            running: None,
        };
        let task = Task::new();
        task.spawn(runtime, |halt| state.run(runs, received, halt));
        Self {
            commands,
            converting,
            webrtc,
            hls,
            task,
        }
    }

    /// Has `output` read from now: its feed goes live once what it needs is
    /// made — at once where it carries the source's own broadcast.
    pub(crate) fn want(&self, output: Output) {
        let _ = self.commands.send(Command::Want(output));
    }

    pub(crate) fn unwant(&self, output: Output) {
        let _ = self.commands.send(Command::Unwant(output));
    }

    /// What WebRTC players read, and each change of it. RTSP's is never
    /// here: it carries what came in, and reads the source's broadcast.
    pub(crate) fn webrtc(&self) -> watch::Receiver<Fed> {
        self.webrtc.clone()
    }

    /// What a recording reads, and each change of it.
    pub(crate) fn hls(&self) -> watch::Receiver<Fed> {
        self.hls.clone()
    }

    /// Each track converted now, and to what: `"1:opus"`.
    pub(crate) fn conversions(&self) -> Vec<String> {
        self.converting
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub(crate) fn halt(&self) {
        self.task.halt();
    }

    /// Halts it and waits for its pipeline to stop.
    pub(crate) async fn wait(&self) {
        self.task.wait().await;
    }
}

/// The converter's task's own state.
struct Converting {
    name: String,
    converting: Arc<Mutex<Vec<String>>>,
    original: Broadcast,
    fed: BTreeMap<Output, watch::Sender<Fed>>,
    wanted: BTreeSet<Output>,
    /// When WebRTC was last asked for.
    asked: Option<Instant>,
    /// Since when WebRTC's broadcast has gone unread.
    idle_since: Option<Instant>,
    /// The source's run live, and its tracks.
    run: Option<(u64, Vec<StreamInfo>)>,
    /// The broadcasts made, by the tracks each has, and the runs each has
    /// been published in: kept across the source's runs, so an output reads
    /// the same one throughout.
    broadcasts: BTreeMap<Vec<TrackFrom>, (Broadcast, u64)>,
    running: Option<Running>,
}

/// The pipeline writing the broadcasts made, for one run of the source.
struct Running {
    pipeline: Arc<Pipeline>,
    /// What builds a branch's own `Tee` while it runs.
    context: Arc<Context>,
    /// Each source track's `Tee`.
    inputs: Vec<TeeHandle>,
    conversions: BTreeMap<(usize, Target), Conversion>,
    /// Each broadcast written, by its tracks: the branches into it.
    written: BTreeMap<Vec<TrackFrom>, Vec<(TeeHandle, BranchId)>>,
}

/// A track converted: its branch off the track's `Tee`, and the `Tee` its
/// packets come out of.
struct Conversion {
    branch: BranchId,
    out: TeeHandle,
    info: StreamInfo,
}

impl Converting {
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
                    match command {
                        Command::Want(output) => {
                            if output == Output::WebRtc {
                                self.asked = Some(Instant::now());
                            }
                            self.wanted.insert(output);
                        }
                        Command::Unwant(output) => {
                            self.wanted.remove(&output);
                        }
                    }
                    self.apply().await;
                }
                event = bus_of(self.running.as_ref().map(|running| Arc::clone(&running.pipeline))) => {
                    match event {
                        Some(BusEvent::Error { error, .. }) => {
                            tracing::warn!(channel = %self.name, "a conversion failed: {error}");
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
                    if self.webrtc_idle() {
                        tracing::info!(channel = %self.name, "no WebRTC player for {}s; its conversions stop", IDLE.as_secs());
                        self.wanted.remove(&Output::WebRtc);
                        self.apply().await;
                    }
                }
            }
        }
        self.stop().await;
        for fed in self.fed.values() {
            fed.send_replace(Fed::Idle);
        }
    }

    /// The source's run is now `live`: what was made for the last one
    /// stops, and what the outputs read is made anew for this one.
    async fn follow(&mut self, live: Option<u64>) {
        self.stop().await;
        self.run = live.and_then(|run| Some((run, self.original.tracks()?)));
        self.apply().await;
    }

    /// Brings what runs, and what each output reads, to what the outputs
    /// wanted now need of the source's tracks.
    async fn apply(&mut self) {
        let Some((run, infos)) = self.run.clone() else {
            for fed in self.fed.values() {
                fed.send_replace(Fed::Idle);
            }
            return;
        };
        let tracks: Vec<Option<Track>> = infos.iter().map(Track::of).collect();
        let wanted: BTreeSet<Output> = self
            .wanted
            .iter()
            .copied()
            .filter(|output| *output != Output::Rtsp)
            .collect();
        let plan = Plan::of(&tracks, &wanted);
        let own: Vec<TrackFrom> = (0..infos.len()).map(TrackFrom::Original).collect();
        let made: BTreeSet<&Vec<TrackFrom>> = plan
            .broadcasts
            .keys()
            .filter(|read| **read != own)
            .collect();

        // What no output reads any more goes first, so a conversion it
        // alone needed stops.
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

        let mut failed: BTreeMap<Output, String> = BTreeMap::new();
        for (read, outputs) in &plan.broadcasts {
            if *read == own {
                continue;
            }
            if let Err(reason) = self.write(read, &infos, run) {
                tracing::warn!(channel = %self.name, "cannot convert for {}: {reason}", names(outputs));
                for output in outputs {
                    failed.insert(*output, reason.clone());
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

        for (output, fed) in &self.fed {
            let now = if !wanted.contains(output) {
                Fed::Idle
            } else if let Some(reason) = failed.get(output) {
                Fed::Failed(reason.clone())
            } else {
                let read = plan
                    .broadcasts
                    .iter()
                    .find(|(_, outputs)| outputs.contains(output))
                    .map(|(read, _)| read);
                match read {
                    Some(read) if *read == own => Fed::Live(Feed {
                        broadcast: self.original.clone(),
                        run,
                    }),
                    Some(read) => {
                        let (broadcast, published) = &self.broadcasts[read];
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

    /// Says what the pipeline converts now, for the status.
    fn report(&self) {
        let now: Vec<String> = self
            .running
            .iter()
            .flat_map(|running| running.conversions.keys())
            .map(|(index, target)| format!("{index}:{}", target_name(*target)))
            .collect();
        *self.converting.lock().unwrap_or_else(|p| p.into_inner()) = now;
    }

    /// Has the broadcast of `read`'s tracks written, by the pipeline — made
    /// first, where none runs — unless it is already.
    fn write(&mut self, read: &[TrackFrom], infos: &[StreamInfo], run: u64) -> Result<(), String> {
        if self
            .running
            .as_ref()
            .is_some_and(|running| running.written.contains_key(read))
        {
            return Ok(());
        }
        if self.running.is_none() {
            self.running = Some(Running::start(&self.name, &self.original)?);
        }
        let running = self.running.as_mut().expect("just made");
        // Every conversion first: what each makes is part of what the
        // broadcast is published with.
        for track in read {
            if let TrackFrom::Converted(index, target) = *track {
                running.convert(&self.name, index, target, &infos[index])?;
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
        let (broadcast, published) = self.broadcasts.entry(read.to_vec()).or_insert_with(|| {
            (
                Broadcast::new(made_name(&self.name, read), BroadcastOptions::default()),
                0,
            )
        });
        let sinks = broadcast
            .publish(&tracks)
            .map_err(|error| format!("cannot publish: {error}"))?;
        *published += 1;
        let mut branches = Vec::new();
        for (track, sink) in read.iter().zip(sinks) {
            let tee = match *track {
                TrackFrom::Original(index) => &running.inputs[index],
                TrackFrom::Converted(index, target) => &running.conversions[&(index, target)].out,
            };
            let branch = tee
                .branch()
                .and_then(|chain| chain.to(sink))
                .and_then(|branch| tee.attach(branch))
                .map_err(|error| format!("cannot wire the broadcast: {error}"))?;
            branches.push((tee.clone(), branch));
        }
        running.written.insert(read.to_vec(), branches);
        tracing::info!(channel = %self.name, broadcast = %broadcast.name(), run, "converting");
        Ok(())
    }

    /// Whether WebRTC's broadcast, one made for it, has gone unread and
    /// unasked for for [`IDLE`].
    fn webrtc_idle(&mut self) -> bool {
        let read = match &*self.fed[&Output::WebRtc].borrow() {
            Fed::Live(feed) if feed.broadcast.name() != self.original.name() => {
                feed.broadcast.stats().readers
            }
            // Its own broadcast costs nothing to go on reading.
            _ => {
                self.idle_since = None;
                return false;
            }
        };
        if read > 0 {
            self.idle_since = None;
            return false;
        }
        // Unread since it was last seen read, or last asked for, whichever
        // came later: a player asking just now is about to read it.
        let now = Instant::now();
        let unread = *self.idle_since.get_or_insert(now);
        let since = self.asked.map_or(unread, |asked| asked.max(unread));
        now - since >= IDLE
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

impl Running {
    /// A pipeline reading `original`, each of its tracks into a `Tee` of
    /// its own with nothing after it yet.
    fn start(name: &str, original: &Broadcast) -> Result<Self, String> {
        let (source, tracks) = BroadcastSource::new(format!("{name}-convert"), original)
            .map_err(|error| format!("cannot read the source: {error}"))?;
        let (pipeline, (context, inputs)) =
            Pipeline::new(format!("{name}-convert"), source, |source, ctx| {
                let mut inputs = Vec::new();
                for track in &tracks {
                    let (branch, tee) = ctx
                        .tee(format!("track-{}", track.index()))
                        .build_dynamic()?;
                    ctx.attach(source, track.index(), branch)?;
                    inputs.push(tee);
                }
                Ok((Arc::clone(ctx), inputs))
            })
            .map_err(|error| format!("cannot wire the conversions: {error}"))?;
        pipeline
            .run()
            .map_err(|error| format!("cannot start the conversions: {error}"))?;
        Ok(Self {
            pipeline,
            context,
            inputs,
            conversions: BTreeMap::new(),
            written: BTreeMap::new(),
        })
    }

    /// Has track `index` — described by `info` — converted to `target`,
    /// unless it is already.
    fn convert(
        &mut self,
        name: &str,
        index: usize,
        target: Target,
        info: &StreamInfo,
    ) -> Result<(), String> {
        if self.conversions.contains_key(&(index, target)) {
            return Ok(());
        }
        let label = format!("{index}-{}", target_name(target));
        let (out_branch, out) = self
            .context
            .tee(format!("converted-{label}"))
            .build_dynamic()
            .map_err(|error| format!("cannot wire a conversion: {error}"))?;
        let input = &self.inputs[index];
        let chain = input
            .branch()
            .map_err(|error| format!("cannot wire a conversion: {error}"))?
            .queue(format!("convert-{label}"), CONVERT_QUEUE);
        let (built, converted) = match target {
            Target::Opus | Target::Aac => {
                let (decoder, encoder) = sound(name, &label, target, info)?;
                let converted = StreamInfo::from(&encoder);
                (
                    chain.pipe(decoder).pipe(encoder).to_branch(out_branch),
                    converted,
                )
            }
            Target::H264 => {
                let (decoder, encoder, converted) = pictures(name, &label, info)?;
                // Joining a stream under way, the decoder begins at a
                // keyframe: before one there is nothing it can decode.
                let built = chain
                    .pipe(FromKeyframe::default())
                    .pipe(decoder)
                    .pipe(encoder)
                    .to_branch(out_branch);
                (built, converted)
            }
        };
        let branch = built
            .and_then(|branch| input.attach(branch))
            .map_err(|error| format!("cannot wire a conversion: {error}"))?;
        self.conversions.insert(
            (index, target),
            Conversion {
                branch,
                out,
                info: converted,
            },
        );
        Ok(())
    }

    /// Takes the branches into the broadcast of `read`'s tracks away.
    fn unwrite(&mut self, read: &[TrackFrom]) {
        for (tee, branch) in self.written.remove(read).unwrap_or_default() {
            let _ = tee.detach(branch);
        }
    }

    /// Takes away each conversion `plan` has no use for.
    fn drop_unused(&mut self, plan: &Plan) {
        let unused: Vec<(usize, Target)> = self
            .conversions
            .keys()
            .filter(|key| !plan.conversions.contains(key))
            .copied()
            .collect();
        for key in unused {
            let conversion = self.conversions.remove(&key).expect("listed");
            let _ = self.inputs[key.0].detach(conversion.branch);
        }
    }
}

/// A sound track's decoder, and its encoder to `target`: on the processor,
/// at 48 kHz, in one channel or two.
fn sound(
    name: &str,
    label: &str,
    target: Target,
    info: &StreamInfo,
) -> Result<(SwDecoder, SwAudioEncoder), String> {
    let decoder = SwDecoder::new(format!("{name}-decode-{label}"), info.parameters().clone())
        .map_err(|error| format!("cannot decode {label}: {error}"))?;
    let codec = match target {
        Target::Opus => AudioCodec::Opus,
        Target::Aac | Target::H264 => AudioCodec::Aac,
    };
    let channels = channels_of(info).clamp(1, 2);
    let encoder = SwAudioEncoder::new(
        format!("{name}-encode-{label}"),
        SwAudioEncoderOptions {
            codec,
            sample_rate: 48_000,
            channels,
            bit_rate: 64_000 * usize::from(channels),
        },
    )
    .map_err(|error| format!("cannot encode {}: {error}", target_name(target)))?;
    Ok((decoder, encoder))
}

/// A picture track's decoder and its H.264 encoder, on the first GPU that
/// opens both — the decoder's pictures handed to the encoder on the GPU,
/// never copied out — and in software where none does; and what the
/// encoder's track is.
fn pictures(
    name: &str,
    label: &str,
    info: &StreamInfo,
) -> Result<(VideoDecodeBin, VideoEncodeBin, StreamInfo), String> {
    let (width, height) = info
        .size()
        .ok_or_else(|| format!("{label}: its pictures' size is not known"))?;
    let rate = info
        .frame_rate()
        .filter(|rate| rate.numerator() > 0 && rate.denominator() > 0)
        .unwrap_or(Rational::new(25, 1));
    let per_second = f64::from(rate);
    let options = VideoEncodeOptions {
        width,
        height,
        frame_rate: rate,
        bit_rate: bit_rate(width, height, per_second),
        // A keyframe a second: a player joining waits no longer.
        gop_size: per_second.round().max(1.0) as u32,
        max_b_frames: Some(0),
        color: info.color(),
    };
    let mut refused = Vec::new();
    for target in decode_targets() {
        let decoder = match VideoDecodeBin::open(
            format!("{name}-decode-{label}"),
            info.parameters().clone(),
            target.clone(),
            None,
        ) {
            Ok(decoder) => decoder,
            Err(error) => {
                refused.push(error.to_string());
                continue;
            }
        };
        let Some(input) = EncodeInput::for_decoded(&target, decoder.output_format()) else {
            refused.push("no encoder takes the decoder's pictures".to_owned());
            continue;
        };
        match VideoEncodeBin::open(format!("{name}-encode-{label}"), input, options) {
            Ok(encoder) => {
                tracing::info!(
                    track = %label,
                    decoding = ?decoder.path(),
                    encoding = ?encoder.path(),
                    "pictures converted"
                );
                let converted = StreamInfo::new(encoder.parameters(), encoder.time_base())
                    .with_frame_rate(rate);
                return Ok((decoder, encoder, converted));
            }
            Err(error) => refused.push(error.to_string()),
        }
    }
    Err(format!(
        "cannot convert {label}'s pictures: {}",
        refused.join("; ")
    ))
}

/// Where pictures may be decoded to for conversion, the GPU first: each
/// conversion a device of its own, so one's work does not wait on
/// another's lock of a shared one.
fn decode_targets() -> Vec<DecodeTarget> {
    let mut targets = Vec::new();
    #[cfg(windows)]
    if let Ok(gpu) = media_pp::elements::D3d11Gpu::new() {
        targets.push(DecodeTarget::D3d11 {
            gpu,
            downstream_hw_frames: DOWNSTREAM_FRAMES,
        });
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(device) = media_pp::elements::CudaDevice::new() {
            targets.push(DecodeTarget::Cuda {
                device,
                downstream_hw_frames: DOWNSTREAM_FRAMES,
            });
        }
        if let Ok(device) = media_pp::elements::VulkanDevice::new() {
            targets.push(DecodeTarget::Vulkan {
                device,
                downstream_hw_frames: DOWNSTREAM_FRAMES,
            });
        }
    }
    targets.push(DecodeTarget::System);
    targets
}

/// What a converted stream of `width`×`height` at `per_second` pictures
/// is encoded at: about a tenth of a bit a pixel, 3.6 Mbit/s for 1080p at
/// 25, within half a megabit and twenty.
fn bit_rate(width: u32, height: u32, per_second: f64) -> usize {
    let bits = f64::from(width) * f64::from(height) * per_second * 0.07;
    (bits as usize).clamp(500_000, 20_000_000)
}

/// Hands on a track's packets from its first keyframe, and from the first
/// after a `Flush`.
#[derive(Default)]
struct FromKeyframe {
    begun: bool,
}

impl Filter for FromKeyframe {
    fn transform(&mut self, buf: MediaBuffer, out: &mut Pushed) -> media_pp::Result<()> {
        if !self.begun {
            let key = match &buf {
                MediaBuffer::Packet(packet) => packet.is_key(),
                _ => true,
            };
            if !key {
                return Ok(());
            }
            self.begun = true;
        }
        out.push(buf);
        Ok(())
    }

    fn reset(&mut self) {
        self.begun = false;
    }
}

/// How many channels a sound track has, as its description says.
fn channels_of(info: &StreamInfo) -> u16 {
    // SAFETY: a plain field of parameters the description owns.
    let channels = unsafe { (*info.parameters().as_ptr()).ch_layout.nb_channels };
    u16::try_from(channels).unwrap_or(1)
}

fn target_name(target: Target) -> &'static str {
    match target {
        Target::H264 => "h264",
        Target::Opus => "opus",
        Target::Aac => "aac",
    }
}

/// A made broadcast's name, for logs: the channel's, and what is converted.
fn made_name(channel: &str, read: &[TrackFrom]) -> String {
    let converted: Vec<String> = read
        .iter()
        .filter_map(|track| match track {
            TrackFrom::Converted(index, target) => Some(format!("{index}{}", target_name(*target))),
            TrackFrom::Original(_) => None,
        })
        .collect();
    format!("{channel}~{}", converted.join("-"))
}

fn names(outputs: &BTreeSet<Output>) -> String {
    outputs
        .iter()
        .map(|output| output.name())
        .collect::<Vec<_>>()
        .join(", ")
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
