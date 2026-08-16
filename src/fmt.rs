//! Turning numbers into the strings the UI shows: timecodes, ruler labels,
//! frame rates, and text clamped to the width it has to fit in.

use crate::text::TextRenderer;

pub(crate) fn format_timecode(t: f64) -> String {
    let total_ms = (t.max(0.0) * 1000.0) as u64;
    let ms = total_ms % 1000;
    let sec = total_ms / 1000;
    let m = sec / 60;
    let s = sec % 60;
    format!("{:02}:{:02}.{:03}", m, s, ms)
}

/// Seconds between ruler ticks at the current zoom, rounded to a value people
/// count in. Paired with [`format_tick_label`], which uses the interval to
/// decide how much precision a label needs.
pub(crate) fn nice_tick_interval(pixels_per_sec: f32) -> f64 {
    const TARGET_PX: f32 = 100.0;
    if pixels_per_sec <= 0.0 {
        return 60.0;
    }
    let raw_secs = (TARGET_PX / pixels_per_sec) as f64;
    let nice = [
        0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0,
    ];
    for &v in &nice {
        if v >= raw_secs {
            return v;
        }
    }
    3600.0
}

pub(crate) fn format_tick_label(t: f64, interval: f64) -> String {
    let total_sec = t.max(0.0);
    if interval < 1.0 {
        let total_ms = (total_sec * 1000.0).round() as u64;
        let s_total = total_ms / 1000;
        let cs = (total_ms % 1000) / 10;
        let m = s_total / 60;
        let s = s_total % 60;
        format!("{}:{:02}.{:02}", m, s, cs)
    } else {
        let total = total_sec.round() as u64;
        let h = total / 3600;
        let m = (total / 60) % 60;
        let s = total % 60;
        if h > 0 {
            format!("{}:{:02}:{:02}", h, m, s)
        } else {
            format!("{}:{:02}", m, s)
        }
    }
}

/// Frame rates as editors write them: whole numbers bare, the broadcast rates
/// to as many decimals as they need and no more (29.97, not 29.970).
pub(crate) fn fmt_fps(fps: f64) -> String {
    if (fps - fps.round()).abs() < 0.001 {
        format!("{}", fps.round() as i64)
    } else {
        let s = format!("{fps:.3}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Shorten `text` so it fits within `max_w` when rendered at `size_px`,
/// appending an ellipsis if truncation happened. Returns the original string
/// when it already fits, so the common case stays zero-allocation at the call
/// site (the caller passes a `&str` either way).
pub(crate) fn truncate_to_width(
    text: &TextRenderer,
    s: &str,
    size_px: f32,
    max_w: f32,
) -> String {
    if text.measure_width(s, size_px) <= max_w {
        return s.to_string();
    }
    let ellipsis = "…";
    let ell_w = text.measure_width(ellipsis, size_px);
    if ell_w > max_w {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0.0;
    for ch in s.chars() {
        let ch_w = text.measure_width(&ch.to_string(), size_px);
        if used + ch_w + ell_w > max_w {
            break;
        }
        out.push(ch);
        used += ch_w;
    }
    out.push_str(ellipsis);
    out
}
