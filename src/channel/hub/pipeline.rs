//! The pipeline that writes a hub's made broadcasts, for one run of its
//! source: the source's broadcast read once, each track into a `Tee` of its
//! own, and branches added and taken away while it runs — a converted
//! track's decoder and encoder ending in a `Tee` of their own, and from
//! either `Tee` into each made broadcast that has the track.

use std::collections::BTreeMap;
use std::sync::Arc;

use media_pp::element::Context;
use media_pp::elements::{Broadcast, BroadcastSource, StreamInfo, TeeHandle};
use media_pp::graph::BranchId;
use media_pp::pipeline::Pipeline;

use super::Refusal;
use super::media::{self, FromKeyframe};
use super::plan::{Plan, Target, TrackFrom};
use crate::config::VideoConversion;

/// What a converted track's branch may hold before its decoder, as packets.
const CONVERT_QUEUE: usize = 64;

/// The pipeline, and what it has wired.
pub(super) struct Running {
    pub(super) pipeline: Arc<Pipeline>,
    /// What builds a branch's own `Tee` while it runs.
    context: Arc<Context>,
    /// Each source track's `Tee`.
    inputs: Vec<TeeHandle>,
    pub(super) conversions: BTreeMap<(usize, Target), Conversion>,
    /// Each broadcast written, by its tracks: the branches into it.
    pub(super) written: BTreeMap<Vec<TrackFrom>, Vec<(TeeHandle, BranchId)>>,
}

/// A track converted: its branch off the track's `Tee`, and the `Tee` its
/// packets come out of.
pub(super) struct Conversion {
    branch: BranchId,
    out: TeeHandle,
    pub(super) info: StreamInfo,
}

impl Running {
    /// A pipeline reading `original`, each of its tracks into a `Tee` of
    /// its own with nothing after it yet.
    pub(super) fn start(name: &str, original: &Broadcast) -> Result<Self, Refusal> {
        let (source, tracks) = BroadcastSource::new(format!("{name}-convert"), original)
            .map_err(|error| Refusal::Failed(format!("cannot read the source: {error}")))?;
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
            .map_err(|error| Refusal::Failed(format!("cannot wire the conversions: {error}")))?;
        pipeline
            .run()
            .map_err(|error| Refusal::Failed(format!("cannot start the conversions: {error}")))?;
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
    pub(super) fn convert(
        &mut self,
        name: &str,
        index: usize,
        target: Target,
        info: &StreamInfo,
        video: VideoConversion,
    ) -> Result<(), Refusal> {
        if self.conversions.contains_key(&(index, target)) {
            return Ok(());
        }
        let label = format!("{index}-{}", target.name());
        let (out_branch, out) = self
            .context
            .tee(format!("converted-{label}"))
            .build_dynamic()
            .map_err(|error| Refusal::Failed(format!("cannot wire a conversion: {error}")))?;
        let input = &self.inputs[index];
        let chain = input
            .branch()
            .map_err(|error| Refusal::Failed(format!("cannot wire a conversion: {error}")))?
            .queue(format!("convert-{label}"), CONVERT_QUEUE);
        let (built, converted) = match target {
            Target::Opus | Target::Aac => {
                let (decoder, encoder) = media::sound(name, &label, target, info)?;
                let converted = StreamInfo::from(&encoder);
                (
                    chain.pipe(decoder).pipe(encoder).to_branch(out_branch),
                    converted,
                )
            }
            Target::H264 => {
                let (decoder, encoder, converted) = media::pictures(name, &label, info, video)?;
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
            .map_err(|error| Refusal::Failed(format!("cannot wire a conversion: {error}")))?;
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

    /// Wires each track of `read` into `sinks`, one each in order: a branch
    /// off the source track's `Tee`, or its conversion's.
    pub(super) fn write(
        &mut self,
        read: &[TrackFrom],
        sinks: Vec<media_pp::elements::BroadcastSink>,
    ) -> Result<(), Refusal> {
        let mut branches = Vec::new();
        for (track, sink) in read.iter().zip(sinks) {
            let tee = match *track {
                TrackFrom::Original(index) => &self.inputs[index],
                TrackFrom::Converted(index, target) => &self.conversions[&(index, target)].out,
            };
            let branch = tee
                .branch()
                .and_then(|chain| chain.to(sink))
                .and_then(|branch| tee.attach(branch))
                .map_err(|error| Refusal::Failed(format!("cannot wire the broadcast: {error}")))?;
            branches.push((tee.clone(), branch));
        }
        self.written.insert(read.to_vec(), branches);
        Ok(())
    }

    /// Takes the branches into the broadcast of `read`'s tracks away.
    pub(super) fn unwrite(&mut self, read: &[TrackFrom]) {
        for (tee, branch) in self.written.remove(read).unwrap_or_default() {
            let _ = tee.detach(branch);
        }
    }

    /// Takes away each conversion `plan` has no use for.
    pub(super) fn drop_unused(&mut self, plan: &Plan) {
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
