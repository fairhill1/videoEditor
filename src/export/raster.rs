//! Where a layer lands in the output frame, and the plane arithmetic that
//! puts it there.
//!
//! Everything below works on raw `frame::Video` planes rather than on anything
//! that knows about the timeline. That is the point: the placement is decided
//! once, from geometry, and the copying is the same handful of loops whether
//! the picture came out of a decoder or off a title rasterizer.

use ffmpeg_next as ffmpeg;
use ffmpeg::software::scaling;
use ffmpeg::{format, frame};

use crate::compose::Layer;

/// Limited-range black in YUV420P, matching the range x264 signals by default,
/// so letterbox bars sit at the same level as genuinely black picture.
const BLACK_Y: u8 = 16;
const BLACK_UV: u8 = 128;

/// A layer's landing place in the output frame, in whole samples, together
/// with the part of the layer's own picture that fills it.
///
/// Everything is even-aligned because the chroma planes are stored at half
/// resolution: an odd offset has no chroma sample to copy from, and an odd size
/// has half of one to copy into.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Placement {
    pub(super) x: u32,
    pub(super) y: u32,
    pub(super) w: u32,
    pub(super) h: u32,
    /// `[u0, v0, u1, v1]`, each 0..1 across the layer's picture. The whole of
    /// it unless the layer runs off an edge of the canvas, in which case this
    /// is the part still on it — which is what keeps a half-off overlay showing
    /// half its picture rather than all of it squeezed into the strip that fits.
    pub(super) src: [f32; 4],
}

pub(super) fn even_down(v: u32) -> u32 {
    v & !1
}

pub(super) fn even_up(v: u32) -> u32 {
    v.saturating_add(1) & !1
}

/// Snap `layer` onto the output's sample grid, or `None` if nothing of it
/// survives — off the canvas, or squeezed below the 2x2 a chroma sample needs.
///
/// Each edge goes to the nearest even sample rather than outward to the next
/// one. Outward never loses a column of picture, but it can claim up to two
/// that the layer does not cover, and the crop below has nothing to put there
/// but a stretched edge. Half a pixel of placement is the cheaper error.
pub(super) fn place_even(layer: &Layer, out_w: u32, out_h: u32) -> Option<Placement> {
    let [lx, ly, lw, lh] = layer.rect;
    if lw <= 0.0 || lh <= 0.0 {
        return None;
    }
    let [vx, vy, vw, vh] = layer.visible_rect(out_w, out_h)?;
    let x0 = even_down(vx.round() as u32);
    let y0 = even_down(vy.round() as u32);
    let x1 = even_up((vx + vw).round() as u32).min(even_down(out_w));
    let y1 = even_up((vy + vh).round() as u32).min(even_down(out_h));
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    // Re-derived from the snapped rect rather than from the unsnapped one, so
    // the picture stays registered with the box it is being scaled into.
    let frac = |edge: f32, origin: f32, extent: f32| ((edge - origin) / extent).clamp(0.0, 1.0);
    Some(Placement {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
        src: [
            frac(x0 as f32, lx, lw),
            frac(y0 as f32, ly, lh),
            frac(x1 as f32, lx, lw),
            frac(y1 as f32, ly, lh),
        ],
    })
}

pub(super) fn fill_black(f: &mut frame::Video) {
    for (plane, value) in [(0usize, BLACK_Y), (1, BLACK_UV), (2, BLACK_UV)] {
        let stride = f.stride(plane);
        let rows = f.plane_height(plane) as usize;
        let data = f.data_mut(plane);
        for row in 0..rows {
            data[row * stride..row * stride + stride].fill(value);
        }
    }
}

/// Paint `src` onto `dst` at `(x, y)`, letting `alpha` of it through.
///
/// Mixing straight in YUV rather than converting out to light and back is what
/// a dissolve has always been in an editor, and it is what the preview's own
/// blend does to the decoded frames it stacks — the two have to agree more than
/// either has to be photometrically exact.
///
/// Fully opaque is by far the common case (every cut, every clip not currently
/// in a fade), so it drops to a straight copy rather than paying for a
/// per-sample mix that cannot change anything.
pub(super) fn compose(dst: &mut frame::Video, src: &frame::Video, x: u32, y: u32, alpha: f32) {
    // 0..=256 rather than 0..=255: a whole power of two makes the mix below a
    // shift, and 256 is exactly opaque instead of one part in 255 short of it.
    let a = (alpha.clamp(0.0, 1.0) * 256.0).round() as i32;
    if a >= 256 {
        blit(dst, src, x, y);
        return;
    }
    if a <= 0 {
        return;
    }
    for_each_plane(dst, src, x, y, |dst_row, src_row| {
        for i in 0..dst_row.len() {
            let (d, s) = (dst_row[i] as i32, src_row[i] as i32);
            // Rounded rather than truncated: at low alpha a truncating mix
            // never quite reaches the source, so a fade-in would sit a level
            // short of the picture it is bringing up.
            dst_row[i] = (d + (((s - d) * a + 128) >> 8)) as u8;
        }
    });
}

