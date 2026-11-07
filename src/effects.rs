//! What a clip can have done to its picture on the way to the canvas.
//!
//! Two halves. The model — [`Effect`], its parameters, and the
//! [`EffectStack`] a clip carries — is what the project file stores and the
//! effects panel edits. The arithmetic — [`BlurPlan`] and the plane functions
//! under it — is what the preview's shader and the export's CPU path both
//! follow, so a blur is computed the same way in both: halve the picture until
//! the blur is small, blur that, and let the normal placement scale it back up.
//! Neither renderer decides on its own how a blur is done; they only choose
//! where to run it.
//!
//! Everything here is pure arithmetic over plain values, so it tests without a
//! GPU or a decoder.

use serde::{Deserialize, Serialize};

/// How many effects one clip can carry. A fixed cap is what lets a clip stay
/// `Copy`, which the undo snapshots and every edit that copies a clip lean on;
/// eight is several more than anyone stacks on one shot.
pub const MAX_EFFECTS: usize = 8;

/// The effects there are, in the order the library lists them.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectKind {
    GaussianBlur,
}

/// How a parameter's stored value reads in the panel.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Unit {
    /// Stored as a fraction of the canvas height, shown as pixels of the
    /// current canvas. A fraction for the reason clip placement is one: the
    /// same project re-mastered from 1080p to 4K has to look the same, not
    /// half as blurred.
    CanvasHeight,
    /// Stored 0 to 1, shown as a percentage.
    Percent,
}

/// One adjustable number on an effect: what it is called, its range, where a
/// fresh effect starts it, and how it reads.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ParamSpec {
    pub name: &'static str,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub unit: Unit,
}

impl ParamSpec {
    /// The value as the panel shows it on a canvas `canvas_h` pixels tall.
    pub fn format(&self, value: f32, canvas_h: u32) -> String {
        match self.unit {
            Unit::CanvasHeight => format!("{} px", (value * canvas_h as f32).round()),
            Unit::Percent => format!("{}%", (value * 100.0).round()),
        }
    }

    /// Where `value` sits along the slider, 0 to 1.
    pub fn fraction(&self, value: f32) -> f32 {
        let span = self.max - self.min;
        if span <= 0.0 {
            return 0.0;
        }
        ((value - self.min) / span).clamp(0.0, 1.0)
    }

    /// The value at `fraction` along the slider.
    pub fn at(&self, fraction: f32) -> f32 {
        self.min + (self.max - self.min) * fraction.clamp(0.0, 1.0)
    }
}

/// The blur's radius. The top is about a twelfth of the frame — far enough to
/// melt footage into a backdrop, which is the heaviest blur an edit asks for.
const BLUR_RADIUS: ParamSpec = ParamSpec {
    name: "Radius",
    min: 0.0,
    max: 0.08,
    default: 0.015,
    unit: Unit::CanvasHeight,
};

impl EffectKind {
    pub const ALL: [EffectKind; 1] = [EffectKind::GaussianBlur];

    pub fn name(self) -> &'static str {
        match self {
            EffectKind::GaussianBlur => "Gaussian Blur",
        }
    }

    /// One line for the library row, saying what it is for rather than what
    /// it is called.
    pub fn blurb(self) -> &'static str {
        match self {
            EffectKind::GaussianBlur => "Soften the picture",
        }
    }

    pub fn params(self) -> &'static [ParamSpec] {
        match self {
            EffectKind::GaussianBlur => std::slice::from_ref(&BLUR_RADIUS),
        }
    }

    /// The effect as it lands on a clip: switched on, every parameter at its
    /// default.
    pub fn instance(self) -> Effect {
        let params = match self {
            EffectKind::GaussianBlur => Params::GaussianBlur { radius: BLUR_RADIUS.default },
        };
        Effect { enabled: true, params }
    }
}

/// An effect's settings. One variant per kind, with named fields, so a project
/// file reads `GaussianBlur(radius: 0.015)` rather than an anonymous list of
/// numbers that only means something next to this source.
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Params {
    GaussianBlur { radius: f32 },
}

/// One effect on one clip.
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Effect {
    /// Off keeps the settings but skips the effect — for comparing with and
    /// without, which is most of what tuning an effect is.
    pub enabled: bool,
    pub params: Params,
}

impl Effect {
    pub fn kind(&self) -> EffectKind {
        match self.params {
            Params::GaussianBlur { .. } => EffectKind::GaussianBlur,
        }
    }

