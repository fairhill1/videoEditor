// One step of the preview's blur: either a 2x2 halving or one direction of a
// separable Gaussian. `src/effects.rs` is the reference for both — the export
// runs the same steps on the CPU — so this file computes exactly what that one
// does, in the same space: gamma-encoded and premultiplied.

struct Params {
    dir: vec2<i32>,
    radius: i32,
    // 0 halves, 1 blurs along `dir`.
    mode: u32,
    sigma: f32,
    // The input is a picture as the preview stores it: an sRGB texture, read
    // back as linear light with straight alpha. Re-encode and premultiply on
    // the way in, so the blur averages what the export averages.
    in_display: u32,
    // The output is going to be drawn as a picture: undo the premultiply and
    // hand back linear light, which is what the quad shader expects a texture
    // sample to be.
    out_display: u32,
    _pad: u32,
}

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var<uniform> p: Params;

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    // One triangle that covers the whole target.
    let x = f32((vi << 1u) & 2u);
    let y = f32(vi & 2u);
    return vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
}

fn linear_to_srgb(c: vec3<f32>) -> vec3<f32> {
    let c0 = clamp(c, vec3<f32>(0.0), vec3<f32>(1.0));
    let lower = c0 * 12.92;
    let higher = 1.055 * pow(c0, vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(higher, lower, c0 <= vec3<f32>(0.0031308));
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let c0 = clamp(c, vec3<f32>(0.0), vec3<f32>(1.0));
    let lower = c0 / 12.92;
    let higher = pow((c0 + 0.055) / 1.055, vec3<f32>(2.4));
    return select(higher, lower, c0 <= vec3<f32>(0.04045));
}

// A texel with the edges clamped, as the CPU path reads them.
fn fetch(q: vec2<i32>) -> vec4<f32> {
    let size = vec2<i32>(textureDimensions(src));
    let c = textureLoad(src, clamp(q, vec2<i32>(0), size - vec2<i32>(1)), 0);
    if (p.in_display == 1u) {
        return vec4<f32>(linear_to_srgb(c.rgb) * c.a, c.a);
    }
    return c;
}

@fragment
fn fs_main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let o = vec2<i32>(floor(pos.xy));
    var acc = vec4<f32>(0.0);
    if (p.mode == 0u) {
        let b = o * 2;
        acc = (fetch(b) + fetch(b + vec2<i32>(1, 0)) + fetch(b + vec2<i32>(0, 1))
            + fetch(b + vec2<i32>(1, 1))) * 0.25;
    } else {
        let two_s2 = 2.0 * p.sigma * p.sigma;
        var wsum = 0.0;
        for (var i = -p.radius; i <= p.radius; i = i + 1) {
            let w = exp(-f32(i * i) / two_s2);
            acc = acc + fetch(o + p.dir * i) * w;
            wsum = wsum + w;
        }
        acc = acc / wsum;
    }
    if (p.out_display == 1u) {
        var rgb = vec3<f32>(0.0);
        if (acc.a > 0.00001) {
            rgb = acc.rgb / acc.a;
        }
        return vec4<f32>(srgb_to_linear(rgb), acc.a);
    }
    return acc;
}