/// Copy `src` into `dst` at `(x, y)`. Both are YUV420P, and `x`/`y` are even,
/// so the chroma planes copy at exactly half the offset.
pub(super) fn blit(dst: &mut frame::Video, src: &frame::Video, x: u32, y: u32) {
    for_each_plane(dst, src, x, y, |dst_row, src_row| {
        dst_row.copy_from_slice(src_row);
    });
}

/// Walk the overlapping rows of `src` placed at `(x, y)` in `dst`, plane by
/// plane, handing each pair to `f` already trimmed to the samples they share.
///
/// The one place that knows YUV420P's shape — that plane 0 is full resolution
/// and planes 1 and 2 are half in both directions — so a copy and a mix cannot
/// disagree about where a row of chroma begins.
fn for_each_plane(
    dst: &mut frame::Video,
    src: &frame::Video,
    x: u32,
    y: u32,
    mut f: impl FnMut(&mut [u8], &[u8]),
) {
    for plane in 0..3 {
        let (dx, dy) = if plane == 0 { (x, y) } else { (x / 2, y / 2) };
        let width = src.plane_width(plane) as usize;
        let rows = src.plane_height(plane) as usize;
        let src_stride = src.stride(plane);
        let dst_stride = dst.stride(plane);
        let dst_rows = dst.plane_height(plane) as usize;
        // The placement is derived from these same dimensions, but clamp
        // anyway: a source whose frame size changes mid-stream would otherwise
        // index past the canvas.
        let rows = rows.min(dst_rows.saturating_sub(dy as usize));
        let width = width.min(dst_stride.saturating_sub(dx as usize));
        let src_data = src.data(plane);
        let dst_data = dst.data_mut(plane);
        for row in 0..rows {
            let from = row * src_stride;
            let to = (dy as usize + row) * dst_stride + dx as usize;
            f(&mut dst_data[to..to + width], &src_data[from..from + width]);
        }
    }
}

/// A colour in the output's YUV, obtained the same way every picture in the
/// render gets there: by handing it to the scaler.
///
/// Two by two because that is the smallest frame YUV420P can represent — one
/// chroma sample needs a full quad of luma. Doing the matrix by hand would be
/// six lines shorter and would quietly disagree with the footage the title sits
/// over the first time either side changed its coefficients.
pub(super) fn color_to_yuv(color: [f32; 4]) -> (u8, u8, u8) {
    let mut rgb = frame::Video::new(format::Pixel::RGBA, 2, 2);
    let stride = rgb.stride(0);
    let bytes = [
        (color[0].clamp(0.0, 1.0) * 255.0).round() as u8,
        (color[1].clamp(0.0, 1.0) * 255.0).round() as u8,
        (color[2].clamp(0.0, 1.0) * 255.0).round() as u8,
        255,
    ];
    let data = rgb.data_mut(0);
    for row in 0..2 {
        for col in 0..2 {
            let at = row * stride + col * 4;
            data[at..at + 4].copy_from_slice(&bytes);
        }
    }
    let mut scaler = None;
    let mut yuv = frame::Video::empty();
    if !scale_into(&mut scaler, &rgb, &mut yuv, 2, 2) {
        // The scaler refusing a 2x2 conversion would be remarkable; mid grey
        // is at least a colour rather than a panic in a background worker.
        return (128, 128, 128);
    }
    (yuv.data(0)[0], yuv.data(1)[0], yuv.data(2)[0])
}

