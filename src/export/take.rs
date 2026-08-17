//! The source side of a render: one decoder or one baked title, each able to
//! hand back the picture due at a given time, already scaled into the
//! placement it was asked for.
//!
//! Both kinds answer the same two questions — how big is your picture, and
//! what does it look like here — which is what lets the encode loop treat a
//! title and a clip of footage as the same thing.

use ffmpeg_next as ffmpeg;
use ffmpeg::software::scaling;
use ffmpeg::{codec, format, frame};

use crate::title::{self, Title};

use super::raster::{
    alloc_gray, alloc_yuv, color_to_yuv, crop_gray8, crop_yuv420p, even_down, even_up,
    scale_gray_into, scale_into, Placement,
};

/// How far ahead a source decoder walks before giving up and seeking. Same
/// reasoning as the preview decoder: keeps per-frame work bounded when the
/// timeline jumps around inside a source.
const FORWARD_DECODE_BUDGET: f64 = 1.0;

/// One source file, decoded on demand for the render. Mirrors the preview
/// decoder's strategy — walk forward when the next request is close, seek when
/// it is behind or far ahead — but stages frames in CPU memory, already scaled
/// into the output's pixel format.
pub(super) struct VideoTake {
    pub(super) ictx: format::context::Input,
    pub(super) decoder: codec::decoder::Video,
    pub(super) stream_index: usize,
    pub(super) time_base_seconds: f64,
    pub(super) width: u32,
    pub(super) height: u32,
    /// Whatever the last picture was scaled into place with. Rebuilt through
    /// `cached` whenever the placement changes, which it does the moment two
    /// clips of one source sit at different sizes.
    pub(super) scaler: Option<scaling::Context>,
    /// Source pixel format to YUV420P at the source's own size, for the crop
    /// path below. Only built when a layer actually runs off the canvas.
    pub(super) native_scaler: Option<scaling::Context>,
    /// Decoded but not yet due: the frame after whatever is staged.
    pub(super) pending: Option<(frame::Video, f64)>,
    /// The decoded frame covering the requested time, kept unscaled.
    ///
    /// It used to be scaled the moment it fell due, because there was one place
    /// it could ever land. Now the same frame can be asked for at two sizes —
    /// two clips of one source on two tracks — so the source picture is what is
    /// held and the scaling happens per request.
    pub(super) due: Option<(frame::Video, f64)>,
    /// The picture as last scaled into place, and the request it answers.
    pub(super) staged: frame::Video,
    pub(super) staged_for: Option<(f64, Placement)>,
    /// Scratch for the crop path: the source converted to YUV420P at its own
    /// size, and the sub-rectangle of it that is still on canvas.
    pub(super) native: frame::Video,
    pub(super) cropped: frame::Video,
}

impl VideoTake {
    pub(super) fn open(path: &str) -> Result<Self, ffmpeg::Error> {
        let ictx = format::input(&path)?;
        let (stream_index, time_base_seconds, parameters) = {
            let stream = ictx
                .streams()
                .best(ffmpeg::media::Type::Video)
                .ok_or(ffmpeg::Error::StreamNotFound)?;
            let tb = stream.time_base();
            (
                stream.index(),
                tb.numerator() as f64 / tb.denominator() as f64,
                stream.parameters(),
            )
        };
        let decoder = codec::context::Context::from_parameters(parameters)?
            .decoder()
            .video()?;
        Ok(Self {
            width: decoder.width(),
            height: decoder.height(),
            ictx,
            decoder,
            stream_index,
            time_base_seconds,
            scaler: None,
            native_scaler: None,
            pending: None,
            due: None,
            staged: frame::Video::empty(),
            staged_for: None,
            native: frame::Video::empty(),
            cropped: frame::Video::empty(),
        })
    }

    /// The source's picture size, which is what decides where a clip of it
    /// lands on the canvas.
    pub(super) fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Make the frame covering source time `t` the due one. Returns false only
    /// when the source yielded nothing at all, in which case the caller leaves
    /// this layer off the canvas.
    pub(super) fn stage(&mut self, t: f64) -> bool {
        match self.due_pts() {
            Some(p) if t >= p && t <= p + FORWARD_DECODE_BUDGET => self.advance_to(t),
            _ => self.seek(t),
        }
        self.due.is_some()
    }

    fn due_pts(&self) -> Option<f64> {
        self.due.as_ref().map(|(_, pts)| *pts)
    }