    /// Parameter `i`, in the order [`EffectKind::params`] lists them.
    pub fn param(&self, i: usize) -> f32 {
        match (self.params, i) {
            (Params::GaussianBlur { radius }, 0) => radius,
            _ => 0.0,
        }
    }

    /// Set parameter `i`, clamped to its range. An index the kind does not
    /// have is ignored.
    pub fn set_param(&mut self, i: usize, value: f32) {
        let Some(spec) = self.kind().params().get(i) else {
            return;
        };
        let value = if value.is_finite() { value.clamp(spec.min, spec.max) } else { spec.default };
        match (&mut self.params, i) {
            (Params::GaussianBlur { radius }, 0) => *radius = value,
            _ => {}
        }
    }
}

/// A clip's effects, applied in order.
///
/// A fixed-size array rather than a `Vec` so [`crate::timeline::Clip`] stays
/// `Copy` — see [`MAX_EFFECTS`]. Kept packed: the effects are always the
/// leading `Some`s. On disk it is a plain list.
#[derive(Copy, Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(from = "Vec<Effect>", into = "Vec<Effect>")]
pub struct EffectStack {
    items: [Option<Effect>; MAX_EFFECTS],
}

impl From<Vec<Effect>> for EffectStack {
    /// A hand-edited file with more than fits keeps the first
    /// [`MAX_EFFECTS`]; the rest are dropped rather than refusing the file.
    fn from(list: Vec<Effect>) -> Self {
        let mut stack = Self::default();
        for effect in list {
            stack.push(effect);
        }
        stack
    }
}

impl From<EffectStack> for Vec<Effect> {
    fn from(stack: EffectStack) -> Self {
        stack.iter().copied().collect()
    }
}

impl EffectStack {
    pub fn iter(&self) -> impl Iterator<Item = &Effect> {
        self.items.iter().map_while(Option::as_ref)
    }

    pub fn len(&self) -> usize {
        self.iter().count()
    }

    pub fn is_empty(&self) -> bool {
        self.items[0].is_none()
    }

    pub fn is_full(&self) -> bool {
        self.items[MAX_EFFECTS - 1].is_some()
    }

    pub fn get(&self, i: usize) -> Option<&Effect> {
        self.items.get(i)?.as_ref()
    }

    pub fn get_mut(&mut self, i: usize) -> Option<&mut Effect> {
        self.items.get_mut(i)?.as_mut()
    }

    /// Add `effect` at the end. False when the stack is full.
    pub fn push(&mut self, effect: Effect) -> bool {
        let n = self.len();
        if n == MAX_EFFECTS {
            return false;
        }
        self.items[n] = Some(effect);
        true
    }

    /// Take out effect `i`, closing the gap behind it.
    pub fn remove(&mut self, i: usize) {
        let n = self.len();
        if i >= n {
            return;
        }
        self.items.copy_within(i + 1..n, i);
        self.items[n - 1] = None;
    }

    /// Every switched-on blur, as one Gaussian's standard deviation in pixels
    /// of a canvas `canvas_h` tall. Zero when nothing blurs.
    ///
    /// Blurs in series compose exactly into one whose variance is the sum of
    /// theirs, so a stack of them is one pass rather than several.
    pub fn blur_sigma(&self, canvas_h: u32) -> f32 {
        self.iter()
            .filter(|e| e.enabled)
            .map(|e| match e.params {
                Params::GaussianBlur { radius } => (radius * canvas_h as f32).powi(2),
            })
            .sum::<f32>()
            .sqrt()
    }
}

/// Below this a blur moves no sample by as much as a level, so it is skipped.
pub const MIN_SIGMA: f32 = 0.3;
/// The widest blur run at full resolution. Past it the picture is halved and
/// the blur with it — a blur that wide has nothing left at the finer scale to
/// lose — which keeps the kernel short however strong the blur is.
pub const MAX_WORK_SIGMA: f32 = 3.0;
/// Enough halvings to take any source this editor opens down to a few pixels.
const MAX_HALVINGS: u32 = 12;

/// How one blur is carried out on a picture: halve it `halvings` times, then
/// run a Gaussian of `sigma` pixels over what is left. Both renderers follow
/// this, so the preview and the file agree on more than the idea of a blur.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct BlurPlan {
    pub halvings: u32,
    pub sigma: f32,
}

impl BlurPlan {
    /// The plan for a blur of `sigma` pixels of the picture it runs on, or
    /// `None` for one too slight to see.
    pub fn for_sigma(sigma: f32) -> Option<Self> {
        if !(sigma >= MIN_SIGMA) || !sigma.is_finite() {
            return None;
        }
        let mut plan = Self { halvings: 0, sigma };
        while plan.sigma > MAX_WORK_SIGMA && plan.halvings < MAX_HALVINGS {
            plan.sigma /= 2.0;
            plan.halvings += 1;
        }
        Some(plan)
    }

