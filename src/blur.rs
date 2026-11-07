//! The preview's half of a blur: the passes that turn a clip's picture into a
//! blurred texture the quad renderer can draw in its place.
//!
//! The steps are [`crate::effects::BlurPlan`]'s — halve, then a Gaussian across
//! and a Gaussian down — and the arithmetic is the CPU path's, ported to
//! `blur.wgsl`. Each blurred clip keeps its own chain of targets between
//! frames, keyed by clip id rather than source, because two clips of one
//! source can be blurred differently: the sharp shot over its own blurred
//! copy is the common case, not the odd one.
//!
//! Drawing is split in two because the passes must run before the frame's
//! main pass reads their output. [`Blur::prepare`] is called while the preview
//! is being laid out and hands back the texture to draw; [`Blur::encode`]
//! records the passes once the frame is known, ahead of the pass that draws
//! it.

use std::collections::HashMap;
use std::num::NonZeroU64;

use crate::effects::{halve, BlurPlan};
use crate::quad::{QuadRenderer, Texture};

/// Every intermediate, and the result. Float so the averaging leaves no bands
/// in a smooth gradient, which is most of what a heavy blur is made of.
const WORK_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
/// Where each pass's parameters sit in an entry's uniform buffer. The spec's
/// guaranteed minimum offset alignment, so any adapter accepts it.
const UNIFORM_STRIDE: u64 = 256;

#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct PassParams {
    dir: [i32; 2],
    radius: i32,
    mode: u32,
    sigma: f32,
    in_display: u32,
    out_display: u32,
    _pad: u32,
}

const PARAMS_SIZE: u64 = std::mem::size_of::<PassParams>() as u64;

/// One clip's chain: a target per halving, one for the pass across, and the
/// result the preview samples.
struct Entry {
    source_size: (u32, u32),
    plan: BlurPlan,
    halves: Vec<wgpu::Texture>,
    across: wgpu::Texture,
    out: Texture,
    uniforms: wgpu::Buffer,
    /// What this frame blurs. Set by `prepare`, consumed by `encode`; an entry
    /// that goes a frame without one has left the canvas and is dropped.
    source: Option<wgpu::TextureView>,
}

pub struct Blur {
    pipeline: wgpu::RenderPipeline,
    bgl: wgpu::BindGroupLayout,
    entries: HashMap<u32, Entry>,
}

impl Blur {
    pub fn new(device: &wgpu::Device) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("blur.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("blur.wgsl").into()),
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("blur bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        // Read with textureLoad, never sampled, so no filter.
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: NonZeroU64::new(PARAMS_SIZE),
                    },
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("blur pl"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("blur pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: WORK_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        Self { pipeline, bgl, entries: HashMap::new() }
    }

    /// The texture clip `key`'s picture will be blurred into this frame, for
    /// drawing in place of `source`. Its contents are only written by the
    /// next [`Blur::encode`], which has to run before the pass that draws it.
    ///
    /// It is smaller than `source` once the plan halves; the quad that draws
    /// it stretches it back over the same rect, which is the scale-up the
    /// export does with its scaler.
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        quads: &QuadRenderer,
        key: u32,
        source: &Texture,
        plan: BlurPlan,
    ) -> &Texture {
        let size = (source.width, source.height);
        let fits = self
            .entries
            .get(&key)
            .is_some_and(|e| e.source_size == size && e.plan.halvings == plan.halvings);
        if !fits {
            self.entries.insert(key, Entry::new(device, quads, size, plan));
        }
        let entry = self.entries.get_mut(&key).expect("just inserted");
        entry.plan = plan;
        entry.source = Some(source.view());
        &entry.out
    }

    /// Record every pass `prepare` asked for this frame, and let go of the
    /// chains of clips that were not on the canvas.
    pub fn encode(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
    ) {
        self.entries.retain(|_, e| e.source.is_some());
        for entry in self.entries.values_mut() {
            let Some(source) = entry.source.take() else {
                continue;
            };
            let plan = entry.plan;
            let mut input = source;
            let mut from_display = 1;
            let mut slot = 0u64;
            let mut pass = |input: &wgpu::TextureView,
                            target: &wgpu::TextureView,
                            params: PassParams| {
                let offset = slot * UNIFORM_STRIDE;
                slot += 1;
                queue.write_buffer(&entry.uniforms, offset, bytemuck::bytes_of(&params));
                let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("blur bg"),
                    layout: &self.bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(input),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                buffer: &entry.uniforms,
                                offset,
                                size: NonZeroU64::new(PARAMS_SIZE),
                            }),
                        },
                    ],
                });
                let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("blur pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                rp.set_pipeline(&self.pipeline);
                rp.set_bind_group(0, &bind, &[]);
                rp.draw(0..3, 0..1);
            };
            let base = PassParams {
                dir: [0, 0],
                radius: plan.radius(),
                mode: 0,
                sigma: plan.sigma,
                in_display: 0,
                out_display: 0,
                _pad: 0,
            };
            for half in &entry.halves {
                let view = half.create_view(&wgpu::TextureViewDescriptor::default());
                pass(&input, &view, PassParams { in_display: from_display, ..base });
                input = view;
                from_display = 0;
            }
            let across = entry.across.create_view(&wgpu::TextureViewDescriptor::default());
            pass(
                &input,
                &across,
                PassParams { dir: [1, 0], mode: 1, in_display: from_display, ..base },
            );
            pass(
                &across,
                &entry.out.view(),
                PassParams { dir: [0, 1], mode: 1, out_display: 1, ..base },
            );
        }
    }
}