/// Paint a flat `color` onto `dst` at `(x, y)`, as much of it at each pixel as
/// `mask` says and `alpha` allows.
///
/// The picture counterpart of a fader: the title never varies in colour, only
/// in how much of it is there, so this is a mix towards a constant rather than
/// towards another picture.
pub(super) fn compose_color(
    dst: &mut frame::Video,
    mask: &frame::Video,
    x: u32,
    y: u32,
    color: (u8, u8, u8),
    alpha: f32,
) {
    let alpha = alpha.clamp(0.0, 1.0);
    if alpha <= 0.0 {
        return;
    }
    let w = mask.width() as usize;
    let h = mask.height() as usize;
    let mask_stride = mask.stride(0);
    let mask_data = mask.data(0);
    let cov = |row: usize, col: usize| mask_data[row * mask_stride + col] as i32;
    // Folded in once here rather than per sample: coverage is already the
    // per-pixel part, and the clip's opacity is the same number everywhere.
    let scale = |c: i32| ((c as f32) * alpha) as i32;

    // Luma, one sample per pixel.
    let stride = dst.stride(0);
    let rows = h.min((dst.plane_height(0) as usize).saturating_sub(y as usize));
    let cols = w.min((dst.plane_width(0) as usize).saturating_sub(x as usize));
    let data = dst.data_mut(0);
    for row in 0..rows {
        let at = (y as usize + row) * stride + x as usize;
        for col in 0..cols {
            data[at + col] = mix(data[at + col], color.0, scale(cov(row, col)));
        }
    }

    // Chroma, one sample per 2x2 block. The block's coverage is the mean of
    // its four, so a diagonal edge fades its colour off at the same rate its
    // luma does instead of stepping a whole sample at a time.
    for (plane, value) in [(1usize, color.1), (2, color.2)] {
        let stride = dst.stride(plane);
        let rows = (h / 2).min((dst.plane_height(plane) as usize).saturating_sub(y as usize / 2));
        let cols = (w / 2).min((dst.plane_width(plane) as usize).saturating_sub(x as usize / 2));
        let data = dst.data_mut(plane);
        for row in 0..rows {
            let at = (y as usize / 2 + row) * stride + x as usize / 2;
            for col in 0..cols {
                let mean = (cov(row * 2, col * 2)
                    + cov(row * 2, col * 2 + 1)
                    + cov(row * 2 + 1, col * 2)
                    + cov(row * 2 + 1, col * 2 + 1))
                    / 4;
                data[at + col] = mix(data[at + col], value, scale(mean));
            }
        }
    }
}

/// `d` moved `a` of the way to `s`, where `a` is 0..=255.
fn mix(d: u8, s: u8, a: i32) -> u8 {
    let (d, s) = (d as i32, s as i32);
    // Over 255 rather than 256 so full coverage reaches the source exactly; the
    // rounding term keeps a low-coverage edge from sitting a level short.
    (d + ((s - d) * a + 127) / 255) as u8
}

/// Make `f` a GRAY8 frame of exactly `w x h`.
pub(super) fn alloc_gray(f: &mut frame::Video, w: u32, h: u32) {
    if f.width() != w || f.height() != h || f.format() != format::Pixel::GRAY8 {
        *f = frame::Video::new(format::Pixel::GRAY8, w, h);
    }
}

/// [`crop_yuv420p`] for a single 8-bit plane, which has no chroma to keep in
/// step and so no alignment to respect.
pub(super) fn crop_gray8(dst: &mut frame::Video, src: &frame::Video, x: u32, y: u32) {
    let width = (dst.width() as usize).min((src.stride(0)).saturating_sub(x as usize));
    let rows = (dst.height() as usize).min((src.height() as usize).saturating_sub(y as usize));
    let src_stride = src.stride(0);
    let dst_stride = dst.stride(0);
    let src_data = src.data(0);
    let dst_data = dst.data_mut(0);
    for row in 0..rows {
        let from = (y as usize + row) * src_stride + x as usize;
        let to = row * dst_stride;
        dst_data[to..to + width].copy_from_slice(&src_data[from..from + width]);
    }
}

/// [`scale_into`] for coverage: one plane in, one plane out, no colour
/// conversion in between.
pub(super) fn scale_gray_into(
    scaler: &mut Option<scaling::Context>,
    src: &frame::Video,
    dst: &mut frame::Video,
    w: u32,
    h: u32,
) -> bool {
    if dst.width() != w || dst.height() != h || dst.format() != format::Pixel::GRAY8 {
        *dst = frame::Video::empty();
    }
    let ctx = match scaler.as_mut() {
        Some(s) => s,
        None => match scaling::Context::get(
            format::Pixel::GRAY8,
            src.width(),
            src.height(),
            format::Pixel::GRAY8,
            w,
            h,
            scaling::Flags::BILINEAR,
        ) {
            Ok(s) => scaler.insert(s),
            Err(e) => {
                log::error!("title scaler setup failed: {e}");
                return false;
            }
        },
    };
    ctx.cached(
        format::Pixel::GRAY8,
        src.width(),
        src.height(),
        format::Pixel::GRAY8,
        w,
        h,
        scaling::Flags::BILINEAR,
    );
    if let Err(e) = ctx.run(src, dst) {
        log::error!("title scale failed: {e}");
        return false;
    }
    true
}

