//! What converts a track: its decoder and encoder, chosen for the codec it
//! is converted to and the GPU there is — and the filter a conversion
//! joining a stream under way begins with.

use media_pp::buffer::MediaBuffer;
use media_pp::element::{Filter, Output};
use media_pp::elements::{
    AudioCodec, DecodeTarget, EncodeInput, StreamInfo, SwAudioEncoder, SwAudioEncoderOptions,
    SwDecoder, VideoDecodeBin, VideoEncodeBin, VideoEncodeOptions,
};
use media_pp::ffmpeg::Rational;

use super::plan::Target;
use crate::config::VideoConversion;

/// What a converted track's pictures may be held by after its decoder: the
/// encoder's own, as many as a GPU encoder keeps while it works.
const DOWNSTREAM_FRAMES: i32 = 4;

/// A sound track's decoder, and its encoder to `target`: on the processor,
/// at 48 kHz, in one channel or two.
pub(super) fn sound(
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
    .map_err(|error| format!("cannot encode {}: {error}", target.name()))?;
    Ok((decoder, encoder))
}

/// A picture track's decoder and its H.264 encoder, on the first GPU that
/// opens both — the decoder's pictures handed to the encoder on the GPU,
/// never copied out — and in software where none does and `video` allows
/// it; and what the encoder's track is.
pub(super) fn pictures(
    name: &str,
    label: &str,
    info: &StreamInfo,
    video: VideoConversion,
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
    let targets = decode_targets(video);
    if targets.is_empty() {
        return Err(format!(
            "cannot convert {label}'s pictures: no GPU device opened, and they are converted on the GPU alone"
        ));
    }
    let mut refused = Vec::new();
    for target in targets {
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
            // Every session of the GPU's encoder taken, or none there: the
            // bin has gone on to software, which this hub refuses.
            Ok(encoder) if video == VideoConversion::Gpu && !encoder.path().is_hardware() => {
                refused.push(format!("no GPU encoder opened, only {:?}", encoder.path()));
            }
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
fn decode_targets(video: VideoConversion) -> Vec<DecodeTarget> {
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
    if video == VideoConversion::Any {
        targets.push(DecodeTarget::System);
    }
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
/// after a `Flush`: what a decoder joining a stream under way can begin at.
#[derive(Default)]
pub(super) struct FromKeyframe {
    begun: bool,
}

impl Filter for FromKeyframe {
    fn transform(&mut self, buf: MediaBuffer, out: &mut Output) -> media_pp::Result<()> {
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