    /// How far each side of a sample the kernel reaches. Three deviations
    /// holds all but a fraction of a percent of the weight.
    pub fn radius(&self) -> i32 {
        (self.sigma * 3.0).ceil() as i32
    }

    /// The size a `w x h` picture is blurred at.
    pub fn work_size(&self, w: u32, h: u32) -> (u32, u32) {
        (0..self.halvings).fold((w, h), |(w, h), _| (halve(w), halve(h)))
    }
}

/// One dimension after a halving: odd sizes round up, the last sample pairing
/// with itself.
pub fn halve(n: u32) -> u32 {
    n.div_ceil(2).max(1)
}

/// The kernel for `plan`, normalised to sum to one, centre tap in the middle.
/// The shader computes the same weights from the same formula.
pub fn kernel(plan: BlurPlan) -> Vec<f32> {
    let r = plan.radius();
    let two_s2 = 2.0 * plan.sigma * plan.sigma;
    let raw: Vec<f32> = (-r..=r).map(|i| (-((i * i) as f32) / two_s2).exp()).collect();
    let sum: f32 = raw.iter().sum();
    raw.into_iter().map(|w| w / sum).collect()
}

/// One channel of a picture as floats, row-major.
#[derive(Clone, Debug, PartialEq)]
pub struct Plane {
    pub w: usize,
    pub h: usize,
    pub data: Vec<f32>,
}

impl Plane {
    pub fn new(w: usize, h: usize) -> Self {
        Self { w, h, data: vec![0.0; w * h] }
    }

    fn at(&self, x: usize, y: usize) -> f32 {
        self.data[y * self.w + x]
    }
}

/// Half the size each way, each sample the mean of the 2x2 block it covers.
/// An odd last row or column averages with itself, as a clamped read would.
pub fn halve_plane(p: &Plane) -> Plane {
    let (w, h) = (halve(p.w as u32) as usize, halve(p.h as u32) as usize);
    let mut out = Plane::new(w, h);
    for y in 0..h {
        let (y0, y1) = ((2 * y).min(p.h - 1), (2 * y + 1).min(p.h - 1));
        for x in 0..w {
            let (x0, x1) = ((2 * x).min(p.w - 1), (2 * x + 1).min(p.w - 1));
            out.data[y * w + x] = (p.at(x0, y0) + p.at(x1, y0) + p.at(x0, y1) + p.at(x1, y1)) * 0.25;
        }
    }
    out
}

/// A separable Gaussian with the edges clamped: past the border the last
/// sample repeats, so a blurred picture keeps its own colour to the edge
/// rather than fading into black or into nothing.
pub fn gaussian(p: &Plane, plan: BlurPlan) -> Plane {
    let k = kernel(plan);
    let r = plan.radius() as isize;
    let (w, h) = (p.w, p.h);
    let mut across = Plane::new(w, h);
    for y in 0..h {
        let row = &p.data[y * w..(y + 1) * w];
        for x in 0..w {
            let mut acc = 0.0;
            for (j, wt) in k.iter().enumerate() {
                let sx = (x as isize + j as isize - r).clamp(0, w as isize - 1) as usize;
                acc += row[sx] * wt;
            }
            across.data[y * w + x] = acc;
        }
    }
    let mut out = Plane::new(w, h);
    for (j, wt) in k.iter().enumerate() {
        for y in 0..h {
            let sy = (y as isize + j as isize - r).clamp(0, h as isize - 1) as usize;
            let src = &across.data[sy * w..(sy + 1) * w];
            let dst = &mut out.data[y * w..(y + 1) * w];
            for (d, s) in dst.iter_mut().zip(src) {
                *d += s * wt;
            }
        }
    }
    out
}