/// Scale `src` into `dst` as YUV420P at `w x h`, building or re-deriving
/// `scaler` to suit. Reports whether `dst` came out usable.
///
/// The scaler is built from the frame rather than from the decoder because a
/// decoder's format is not settled until something has actually come out of it,
/// and re-derived on every call because both ends move now: a source can change
/// resolution mid-stream, and a layer's placement changes the moment its clip
/// is scaled or dragged.
pub(super) fn scale_into(
    scaler: &mut Option<scaling::Context>,
    src: &frame::Video,
    dst: &mut frame::Video,
    w: u32,
    h: u32,
) -> bool {
    // `run` allocates only into an empty frame and refuses one already sized
    // for a different rect, so a changed target means starting the buffer over
    // rather than handing it a picture it has no room for. An unallocated frame
    // reports a zero size, so it takes this branch harmlessly on the first call.
    if dst.width() != w || dst.height() != h {
        *dst = frame::Video::empty();
    }
    let ctx = match scaler.as_mut() {
        Some(s) => s,
        None => match scaling::Context::get(
            src.format(),
            src.width(),
            src.height(),
            format::Pixel::YUV420P,
            w,
            h,
            scaling::Flags::BILINEAR,
        ) {
            Ok(s) => scaler.insert(s),
            Err(e) => {
                log::error!("export scaler setup failed: {e}");
                return false;
            }
        },
    };
    ctx.cached(
        src.format(),
        src.width(),
        src.height(),
        format::Pixel::YUV420P,
        w,
        h,
        scaling::Flags::BILINEAR,
    );
    if let Err(e) = ctx.run(src, dst) {
        log::error!("export scale failed: {e}");
        return false;
    }
    true
}

/// Make `f` a YUV420P frame of exactly `w x h`, reusing its buffer when it
/// already is one.
pub(super) fn alloc_yuv(f: &mut frame::Video, w: u32, h: u32) {
    if f.width() != w || f.height() != h || f.format() != format::Pixel::YUV420P {
        *f = frame::Video::new(format::Pixel::YUV420P, w, h);
    }
}

