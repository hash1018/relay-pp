//! Pulling a camera with FFmpeg's RTSP client, media-pp's `RtspSource`:
//! what takes every camera — `rtsps://`, RTP over UDP, any codec FFmpeg
//! depacketizes — at several times the processor time of `retina`'s, on a
//! thread of each connection's own.

use std::sync::Arc;

use media_pp::bus::BusEvent;
use media_pp::elements::{RtspOptions, RtspSource, StreamInfo};
use media_pp::ffmpeg::media;
use media_pp::pipeline::Pipeline;
use tokio::sync::watch;

use super::{CAMERA_TIMEOUT, Ended, Inner, Puller, halted};

/// Pulls the camera until it fails or the source is halted.
///
/// Each connection is a pipeline of its own ending in the broadcast,
/// published again: the broadcast is what outlives it, and publishing
/// again is what tells its readers that one run ended and another began.
pub(super) async fn pull(inner: &Arc<Inner>, halt: &mut watch::Receiver<bool>) -> Ended {
    let opening = tokio::task::spawn_blocking({
        let inner = Arc::clone(inner);
        move || open(&inner)
    });
    let opened = tokio::select! {
        opened = opening => opened,
        // The open goes on to its end on the blocking pool, and what it
        // opens is dropped there.
        () = halted(halt) => return Ended::Halted,
    };
    let (source, streams) = match opened {
        Ok(Ok(opened)) => opened,
        Ok(Err(reason)) => return Ended::Failed(reason),
        Err(_) => return Ended::Failed("opening the camera panicked".into()),
    };
    let pipeline = match start(inner, source, &streams) {
        Ok(pipeline) => pipeline,
        Err(reason) => return Ended::Failed(reason),
    };
    inner.went_live(Puller::Ffmpeg);

    let ended = tokio::select! {
        reason = watch_bus(&pipeline) => Ended::Failed(reason),
        () = halted(halt) => Ended::Halted,
        // A sink cannot go on with this run: a new connection describes
        // the camera anew.
        () = inner.publisher.renewal() => Ended::Reconnect,
    };
    // Stopping joins the pipeline's thread, which a camera gone silent
    // holds up to its timeout: not on the runtime's own threads.
    let _ = tokio::task::spawn_blocking(move || pipeline.stop()).await;
    ended
}

/// Opens the camera: a media-pp source, and the streams of it a broadcast
/// carries — pictures and sound, not a camera's data or metadata streams.
/// Blocks, up to the camera's timeout.
fn open(inner: &Inner) -> Result<(RtspSource, Vec<StreamInfo>), String> {
    let (source, streams) = RtspSource::open(
        format!("{}-camera", inner.name),
        &inner.url,
        RtspOptions {
            transport: inner.transport,
            timeout: CAMERA_TIMEOUT,
        },
    )
    .map_err(|error| format!("cannot open the camera: {error}"))?;
    let kept: Vec<StreamInfo> = streams
        .into_iter()
        .filter(|stream| matches!(stream.kind(), media::Type::Video | media::Type::Audio))
        .collect();
    if kept.is_empty() {
        return Err("the camera sends neither pictures nor sound".into());
    }
    Ok((source, kept))
}

/// Publishes `streams` again and starts a pipeline writing `source` into
/// them.
fn start(
    inner: &Inner,
    source: RtspSource,
    streams: &[StreamInfo],
) -> Result<Arc<Pipeline>, String> {
    let sinks = inner.publisher.publish(streams)?;
    let (pipeline, ()) = Pipeline::new(format!("{}-pull", inner.name), source, |source, ctx| {
        for (stream, sink) in streams.iter().zip(sinks) {
            ctx.attach(source, stream.index(), ctx.branch().to(sink)?)?;
        }
        Ok(())
    })
    .map_err(|error| format!("cannot wire the camera: {error}"))?;
    pipeline
        .run()
        .map_err(|error| format!("cannot start the camera: {error}"))?;
    Ok(pipeline)
}

/// Why a connection's pipeline ended, as its bus says — which it does only
/// for a failure or the camera ending its stream, since nothing else stops
/// it while this waits.
async fn watch_bus(pipeline: &Pipeline) -> String {
    loop {
        match pipeline.bus().recv_async().await {
            Some(BusEvent::Error { error, .. }) => return format!("the camera failed: {error}"),
            Some(BusEvent::Finished) => return "the camera ended its stream".to_owned(),
            Some(_) => {}
            None => return "the camera's pipeline ended".to_owned(),
        }
    }
}
