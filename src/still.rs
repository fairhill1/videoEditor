//! Still images: a picture with no length of its own, stretched across however
//! long its clip is made.
//!
//! Decoded once, through ffmpeg like everything else, rather than kept as a
//! one-frame video. A one-frame video looks like footage that ends almost at
//! once: its clip would drop onto the timeline a frame long, and every decoder
//! that keeps going back to the file for "the frame at t" would keep decoding
//! the same picture.

use ffmpeg_next as ffmpeg;

use crate::quad::{QuadRenderer, Texture};
use crate::title;

/// What the file picker offers as images, and what an import treats as one.
/// No GIF: an animated one is footage, and a still one is rare enough not to be
/// worth getting the other case wrong for.
pub(crate) const EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "bmp", "tif", "tiff"];

/// How long a still's clip starts out when dropped: the same as a title's,
/// which is also a picture with no length of its own.
pub(crate) const DEFAULT_DURATION: f64 = title::DEFAULT_DURATION;
/// How far a still's clip can be stretched. A bound, not a length, as for a
/// title.
pub(crate) const MAX_DURATION: f64 = title::MAX_DURATION;

/// Whether `path` names an image, going by its extension.
pub(crate) fn is_still(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// A decoded image as straight (not premultiplied) RGBA, rows packed.
pub(crate) struct Rgba {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: Vec<u8>,
}

impl Rgba {
    /// Whether any pixel lets what is underneath show through. An opaque image
    /// can be composed as a plain picture, which is cheaper than mixing every
    /// pixel by its own alpha.
    pub(crate) fn has_transparency(&self) -> bool {
        self.pixels.chunks_exact(4).any(|p| p[3] < 255)
    }
}

/// Decode the picture in `path`.
///
/// To RGBA whatever the file holds: a PNG converts straight across, and a JPEG
/// is full-range YUV with the BT.601 matrix, which is the converter's default
/// for it.
pub(crate) fn decode(path: &str) -> Result<Rgba, ffmpeg::Error> {
    ffmpeg::init()?;
    let mut ictx = ffmpeg::format::input(&path)?;
    let (stream_index, parameters) = {
        let input = ictx
            .streams()
            .best(ffmpeg::media::Type::Video)
            .ok_or(ffmpeg::Error::StreamNotFound)?;
        (input.index(), input.parameters())
    };
    let mut decoder = ffmpeg::codec::context::Context::from_parameters(parameters)?
        .decoder()
        .video()?;
    let mut frame = ffmpeg::frame::Video::empty();
    let mut got = false;
    for (stream, packet) in ictx.packets() {
        if stream.index() != stream_index {
            continue;
        }
        decoder.send_packet(&packet)?;
        if decoder.receive_frame(&mut frame).is_ok() {
            got = true;
            break;
        }
    }
    if !got {
        decoder.send_eof()?;
        decoder.receive_frame(&mut frame)?;
    }

    let (width, height) = (frame.width(), frame.height());
    let mut scaler = ffmpeg::software::scaling::Context::get(
        frame.format(),
        width,
        height,
        ffmpeg::format::Pixel::RGBA,
        width,
        height,
        ffmpeg::software::scaling::Flags::BILINEAR,
    )?;
    let mut rgba = ffmpeg::frame::Video::empty();
    scaler.run(&frame, &mut rgba)?;
    let stride = rgba.stride(0);
    let row_bytes = width as usize * 4;
    let mut pixels = vec![0u8; row_bytes * height as usize];
    for y in 0..height as usize {
        pixels[y * row_bytes..(y + 1) * row_bytes]
            .copy_from_slice(&rgba.data(0)[y * stride..y * stride + row_bytes]);
    }
    Ok(Rgba { width, height, pixels })
}

/// A still as the preview holds it: uploaded once, drawn every frame.
pub struct Still {
    texture: Texture,
    width: u32,
    height: u32,
}

impl Still {
    pub fn open(
        path: &str,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        quads: &QuadRenderer,
    ) -> Result<Self, ffmpeg::Error> {
        let image = decode(path)?;
        let texture = quads.create_empty_texture(
            device,
            image.width,
            image.height,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        );
        texture.write_region(queue, 0, 0, image.width, image.height, &image.pixels);
        Ok(Self { texture, width: image.width, height: image.height })
    }

    pub fn texture(&self) -> &Texture {
        &self.texture
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_are_told_apart_by_extension_in_any_case() {
        assert!(is_still("/a/logo.png"));
        assert!(is_still("/a/SHOT.JPG"));
        assert!(is_still("cover.Jpeg"));
        assert!(!is_still("/a/clip.mp4"));
        assert!(!is_still("/a/anim.gif"));
        assert!(!is_still("/a/png"));
    }

    /// Writes a small PNG with a transparent corner through ffmpeg's own CLI
    /// and reads it back. Skips if there is no `ffmpeg` on the PATH.
    #[test]
    fn a_png_decodes_with_its_size_and_its_alpha() {
        let dir = std::env::temp_dir().join(format!("ruve-still-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("probe.png");
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("color=c=red@0.5:s=33x21,format=rgba")
            .args(["-frames:v", "1"])
            .arg(&path)
            .status();
        if !made.is_ok_and(|s| s.success()) {
            return;
        }
        let image = decode(path.to_str().unwrap()).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!((image.width, image.height), (33, 21));
        assert_eq!(image.pixels.len(), 33 * 21 * 4);
        let px = &image.pixels[..4];
        assert!(px[0] > 240 && px[1] < 15 && px[2] < 15, "{px:?}");
        assert!((120..=135).contains(&px[3]), "alpha {}", px[3]);
        assert!(image.has_transparency());
    }
}