    /// The due frame scaled into `placement`, ready to compose.
    ///
    /// Cached on the pair of frame and placement, so a source held on screen —
    /// a still, or a clip running slower than the output rate — is scaled once
    /// rather than once per output frame.
    pub(super) fn render(&mut self, placement: Placement) -> Option<&frame::Video> {
        let pts = self.due_pts()?;
        if self.staged_for != Some((pts, placement)) {
            let ok = if placement.src == [0.0, 0.0, 1.0, 1.0] {
                // The whole picture is on canvas, which is every clip that has
                // not been pushed past an edge: straight from the decoder's
                // format into the output rect in one pass, exactly as it went
                // before there were layers.
                let (due, _) = self.due.take()?;
                let ok = scale_into(
                    &mut self.scaler,
                    &due,
                    &mut self.staged,
                    placement.w,
                    placement.h,
                );
                self.due = Some((due, pts));
                ok
            } else {
                self.render_cropped(placement)
            };
            if !ok {
                self.staged_for = None;
                return None;
            }
            self.staged_for = Some((pts, placement));
        }
        Some(&self.staged)
    }

    /// The part-of-a-picture path: convert to YUV420P at the source's own size,
    /// cut out the piece that is still on canvas, and scale only that.
    ///
    /// Scaling the whole layer and then copying the visible corner of it would
    /// be one pass fewer, but a clip scaled up and pushed off an edge would
    /// have the editor allocating a picture many times the canvas to throw
    /// nearly all of it away.
    fn render_cropped(&mut self, placement: Placement) -> bool {
        let Some((due, pts)) = self.due.take() else {
            return false;
        };
        let ok = self.crop_and_scale(&due, placement);
        self.due = Some((due, pts));
        ok
    }

    fn crop_and_scale(&mut self, due: &frame::Video, placement: Placement) -> bool {
        let (sw, sh) = (due.width(), due.height());
        let [u0, v0, u1, v1] = placement.src;
        let x0 = even_down((u0 * sw as f32).round() as u32);
        let y0 = even_down((v0 * sh as f32).round() as u32);
        let x1 = even_up((u1 * sw as f32).round() as u32).min(even_down(sw));
        let y1 = even_up((v1 * sh as f32).round() as u32).min(even_down(sh));
        if x1 <= x0 || y1 <= y0 {
            return false;
        }
        if !scale_into(&mut self.native_scaler, due, &mut self.native, sw, sh) {
            return false;
        }
        alloc_yuv(&mut self.cropped, x1 - x0, y1 - y0);
        crop_yuv420p(&mut self.cropped, &self.native, x0, y0);
        scale_into(
            &mut self.scaler,
            &self.cropped,
            &mut self.staged,
            placement.w,
            placement.h,
        )
    }

    fn advance_to(&mut self, t: f64) {
        if self.pending.is_none() {
            match self.decode_next() {
                Some(p) => self.pending = Some(p),
                // Past the end of the source: keep showing the last frame,
                // which is what the trim bounds should have prevented anyway.
                None => return,
            }
        }
        if self.pending.as_ref().unwrap().1 > t {
            return;
        }
        loop {
            match self.decode_next() {
                Some((next, next_pts)) => {
                    if next_pts <= t {
                        // The frame we were holding is already superseded, so
                        // it never needs to be looked at again.
                        self.pending = Some((next, next_pts));
                    } else {
                        let due = self.pending.take().unwrap();
                        self.set_due(due);
                        self.pending = Some((next, next_pts));
                        return;
                    }
                }
                None => {
                    if let Some(due) = self.pending.take() {
                        self.set_due(due);
                    }
                    return;
                }
            }
        }
    }

    fn seek(&mut self, t: f64) {
        let ts = (t.max(0.0) * 1_000_000.0) as i64;
        let _ = self.ictx.seek(ts, ..);
        self.decoder.flush();
        self.pending = None;
        self.due = None;
        self.staged_for = None;

        let mut last: Option<(frame::Video, f64)> = None;
        loop {
            match self.decode_next() {
                Some((frame, pts)) => {
                    if pts > t {
                        self.set_due(last.take().unwrap_or_else(|| (frame.clone(), pts)));
                        self.pending = Some((frame, pts));
                        return;
                    }
                    last = Some((frame, pts));
                }
                None => {
                    if let Some(due) = last.take() {
                        self.set_due(due);
                    }
                    return;
                }
            }
        }
    }

