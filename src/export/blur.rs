//! The export's half of a blur: [`crate::effects`]' plane arithmetic run over
//! ffmpeg frames.
//!
//! The preview blurs encoded RGB; this blurs YUV. They agree because the
//! conversion between the two is linear, and a blur commutes with anything
//! linear — so blurring Y, U and V is blurring the encoded R, G and B the
//! preview does. Chroma is at half resolution here, so it is halved and blurred
//! on its own grid, which lands it at exactly the size the luma's work picture
//! needs.
//!
//! A picture with a mask is blurred premultiplied, as the preview's is: a
//! transparent pixel's colour is whatever the file happened to store there,
//! and averaging it in at full weight would bleed that into the edge of
//! whatever is opaque. Premultiplying and dividing back out is unmoved by the
//! offsets YUV carries — black at 16, grey chroma at 128 — so it means the
//! same here as on the preview's RGB.

use ffmpeg_next as ffmpeg;
use ffmpeg::frame;

use crate::effects::{blur_plane, halve_plane, BlurPlan, Plane};

use super::raster::{alloc_gray, alloc_yuv};

/// Below this much coverage a blurred sample has no colour worth recovering.
const MIN_COVERAGE: f32 = 1.0 / 1024.0;

fn read_plane(f: &frame::Video, plane: usize) -> Plane {
    let (w, h) = (f.plane_width(plane) as usize, f.plane_height(plane) as usize);
    let stride = f.stride(plane);
    let data = f.data(plane);
    let mut p = Plane::new(w, h);
    for y in 0..h {
        for (x, v) in data[y * stride..y * stride + w].iter().enumerate() {
            p.data[y * w + x] = *v as f32;
        }
    }
    p
}

fn write_plane(f: &mut frame::Video, plane: usize, p: &Plane) {
    let (w, h) = (
        (f.plane_width(plane) as usize).min(p.w),
        (f.plane_height(plane) as usize).min(p.h),
    );
    let stride = f.stride(plane);
    let data = f.data_mut(plane);
    for y in 0..h {
        for x in 0..w {
            data[y * stride + x] = p.data[y * p.w + x].round().clamp(0.0, 255.0) as u8;
        }
    }
}

/// Blur one plane, premultiplied by `coverage` (0 to 1, same size) when there
/// is one.
fn blur_with(p: &Plane, coverage: Option<(&Plane, &Plane)>, plan: BlurPlan) -> Plane {
    let Some((cover, blurred_cover)) = coverage else {
        return blur_plane(p, plan);
    };
    let mut pre = p.clone();
    for (v, c) in pre.data.iter_mut().zip(&cover.data) {
        *v *= c;
    }
    let mut out = blur_plane(&pre, plan);
    for (v, c) in out.data.iter_mut().zip(&blurred_cover.data) {
        *v = if *c > MIN_COVERAGE { *v / c } else { 0.0 };
    }
    out
}

/// Blur the YUV420P `src` into `out`, at the plan's work size. With a `mask`
/// (GRAY8, the same size) the picture is blurred premultiplied and the mask is
/// blurred into `out_mask` alongside it.
pub(super) fn blur_yuv(
    src: &frame::Video,
    mask: Option<&frame::Video>,
    plan: BlurPlan,
    out: &mut frame::Video,
    out_mask: &mut frame::Video,
) {
    let (w, h) = plan.work_size(src.width(), src.height());
    alloc_yuv(out, w, h);

    // Coverage on the luma grid, and halved once onto the chroma grid, each
    // with its blurred counterpart for undoing the premultiply.
    let cover = mask.map(|m| {
        let mut luma = read_plane(m, 0);
        luma.data.iter_mut().for_each(|v| *v /= 255.0);
        let chroma = halve_plane(&luma);
        let (bl, bc) = (blur_plane(&luma, plan), blur_plane(&chroma, chroma_plan(plan)));
        (luma, bl, chroma, bc)
    });

    let y = read_plane(src, 0);
    let luma_cover = cover.as_ref().map(|(l, bl, _, _)| (l, bl));
    write_plane(out, 0, &blur_with(&y, luma_cover, plan));
    for plane in 1..3 {
        let c = read_plane(src, plane);
        let chroma_cover = cover.as_ref().map(|(_, _, c, bc)| (c, bc));
        write_plane(out, plane, &blur_with(&c, chroma_cover, chroma_plan(plan)));
    }

    if let Some((_, blurred_luma_cover, _, _)) = cover {
        alloc_gray(out_mask, w, h);
        let mut m = blurred_luma_cover;
        m.data.iter_mut().for_each(|v| *v *= 255.0);
        write_plane(out_mask, 0, &m);
    }
}

/// The plan on the chroma grid: the same halvings, the blur at half the
/// radius, since a chroma sample spans two luma samples.
fn chroma_plan(plan: BlurPlan) -> BlurPlan {
    BlurPlan { sigma: plan.sigma * 0.5, ..plan }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffmpeg::format;

    fn yuv(w: u32, h: u32, fill: impl Fn(u32, u32) -> u8) -> frame::Video {
        let mut f = frame::Video::new(format::Pixel::YUV420P, w, h);
        let stride = f.stride(0);
        for y in 0..h {
            for x in 0..w {
                f.data_mut(0)[y as usize * stride + x as usize] = fill(x, y);
            }
        }
        for plane in 1..3 {
            let stride = f.stride(plane);
            let rows = f.plane_height(plane) as usize;
            for row in 0..rows {
                f.data_mut(plane)[row * stride..row * stride + stride].fill(128);
            }
        }
        f
    }

    #[test]
    fn the_work_picture_is_a_valid_smaller_frame() {
        let src = yuv(64, 36, |x, _| if x < 32 { 0 } else { 255 });
        let plan = BlurPlan::for_sigma(12.0).unwrap();
        let (mut out, mut mask) = (frame::Video::empty(), frame::Video::empty());
        blur_yuv(&src, None, plan, &mut out, &mut mask);
        assert_eq!((out.width(), out.height()), plan.work_size(64, 36));
        // Grey stays grey: neutral chroma survives the blur untouched.
        assert!(out.data(1)[..out.plane_width(1) as usize].iter().all(|v| *v == 128));
        // The edge has gone soft: something between black and white in the
        // middle, the extremes still at each end.
        let row = &out.data(0)[..out.width() as usize];
        assert!(row[0] < 40 && *row.last().unwrap() > 215, "{row:?}");
        assert!(row.iter().any(|v| *v > 60 && *v < 195), "{row:?}");
    }

    /// Whatever colour a fully transparent pixel stores must not bleed into
    /// the opaque part beside it.
    #[test]
    fn transparent_pixels_lend_no_colour_to_their_neighbours() {
        let src = yuv(32, 8, |x, _| if x < 16 { 200 } else { 0 });
        let mut mask = frame::Video::new(format::Pixel::GRAY8, 32, 8);
        let stride = mask.stride(0);
        for y in 0..8 {
            for x in 0..32 {
                mask.data_mut(0)[y * stride + x] = if x < 16 { 255 } else { 0 };
            }
        }
        let plan = BlurPlan { halvings: 0, sigma: 2.0 };
        let (mut out, mut out_mask) = (frame::Video::empty(), frame::Video::empty());
        blur_yuv(&src, Some(&mask), plan, &mut out, &mut out_mask);
        // Just inside the edge, where an unpremultiplied blur would have
        // pulled the 200 down towards the 0 next door.
        assert!(out.data(0)[15] >= 199, "{}", out.data(0)[15]);
        // The coverage itself is what softens.
        assert!(out_mask.data(0)[15] < 255 && out_mask.data(0)[16] > 0);
    }
}
