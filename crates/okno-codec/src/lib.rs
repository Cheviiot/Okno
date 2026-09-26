//! Video encoding and decoding.
//!
//! The MVP uses software H.264 (Cisco OpenH264, built from source) in the
//! screen-content real-time mode: constrained baseline, no B-frames, so every
//! packet decodes as soon as it arrives. Hardware encoders (VA-API, Media
//! Foundation) can replace [`VideoEncoder`] later without protocol changes.

use openh264::OpenH264API;
use openh264::decoder::Decoder;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode, UsageType,
};
use openh264::formats::{BgraSliceU8, RgbaSliceU8, YUVBuffer, YUVSource};

/// Byte order of a 32-bit pixel. The fourth byte (alpha or padding) is
/// ignored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra,
    Rgba,
}

/// An uncompressed captured image.
#[derive(Clone, Debug)]
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    /// Bytes per row; at least `width * 4`.
    pub stride: usize,
    pub format: PixelFormat,
    pub data: Vec<u8>,
}

impl RawFrame {
    /// A tightly packed frame.
    pub fn packed(width: u32, height: u32, format: PixelFormat, data: Vec<u8>) -> Self {
        Self { width, height, stride: width as usize * 4, format, data }
    }

    fn is_valid(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.stride >= self.width as usize * 4
            && self.data.len() >= self.stride * (self.height as usize - 1) + self.width as usize * 4
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("H.264: {0}")]
    H264(#[from] openh264::Error),
    #[error("frame buffer does not match its size")]
    BadFrame,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncoderSettings {
    pub max_fps: u32,
    pub bitrate_kbps: u32,
}

impl Default for EncoderSettings {
    fn default() -> Self {
        Self { max_fps: 30, bitrate_kbps: 8000 }
    }
}

pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub width: u32,
    pub height: u32,
}

pub struct VideoEncoder {
    settings: EncoderSettings,
    encoder: Encoder,
    yuv: Option<YUVBuffer>,
    packed: Vec<u8>,
}

impl VideoEncoder {
    pub fn new(settings: EncoderSettings) -> Result<Self, CodecError> {
        Ok(Self { encoder: make_encoder(settings)?, settings, yuv: None, packed: Vec::new() })
    }

    pub fn settings(&self) -> EncoderSettings {
        self.settings
    }

    /// Applies new rate settings; the next frame is a keyframe.
    pub fn reconfigure(&mut self, settings: EncoderSettings) -> Result<(), CodecError> {
        if settings != self.settings {
            self.encoder = make_encoder(settings)?;
            self.settings = settings;
        }
        Ok(())
    }

    pub fn request_keyframe(&mut self) {
        self.encoder.force_intra_frame();
    }

    /// Encodes one frame. Returns `None` when the rate control skipped it.
    ///
    /// H.264 with 4:2:0 chroma needs even dimensions, so an odd last column
    /// or row is dropped.
    pub fn encode(&mut self, frame: &RawFrame, pts_us: u64) -> Result<Option<EncodedFrame>, CodecError> {
        if !frame.is_valid() {
            return Err(CodecError::BadFrame);
        }
        let width = frame.width as usize & !1;
        let height = frame.height as usize & !1;
        if width == 0 || height == 0 {
            return Err(CodecError::BadFrame);
        }

        // Pack rows contiguously at the even size.
        let row = width * 4;
        let pixels: &[u8] = if frame.stride == row && frame.height as usize == height {
            &frame.data[..row * height]
        } else {
            self.packed.clear();
            for y in 0..height {
                let start = y * frame.stride;
                self.packed.extend_from_slice(&frame.data[start..start + row]);
            }
            &self.packed
        };

        let yuv = match &mut self.yuv {
            Some(buf) if buf.dimensions() == (width, height) => buf,
            slot => slot.insert(YUVBuffer::new(width, height)),
        };
        match frame.format {
            PixelFormat::Bgra => yuv.read_rgb8(BgraSliceU8::new(pixels, (width, height))),
            PixelFormat::Rgba => yuv.read_rgb8(RgbaSliceU8::new(pixels, (width, height))),
        }

        let ts = openh264::Timestamp::from_millis(pts_us / 1000);
        let stream = self.encoder.encode_at(&*yuv, ts)?;
        let keyframe = matches!(stream.frame_type(), FrameType::IDR | FrameType::I);
        if matches!(stream.frame_type(), FrameType::Skip | FrameType::Invalid) {
            return Ok(None);
        }
        let data = stream.to_vec();
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(EncodedFrame { data, keyframe, width: width as u32, height: height as u32 }))
    }
}