impl Entry {
    fn new(device: &wgpu::Device, quads: &QuadRenderer, size: (u32, u32), plan: BlurPlan) -> Self {
        let target = |(w, h): (u32, u32)| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some("blur target"),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: WORK_FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            })
        };
        let mut halves = Vec::new();
        let mut at = size;
        for _ in 0..plan.halvings {
            at = (halve(at.0), halve(at.1));
            halves.push(target(at));
        }
        let passes = plan.halvings as u64 + 2;
        Self {
            source_size: size,
            plan,
            halves,
            across: target(at),
            out: quads.create_target_texture(device, at.0, at.1, WORK_FORMAT),
            uniforms: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("blur uniforms"),
                size: passes * UNIFORM_STRIDE,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            source: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::{blur_plane, Plane};

    /// A device with no window, or `None` on a machine without a usable GPU,
    /// where the test has nothing to say.
    fn gpu() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .ok()?;
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()
    }

    fn f16_to_f32(h: u16) -> f32 {
        let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
        let exp = ((h >> 10) & 0x1f) as i32;
        let frac = (h & 0x3ff) as f32;
        sign * match exp {
            0 => frac * 2f32.powi(-24),
            31 => f32::INFINITY,
            _ => (1.0 + frac / 1024.0) * 2f32.powi(exp - 15),
        }
    }

    fn linear_to_srgb(c: f32) -> f32 {
        if c <= 0.0031308 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
    }

    /// The preview's shader against the export's arithmetic, on the same
    /// picture: the claim the whole design rests on, that what you see blurred
    /// on screen is what the file gets.
    #[test]
    fn the_shader_blurs_as_the_export_does() {
        let Some((device, queue)) = gpu() else {
            return;
        };
        let quads = QuadRenderer::new(&device, &queue, wgpu::TextureFormat::Rgba8UnormSrgb);
        let (w, h) = (48u32, 14u32);
        // Bands of grey, a hard step, and a ramp: something for every kind
        // of mistake to show up in.
        let level = |x: u32| -> u8 {
            match x {
                0..12 => 30,
                12..24 => 220,
                _ => (60 + (x - 24) * 7) as u8,
            }
        };
        let mut rgba = Vec::new();
        for _ in 0..h {
            for x in 0..w {
                let v = level(x);
                rgba.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let source = quads.create_empty_texture(&device, w, h, wgpu::TextureFormat::Rgba8UnormSrgb);
        source.write_region(&queue, 0, 0, w, h, &rgba);

        let plan = BlurPlan::for_sigma(5.0).unwrap();
        assert_eq!(plan.halvings, 1, "the test means to cover a halving");
        let mut blur = Blur::new(&device);
        let (ow, oh) = {
            let out = blur.prepare(&device, &quads, 0, &source, plan);
            (out.width, out.height)
        };
        assert_eq!((ow, oh), plan.work_size(w, h));
        let mut encoder = device.create_command_encoder(&Default::default());
        blur.encode(&device, &queue, &mut encoder);

        let row_bytes = (ow * 8).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (row_bytes * oh) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let out = &blur.entries[&0].out;
        encoder.copy_texture_to_buffer(
            out.texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row_bytes),
                    rows_per_image: Some(oh),
                },
            },
            wgpu::Extent3d { width: ow, height: oh, depth_or_array_layers: 1 },
        );
        queue.submit([encoder.finish()]);
        readback.slice(..).map_async(wgpu::MapMode::Read, |r| r.unwrap());
        device
            .poll(wgpu::PollType::Wait { submission_index: None, timeout: None })
            .unwrap();
        let bytes = readback.slice(..).get_mapped_range().to_vec();

        let mut plane = Plane::new(w as usize, h as usize);
        for y in 0..h as usize {
            for x in 0..w as usize {
                plane.data[y * w as usize + x] = level(x as u32) as f32;
            }
        }
        let want = blur_plane(&plane, plan);
        for y in 0..oh as usize {
            for x in 0..ow as usize {
                let at = y * row_bytes as usize + x * 8;
                let red = f16_to_f32(u16::from_le_bytes([bytes[at], bytes[at + 1]]));
                let alpha = f16_to_f32(u16::from_le_bytes([bytes[at + 6], bytes[at + 7]]));
                let got = linear_to_srgb(red) * 255.0;
                let expect = want.data[y * ow as usize + x];
                assert!((got - expect).abs() < 1.5, "({x},{y}): shader {got}, export {expect}");
                assert!((alpha - 1.0).abs() < 1e-3, "({x},{y}): alpha {alpha}");
            }
        }
    }
}
