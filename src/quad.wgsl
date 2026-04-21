struct Uniforms {
    screen_size: vec2<f32>,
}

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

struct Instance {
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) uv: vec4<f32>, // u0, v0, u1, v1
}

struct VsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) uv: vec2<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, inst: Instance) -> VsOut {
    var corner = vec2<f32>(0.0, 0.0);
    var uv = vec2<f32>(inst.uv.x, inst.uv.y);
    if (vi == 1u) {
        corner = vec2<f32>(1.0, 0.0);
        uv = vec2<f32>(inst.uv.z, inst.uv.y);
    } else if (vi == 2u) {
        corner = vec2<f32>(0.0, 1.0);
        uv = vec2<f32>(inst.uv.x, inst.uv.w);
    } else if (vi == 3u) {
        corner = vec2<f32>(1.0, 1.0);
        uv = vec2<f32>(inst.uv.z, inst.uv.w);
    }

    let screen_pos = inst.pos + corner * inst.size;
    let ndc = vec2<f32>(
        screen_pos.x / u.screen_size.x * 2.0 - 1.0,
        1.0 - screen_pos.y / u.screen_size.y * 2.0,
    );

    var out: VsOut;
    out.clip_pos = vec4<f32>(ndc, 0.0, 1.0);
    out.color = inst.color;
    out.uv = uv;
    return out;
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let cutoff = step(c, vec3<f32>(0.04045));
    let lower = c / 12.92;
    let higher = pow((c + 0.055) / 1.055, vec3<f32>(2.4));
    return mix(higher, lower, cutoff);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Texture is sampled in its native space (sRGB textures auto-convert to linear
    // via the texture format). Input color is sRGB convention, convert then multiply.
    let tex_sample = textureSample(tex, samp, in.uv);
    let rgb = srgb_to_linear(in.color.rgb) * tex_sample.rgb;
    return vec4<f32>(rgb, in.color.a * tex_sample.a);
}