fn make_encoder(settings: EncoderSettings) -> Result<Encoder, CodecError> {
    let fps = settings.max_fps.clamp(1, 120);
    let config = EncoderConfig::new()
        .usage_type(UsageType::ScreenContentRealTime)
        .rate_control_mode(RateControlMode::Bitrate)
        .bitrate(BitRate::from_bps(settings.bitrate_kbps.clamp(100, 100_000) * 1000))
        .max_frame_rate(FrameRate::from_hz(fps as f32))
        .skip_frames(true)
        // A periodic keyframe heals any decoder desync within 10 s.
        .intra_frame_period(IntraFramePeriod::from_num_frames(fps * 10))
        .num_threads(4);
    Ok(Encoder::with_api_config(OpenH264API::from_source(), config)?)
}

/// A decoded frame, borrowed from the decoder until the next call.
pub struct DecodedFrame<'a> {
    pub width: u32,
    pub height: u32,
    /// Packed RGBA, `width * height * 4` bytes.
    pub rgba: &'a [u8],
}

pub struct VideoDecoder {
    decoder: Decoder,
    rgba: Vec<u8>,
}

impl VideoDecoder {
    pub fn new() -> Result<Self, CodecError> {
        Ok(Self { decoder: Decoder::new()?, rgba: Vec::new() })
    }

    /// Decodes one packet. `None` means the decoder needs more data (for
    /// example a keyframe after joining mid-stream).
    pub fn decode(&mut self, packet: &[u8]) -> Result<Option<DecodedFrame<'_>>, CodecError> {
        let Some(yuv) = self.decoder.decode(packet)? else {
            return Ok(None);
        };
        let (width, height) = yuv.dimensions();
        self.rgba.resize(width * height * 4, 0);
        yuv.write_rgba8(&mut self.rgba);
        Ok(Some(DecodedFrame { width: width as u32, height: height as u32, rgba: &self.rgba }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Solid-colour BGRA frame with a stride wider than the row.
    fn frame(width: u32, height: u32, bgra: [u8; 4]) -> RawFrame {
        let stride = width as usize * 4 + 64;
        let mut data = vec![0u8; stride * height as usize];
        for y in 0..height as usize {
            for x in 0..width as usize {
                data[y * stride + x * 4..][..4].copy_from_slice(&bgra);
            }
        }
        RawFrame { width, height, stride, format: PixelFormat::Bgra, data }
    }

    #[test]
    fn round_trip_keeps_size_and_colour() {
        let mut enc = VideoEncoder::new(EncoderSettings::default()).unwrap();
        let mut dec = VideoDecoder::new().unwrap();
        // Odd size: expect the last column and row to be dropped.
        let src = frame(321, 241, [200, 100, 50, 255]);
        let mut decoded = None;
        for i in 0..5 {
            if let Some(packet) = enc.encode(&src, i * 33_000).unwrap() {
                assert_eq!((packet.width, packet.height), (320, 240));
                if i == 0 {
                    assert!(packet.keyframe);
                }
                if let Some(out) = dec.decode(&packet.data).unwrap() {
                    decoded = Some((out.width, out.height, out.rgba[..4].to_vec()));
                }
            }
        }
        let (w, h, px) = decoded.expect("at least one decoded frame");
        assert_eq!((w, h), (320, 240));
        // BGRA (200,100,50) is RGB (50,100,200); allow YUV rounding.
        for (got, want) in px[..3].iter().zip([50u8, 100, 200]) {
            assert!(got.abs_diff(want) <= 6, "{px:?}");
        }
    }

    #[test]
    fn keyframe_on_request() {
        let mut enc = VideoEncoder::new(EncoderSettings::default()).unwrap();
        let src = frame(64, 64, [0, 0, 0, 255]);
        for i in 0..3 {
            enc.encode(&src, i * 33_000).unwrap();
        }
        enc.request_keyframe();
        let packet = enc.encode(&src, 200_000).unwrap().expect("forced frame is not skipped");
        assert!(packet.keyframe);
    }

    #[test]
    fn rejects_short_buffer() {
        let mut enc = VideoEncoder::new(EncoderSettings::default()).unwrap();
        let bad = RawFrame::packed(64, 64, PixelFormat::Rgba, vec![0; 100]);
        assert!(matches!(enc.encode(&bad, 0), Err(CodecError::BadFrame)));
    }
}