    /// Adopt a newly-due source frame, retiring whatever was scaled from the
    /// last one — the staged picture is only ever an answer about a particular
    /// frame, and holding it past that frame is how a render starts showing a
    /// picture one frame stale.
    fn set_due(&mut self, due: (frame::Video, f64)) {
        self.due = Some(due);
        self.staged_for = None;
    }

    fn decode_next(&mut self) -> Option<(frame::Video, f64)> {
        let mut frame = frame::Video::empty();
        loop {
            match self.decoder.receive_frame(&mut frame) {
                Ok(()) => {
                    let pts = frame.pts().unwrap_or(0) as f64 * self.time_base_seconds;
                    return Some((frame, pts));
                }
                Err(_) => match self.next_packet() {
                    Some(packet) => {
                        let _ = self.decoder.send_packet(&packet);
                    }
                    None => return None,
                },
            }
        }
    }

    fn next_packet(&mut self) -> Option<ffmpeg::Packet> {
        let mut iter = self.ictx.packets();
        loop {
            let (stream, packet) = iter.next()?;
            if stream.index() == self.stream_index {
                return Some(packet);
            }
        }
    }

}

/// A title, rasterized once for the whole render.
///
/// Nothing here decodes. What varies across a title's frame is only how much of
/// one colour is present, so what is carried is a coverage mask and three
/// numbers — not a picture. That is also why it scales in one plane rather than
/// three: a title moved and shrunk on the canvas resamples a quarter of the
/// data a clip of footage would.
pub(super) struct TitleTake {
    /// Coverage at canvas size, held as a frame so the scaler can take it.
    pub(super) mask: frame::Video,
    pub(super) cropped: frame::Video,
    pub(super) scaled: frame::Video,
    pub(super) scaler: Option<scaling::Context>,
    /// The title's colour, already in the output's own YUV.
    pub(super) color: (u8, u8, u8),
    /// The colour's own alpha, before the clip's opacity and fades multiply in.
    pub(super) alpha: f32,
    pub(super) width: u32,
    pub(super) height: u32,
}

impl TitleTake {
    pub(super) fn bake(title: &Title, width: u32, height: u32) -> Self {
        let mask = title::rasterize(title, width, height);
        let mut frame = frame::Video::new(format::Pixel::GRAY8, width, height);
        let stride = frame.stride(0);
        let data = frame.data_mut(0);
        for row in 0..height as usize {
            let from = row * width as usize;
            data[row * stride..row * stride + width as usize]
                .copy_from_slice(&mask.coverage[from..from + width as usize]);
        }
        Self {
            mask: frame,
            cropped: frame::Video::empty(),
            scaled: frame::Video::empty(),
            scaler: None,
            color: color_to_yuv(title.color),
            alpha: title.color[3],
            width,
            height,
        }
    }

    pub(super) fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The coverage this placement needs: the part of the mask still on canvas,
    /// at the size it is being drawn.
    ///
    /// The common case — a title left where it was made — is the mask itself,
    /// untouched: it was rasterized at canvas size, so filling the canvas asks
    /// for no crop and no resample.
    pub(super) fn stage(&mut self, placement: Placement) -> Option<&frame::Video> {
        let full = placement.src == [0.0, 0.0, 1.0, 1.0];
        if full && placement.w == self.width && placement.h == self.height {
            return Some(&self.mask);
        }
        let source = if full {
            &self.mask
        } else {
            let [u0, v0, u1, v1] = placement.src;
            let x0 = (u0 * self.width as f32).round() as u32;
            let y0 = (v0 * self.height as f32).round() as u32;
            let x1 = (u1 * self.width as f32).round().min(self.width as f32) as u32;
            let y1 = (v1 * self.height as f32).round().min(self.height as f32) as u32;
            if x1 <= x0 || y1 <= y0 {
                return None;
            }
            alloc_gray(&mut self.cropped, x1 - x0, y1 - y0);
            crop_gray8(&mut self.cropped, &self.mask, x0, y0);
            &self.cropped
        };
        // Already exactly the rect it is drawn at, which is what a full-size
        // title pushed off an edge crops down to. Nothing left to resample.
        if source.width() == placement.w && source.height() == placement.h {
            return Some(source);
        }
        if !scale_gray_into(
            &mut self.scaler,
            source,
            &mut self.scaled,
            placement.w,
            placement.h,
        ) {
            return None;
        }
        Some(&self.scaled)
    }
}
