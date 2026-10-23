//! Which RGB/YUV matrix a conversion uses.
//!
//! swscale converts with BT.601 unless told otherwise, whatever the footage
//! says. That was right for SD and is wrong for everything recorded since: HD
//! and up is BT.709, and decoding it with BT.601 shifts saturated colours —
//! reds go orange, greens go yellow — while leaving greys alone, which is why
//! it goes unnoticed. Both the preview (YUV to RGB) and the export's title
//! colours (RGB to YUV) set the matrix here rather than taking that default.

use ffmpeg_next as ffmpeg;
use ffmpeg::color::Space;
use ffmpeg::ffi;
use ffmpeg::software::scaling;
use std::ffi::c_int;
use std::ptr;

/// The swscale matrix for footage tagged `space`, `height` lines tall.
///
/// Untagged footage gets the guess players make: BT.709 from 720 lines up,
/// BT.601 below.
pub(crate) fn matrix_for(space: Space, height: u32) -> c_int {
    match space {
        Space::BT709 => ffi::SWS_CS_ITU709,
        Space::BT470BG | Space::SMPTE170M => ffi::SWS_CS_ITU601,
        Space::SMPTE240M => ffi::SWS_CS_SMPTE240M,
        Space::FCC => ffi::SWS_CS_FCC,
        Space::BT2020NCL | Space::BT2020CL => ffi::SWS_CS_BT2020,
        _ if height >= 720 => ffi::SWS_CS_ITU709,
        _ => ffi::SWS_CS_ITU601,
    }
}

/// Switch `scaler` to `matrix` (one of the `SWS_CS_*` values) on both sides,
/// keeping the ranges and picture adjustments it already chose. False if
/// swscale refused, which leaves it on its default.
///
/// Only the YUV side of a conversion reads its table, so setting both is the
/// same call for YUV to RGB as for RGB to YUV.
pub(crate) fn use_matrix(scaler: &mut scaling::Context, matrix: c_int) -> bool {
    // SAFETY: the context is live for the borrow, every out-pointer refers to
    // a local, and the coefficient table swscale hands back is static.
    unsafe {
        let ctx = scaler.as_mut_ptr();
        let (mut inv_table, mut table) = (ptr::null_mut(), ptr::null_mut());
        let (mut src_range, mut dst_range): (c_int, c_int) = (0, 0);
        let (mut brightness, mut contrast, mut saturation): (c_int, c_int, c_int) = (0, 0, 0);
        if ffi::sws_getColorspaceDetails(
            ctx,
            &mut inv_table,
            &mut src_range,
            &mut table,
            &mut dst_range,
            &mut brightness,
            &mut contrast,
            &mut saturation,
        ) < 0
        {
            return false;
        }
        let coefficients = ffi::sws_getCoefficients(matrix);
        ffi::sws_setColorspaceDetails(
            ctx,
            coefficients,
            src_range,
            coefficients,
            dst_range,
            brightness,
            contrast,
            saturation,
        ) >= 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tag_wins_over_the_size_guess() {
        assert_eq!(matrix_for(Space::BT709, 480), ffi::SWS_CS_ITU709);
        assert_eq!(matrix_for(Space::SMPTE170M, 2160), ffi::SWS_CS_ITU601);
    }

    /// The preview's direction. BT.709 red (Y 63, Cb 102, Cr 240) decodes
    /// back to full red only through the BT.709 matrix; through the default it
    /// comes out a darker red, around 233.
    #[test]
    fn bt709_red_decodes_to_red() {
        use ffmpeg::{format, frame};
        let mut yuv = frame::Video::new(format::Pixel::YUV420P, 2, 2);
        for (plane, value) in [(0usize, 63u8), (1, 102), (2, 240)] {
            let stride = yuv.stride(plane);
            let rows = yuv.plane_height(plane) as usize;
            let data = yuv.data_mut(plane);
            for row in 0..rows {
                data[row * stride..row * stride + stride].fill(value);
            }
        }
        let decode = |matrix: Option<c_int>| {
            let mut scaler = scaling::Context::get(
                format::Pixel::YUV420P,
                2,
                2,
                format::Pixel::RGBA,
                2,
                2,
                scaling::Flags::BILINEAR,
            )
            .unwrap();
            if let Some(m) = matrix {
                assert!(use_matrix(&mut scaler, m));
            }
            let mut rgb = frame::Video::empty();
            scaler.run(&yuv, &mut rgb).unwrap();
            let px = &rgb.data(0)[..3];
            (px[0], px[1], px[2])
        };
        let (r, g, b) = decode(Some(ffi::SWS_CS_ITU709));
        assert!(r >= 250 && g <= 5 && b <= 5, "{r},{g},{b}");
        let (r, _, _) = decode(None);
        assert!(r <= 240, "default matrix should differ, got red {r}");
    }

    #[test]
    fn untagged_footage_is_guessed_from_its_height() {
        assert_eq!(matrix_for(Space::Unspecified, 1440), ffi::SWS_CS_ITU709);
        assert_eq!(matrix_for(Space::Unspecified, 720), ffi::SWS_CS_ITU709);
        assert_eq!(matrix_for(Space::Unspecified, 576), ffi::SWS_CS_ITU601);
    }
}
