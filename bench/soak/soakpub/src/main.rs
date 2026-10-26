//! A publisher for the soak: plays a file in a loop, at its own speed, into
//! an RTSP server (`rtsp://`), a WHIP server (`http://`) or an RTMP server
//! (`rtmp://`, its pictures alone: FLV carries no Opus), and connects
//! again two seconds after the connection fails, as an encoder does, until
//! `seconds` have gone by.
//!
//!     soakpub <file> <url> <seconds>

use std::time::{Duration, Instant};

use media_pp::bus::BusEvent;
use media_pp::elements::{
    FileDemuxer, Pacer, RtmpMuxer, RtspMuxer, RtspOptions, WebRtcMuxer, WebRtcOptions,
};
use media_pp::pipeline::Pipeline;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: soakpub <file> <url> <seconds>");
        std::process::exit(1);
    }
    let _log = media_pp::log::init("soakpub", "logs", media_pp::log::Level::Info, 2);
    let (file, url) = (&args[1], &args[2]);
    let until = Instant::now() + Duration::from_secs(args[3].parse().unwrap_or(60));
    let mut connections = 0u32;
    while Instant::now() < until {
        connections += 1;
        match publish(file, url) {
            Ok(pipeline) => {
                println!("{} connection {connections}: publishing", stamp());
                let ended = watch(&pipeline, until);
                pipeline.stop();
                println!("{} connection {connections}: {ended}", stamp());
            }
            Err(error) => println!("{} connection {connections}: cannot publish: {error}", stamp()),
        }
        if Instant::now() < until {
            std::thread::sleep(Duration::from_secs(2));
        }
    }
    println!("{} done after {connections} connections", stamp());
}

fn stamp() -> String {
    format!("{:?}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs())
}

/// What ended the connection, or that time ran out.
fn watch(pipeline: &Pipeline, until: Instant) -> String {
    let bus = pipeline.bus();
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return "time is up".to_owned();
        }
        match bus.recv_timeout(left.min(Duration::from_secs(1))) {
            Ok(BusEvent::Error { name, error, .. }) => return format!("failed at {name}: {error}"),
            Ok(BusEvent::Finished) => return "finished".to_owned(),
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => return "the bus ended".to_owned(),
        }
    }
}

fn publish(file: &str, url: &str) -> media_pp::Result<std::sync::Arc<Pipeline>> {
    let (source, streams) = FileDemuxer::open("file", file)?;
    source.looping_handle().set_looping(true);
    let whip = url.starts_with("http");
    let (pipeline, ()) = Pipeline::new("soakpub", source, |source, ctx| {
        let mut sinks = Vec::new();
        if whip {
            let mut muxer = WebRtcMuxer::create("whip", url, WebRtcOptions::default())?;
            let tracks: Vec<_> = streams
                .iter()
                .map(|stream| muxer.add_stream(format!("t{}", stream.index()), stream))
                .collect::<media_pp::Result<_>>()?;
            let mut opened = muxer.open()?;
            for (stream, track) in streams.iter().zip(tracks) {
                sinks.push((stream.index(), opened.take(track)?));
            }
        } else if url.starts_with("rtmp") {
            // FLV carries H.264 and AAC: the file's pictures alone.
            let mut muxer = RtmpMuxer::create(url)?;
            let video: Vec<_> = streams
                .iter()
                .filter(|stream| stream.kind() == media_pp::ffmpeg::media::Type::Video)
                .collect();
            let tracks: Vec<_> = video
                .iter()
                .map(|stream| muxer.add_stream(format!("t{}", stream.index()), *stream))
                .collect::<media_pp::Result<_>>()?;
            let mut opened = muxer.open()?;
            for (stream, track) in video.iter().zip(tracks) {
                sinks.push((stream.index(), opened.take(track)?));
            }
        } else {
            let mut muxer = RtspMuxer::create(url, RtspOptions::default())?;
            let tracks: Vec<_> = streams
                .iter()
                .map(|stream| muxer.add_stream(format!("t{}", stream.index()), stream))
                .collect::<media_pp::Result<_>>()?;
            let mut opened = muxer.open()?;
            for (stream, track) in streams.iter().zip(tracks) {
                sinks.push((stream.index(), opened.take(track)?));
            }
        }
        for (index, sink) in sinks {
            let branch = ctx
                .branch()
                .queue(format!("q{index}"), 64)
                .pipe(Pacer::new(format!("pace{index}")))
                .to(sink)?;
            ctx.attach(source, index, branch)?;
        }
        Ok(())
    })?;
    pipeline.run()?;
    Ok(pipeline)
}