/// The whole plan on one plane: halve, then blur. The result is at
/// [`BlurPlan::work_size`].
pub fn blur_plane(p: &Plane, plan: BlurPlan) -> Plane {
    let mut cur = p.clone();
    for _ in 0..plan.halvings {
        cur = halve_plane(&cur);
    }
    gaussian(&cur, plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stack_keeps_its_order_and_closes_gaps() {
        let mut stack = EffectStack::default();
        assert!(stack.is_empty());
        for r in [0.01, 0.02, 0.03] {
            let mut e = EffectKind::GaussianBlur.instance();
            e.set_param(0, r);
            assert!(stack.push(e));
        }
        stack.remove(1);
        let radii: Vec<f32> = stack.iter().map(|e| e.param(0)).collect();
        assert_eq!(radii, [0.01, 0.03]);
        assert_eq!(stack.len(), 2);
    }

    #[test]
    fn a_full_stack_refuses_another() {
        let mut stack = EffectStack::default();
        for _ in 0..MAX_EFFECTS {
            assert!(stack.push(EffectKind::GaussianBlur.instance()));
        }
        assert!(stack.is_full());
        assert!(!stack.push(EffectKind::GaussianBlur.instance()));
    }

    #[test]
    fn a_stack_round_trips_as_a_plain_list() {
        let mut stack = EffectStack::default();
        stack.push(EffectKind::GaussianBlur.instance());
        let text = ron::to_string(&stack).unwrap();
        assert!(text.starts_with('['), "{text}");
        let back: EffectStack = ron::from_str(&text).unwrap();
        assert_eq!(back, stack);
    }

    #[test]
    fn parameters_stay_in_range() {
        let mut e = EffectKind::GaussianBlur.instance();
        e.set_param(0, 5.0);
        assert_eq!(e.param(0), BLUR_RADIUS.max);
        e.set_param(0, f32::NAN);
        assert_eq!(e.param(0), BLUR_RADIUS.default);
    }

    /// Two blurs in a row are one blur with their variances added; a switched
    /// off one adds nothing.
    #[test]
    fn stacked_blurs_combine_in_quadrature() {
        let mut stack = EffectStack::default();
        let mut a = EffectKind::GaussianBlur.instance();
        a.set_param(0, 0.03);
        let mut b = a;
        b.set_param(0, 0.04);
        let mut off = a;
        off.enabled = false;
        stack.push(a);
        stack.push(b);
        stack.push(off);
        assert!((stack.blur_sigma(100) - 5.0).abs() < 1e-4);
    }

    #[test]
    fn a_wide_blur_runs_on_a_smaller_picture() {
        assert_eq!(BlurPlan::for_sigma(0.1), None);
        assert_eq!(BlurPlan::for_sigma(2.0), Some(BlurPlan { halvings: 0, sigma: 2.0 }));
        let plan = BlurPlan::for_sigma(40.0).unwrap();
        assert_eq!(plan.halvings, 4);
        assert!(plan.sigma <= MAX_WORK_SIGMA);
        assert_eq!(plan.work_size(1920, 1081), (120, 68));
    }

    #[test]
    fn the_kernel_sums_to_one_and_is_symmetric() {
        let k = kernel(BlurPlan { halvings: 0, sigma: 2.5 });
        assert!((k.iter().sum::<f32>() - 1.0).abs() < 1e-5);
        for i in 0..k.len() / 2 {
            assert_eq!(k[i], k[k.len() - 1 - i]);
        }
    }

    /// Clamped edges mean a flat picture stays flat all the way out, which is
    /// what keeps a blurred backdrop from growing a dark rim.
    #[test]
    fn a_flat_picture_blurs_to_itself() {
        let mut p = Plane::new(37, 21);
        p.data.fill(0.7);
        let out = blur_plane(&p, BlurPlan::for_sigma(9.0).unwrap());
        assert!(out.data.iter().all(|v| (v - 0.7).abs() < 1e-5));
    }

    #[test]
    fn a_hard_edge_comes_out_soft() {
        let mut p = Plane::new(40, 4);
        for y in 0..4 {
            for x in 20..40 {
                p.data[y * 40 + x] = 1.0;
            }
        }
        let out = gaussian(&p, BlurPlan { halvings: 0, sigma: 2.0 });
        let row = &out.data[0..40];
        assert!(row[19] > 0.2 && row[19] < 0.5, "{}", row[19]);
        assert!(row[20] > 0.5 && row[20] < 0.8, "{}", row[20]);
        assert!(row[0] < 1e-6 && (row[39] - 1.0).abs() < 1e-6);
        // Monotonic across the edge: a Gaussian has no ringing.
        assert!(row.windows(2).all(|w| w[1] >= w[0] - 1e-6));
    }

    #[test]
    fn halving_averages_blocks_and_keeps_an_odd_edge() {
        let p = Plane { w: 3, h: 2, data: vec![0.0, 1.0, 2.0, 4.0, 5.0, 6.0] };
        let out = halve_plane(&p);
        assert_eq!((out.w, out.h), (2, 1));
        assert_eq!(out.data, vec![2.5, 4.0]);
    }
}