/// Copy the sub-rectangle of `src` starting at `(x, y)` and the size of `dst`
/// into `dst`. Both are YUV420P and `x`/`y` are even, so chroma copies at
/// exactly half the offset — the same rule [`blit`] follows in the other
/// direction.
pub(super) fn crop_yuv420p(dst: &mut frame::Video, src: &frame::Video, x: u32, y: u32) {
    for plane in 0..3 {
        let (sx, sy) = if plane == 0 { (x, y) } else { (x / 2, y / 2) };
        let width = dst.plane_width(plane) as usize;
        let rows = dst.plane_height(plane) as usize;
        let src_stride = src.stride(plane);
        let dst_stride = dst.stride(plane);
        let rows = rows.min((src.plane_height(plane) as usize).saturating_sub(sy as usize));
        let width = width.min(src_stride.saturating_sub(sx as usize));
        let src_data = src.data(plane);
        let dst_data = dst.data_mut(plane);
        for row in 0..rows {
            let from = (sy as usize + row) * src_stride + sx as usize;
            let to = row * dst_stride;
            dst_data[to..to + width].copy_from_slice(&src_data[from..from + width]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::SourceId;

    fn layer(rect: [f32; 4]) -> Layer {
        Layer {
            source: SourceId(0),
            source_time: 0.0,
            rect,
            alpha: 1.0,
        }
    }

    /// The ordinary case, and the one that has to stay a single scaling pass:
    /// a clip filling the canvas takes all of its own picture.
    #[test]
    fn a_layer_filling_the_canvas_asks_for_all_of_its_picture() {
        let p = place_even(&layer([0.0, 0.0, 1920.0, 1080.0]), 1920, 1080).unwrap();
        assert_eq!((p.x, p.y, p.w, p.h), (0, 0, 1920, 1080));
        assert_eq!(p.src, [0.0, 0.0, 1.0, 1.0]);
    }

    #[test]
    fn every_edge_lands_on_an_even_sample() {
        // Odd numbers all round: chroma is half resolution, so an odd offset or
        // size would put the planes half a sample out of step.
        let p = place_even(&layer([13.5, 7.25, 401.0, 333.0]), 1280, 720).unwrap();
        for v in [p.x, p.y, p.w, p.h] {
            assert_eq!(v % 2, 0, "{v} is odd");
        }
        assert!(p.x + p.w <= 1280 && p.y + p.h <= 720);
    }

    /// A layer pushed off the left edge keeps the part still on canvas, and
    /// says which part of its picture that is — without this the visible strip
    /// would show the whole frame squeezed into it.
    #[test]
    fn a_layer_off_an_edge_is_cut_rather_than_squeezed() {
        let p = place_even(&layer([-200.0, 0.0, 400.0, 1080.0]), 1920, 1080).unwrap();
        assert_eq!((p.x, p.y, p.w, p.h), (0, 0, 200, 1080));
        assert_eq!(p.src, [0.5, 0.0, 1.0, 1.0]);
    }

    #[test]
    fn a_layer_scaled_past_the_canvas_is_cropped_to_it() {
        let p = place_even(&layer([-1920.0, -1080.0, 5760.0, 3240.0]), 1920, 1080).unwrap();
        assert_eq!((p.x, p.y, p.w, p.h), (0, 0, 1920, 1080));
        assert_eq!(p.src, [1.0 / 3.0, 1.0 / 3.0, 2.0 / 3.0, 2.0 / 3.0]);
    }

    #[test]
    fn a_layer_entirely_off_canvas_places_nowhere() {
        assert!(place_even(&layer([1920.0, 0.0, 400.0, 200.0]), 1920, 1080).is_none());
        assert!(place_even(&layer([0.0, 0.0, 0.0, 0.0]), 1920, 1080).is_none());
    }

    /// Below a 2x2 there is no chroma sample to write, so a layer squeezed to
    /// a sliver drops out rather than producing a half-written plane.
    #[test]
    fn a_sliver_thinner_than_a_chroma_sample_places_nowhere() {
        assert!(place_even(&layer([1919.5, 0.0, 1.0, 200.0]), 1920, 1080).is_none());
    }

    fn flat_frame(w: u32, h: u32, y: u8, uv: u8) -> frame::Video {
        let mut f = frame::Video::new(format::Pixel::YUV420P, w, h);
        for (plane, value) in [(0usize, y), (1, uv), (2, uv)] {
            let stride = f.stride(plane);
            let rows = f.plane_height(plane) as usize;
            let data = f.data_mut(plane);
            for row in 0..rows {
                data[row * stride..row * stride + stride].fill(value);
            }
        }
        f
    }

    /// Half opacity lands half way, which is the whole of what a dissolve is.
    /// The rounded mix is what keeps this exact rather than a level short.
    #[test]
    fn composing_at_half_alpha_lands_between_the_two_pictures() {
        let mut canvas = flat_frame(4, 4, 0, 0);
        let over = flat_frame(2, 2, 200, 100);
        compose(&mut canvas, &over, 0, 0, 0.5);
        assert_eq!(canvas.data(0)[0], 100);
        assert_eq!(canvas.data(1)[0], 50);
        // Outside the composed rect the canvas is untouched.
        assert_eq!(canvas.data(0)[2], 0);
    }

    #[test]
    fn composing_at_full_alpha_replaces_the_canvas_exactly() {
        let mut canvas = flat_frame(4, 4, 16, 128);
        let over = flat_frame(2, 2, 235, 64);
        compose(&mut canvas, &over, 2, 2, 1.0);
        let stride = canvas.stride(0);
        assert_eq!(canvas.data(0)[2 * stride + 2], 235);
        assert_eq!(canvas.data(0)[0], 16);
    }

    /// A layer faded to nothing must leave no mark at all — a single level of
    /// leakage across a whole frame is a visible haze on a dark picture.
    #[test]
    fn composing_at_zero_alpha_leaves_the_canvas_alone() {
        let mut canvas = flat_frame(4, 4, 16, 128);
        let over = flat_frame(4, 4, 235, 64);
        compose(&mut canvas, &over, 0, 0, 0.0);
        assert_eq!(canvas.data(0)[0], 16);
        assert_eq!(canvas.data(1)[0], 128);
    }
}
