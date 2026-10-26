//! A file's picture re-encoded to HEVC on NVENC, its sound copied: what
//! the Linux soak's HEVC camera serves, so that a WebRTC player of it is
//! converted on the GPU.
//!
//!     hevcify <in.mp4> <out.mp4>

use media_pp::bus::BusEvent;
use media_pp::elements::{
    CudaCodec, CudaDecoder, CudaDevice, CudaEncoder, CudaEncoderOptions, CudaFrameFormat,
    FileDemuxer, FileMuxer, Mp4Layout,
};
use media_pp::ffmpeg;
use media_pp::pipeline::Pipeline;

fn main() -> media_pp::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let (demuxer, streams) = FileDemuxer::open("file", &args[1])?;
    let video = streams
        .iter()
        .find(|s| s.kind() == ffmpeg::media::Type::Video)
        .expect("a picture")
        .clone();
    let audio = streams
        .iter()
        .find(|s| s.kind() == ffmpeg::media::Type::Audio)
        .cloned();
    let (width, height) = video.size().expect("a picture size");
    let cuda = CudaDevice::new()?;
    let decoder = CudaDecoder::new("decoder", video.parameters().clone(), &cuda, 8)?;
    let encoder = CudaEncoder::new(
        "encoder",
        &cuda,
        CudaEncoderOptions {
            codec: CudaCodec::H265,
            input_format: CudaFrameFormat::Nv12,
            width,
            height,
            frame_rate: video.frame_rate().unwrap_or(ffmpeg::Rational::new(30, 1)),
            bit_rate: 3_000_000,
            gop_size: 60,
            max_b_frames: Some(0),
        },
    )?;
    let mut muxer = FileMuxer::create(&args[2], Mp4Layout::FastStart)?;
    let video_track = muxer.add_stream("video", &encoder)?;
    let audio_track = audio
        .as_ref()
        .map(|a| muxer.add_stream("audio", a))
        .transpose()?;
    let mut opened = muxer.open()?;
    let video_sink = opened.take(video_track)?;
    let audio_sink = audio_track.map(|t| opened.take(t)).transpose()?;
    let (pipeline, ()) = Pipeline::new("hevcify", demuxer, |source, ctx| {
        let branch = ctx
            .branch()
            .pipe(decoder)
            .queue("frames", 8)
            .pipe(encoder)
            .to(video_sink)?;
        ctx.attach(source, video.index(), branch)?;
        if let (Some(audio), Some(sink)) = (&audio, audio_sink) {
            let branch = ctx.branch().queue("sound", 64).to(sink)?;
            ctx.attach(source, audio.index(), branch)?;
        }
        Ok(())
    })?;
    pipeline.run()?;
    for event in pipeline.bus().iter() {
        match event {
            BusEvent::Finished => break,
            BusEvent::Error { error, .. } => return Err(error),
            _ => {}
        }
    }
    pipeline.stop();
    println!("wrote {}", args[2]);
    Ok(())
}
