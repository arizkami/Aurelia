//! What a shadow actually looks like, measured on a real GPU.
//!
//! Every other test in this crate checks that a shader *compiles*. This one
//! renders one and reads the pixels back, because the defect it exists to catch
//! compiles perfectly: a box shadow whose falloff is the wrong shape is a
//! smudge with a hard inner edge, and nothing short of looking at the output
//! tells you so.
//!
//! The reference is the closed form. A Gaussian-blurred half-plane is exactly
//! the normal CDF of the distance to its edge, so far from the corners of a box
//! that is large compared to sigma, coverage along the vertical through the
//! middle must be `Φ(-d / σ)` for a point `d` outside the edge — one half on
//! the edge itself, and 0.16, 0.023, 0.001 at one, two and three sigma out.
//! Those four numbers are the test.
//!
//! Skips itself where there is no adapter, the same as the pipeline suite: a
//! machine with no GPU is an environment fact, not a defect.

use spherekit_render::{GpuClip, GpuTransform, QuadInstance, quad_flags};
use spherekit_wgpu::pipeline::{PipelineCache, PipelineKey, PipelineKind};

/// Side of the square target the shadow is rendered into.
const SIZE: u32 = 256;
/// The caster's half-extent, comfortably larger than sigma in both axes.
const HALF: f32 = 60.0;
/// The blur's standard deviation.
const SIGMA: f32 = 12.0;

/// Frame uniforms, mirroring the private `PassUniforms` in the renderer.
///
/// Duplicated rather than exported: the layout is a contract with
/// `common/frame.wgsl`, and a test that reproduces it from the shader's own
/// declaration is a second reader of that contract rather than a user of
/// whatever the renderer happens to do.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct PassUniforms {
    viewport: [f32; 2],
    scale_factor: f32,
    time: f32,
    target_origin: [f32; 2],
    _pad: [f32; 2],
}

fn headless() -> Option<(wgpu::Device, wgpu::Queue, String)> {
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
        apply_limit_buckets: false,
    }))
    .ok()?;
    let info = adapter.get_info();
    let name = format!("{} ({:?})", info.name, info.backend);

    let adapter_limits = adapter.limits();
    let mut required = wgpu::Limits::downlevel_defaults().using_resolution(adapter_limits.clone());
    required.max_texture_dimension_2d = adapter_limits.max_texture_dimension_2d.min(8192);
    required.max_storage_buffers_per_shader_stage =
        adapter_limits.max_storage_buffers_per_shader_stage.clamp(3, 4);
    required.max_storage_buffer_binding_size = adapter_limits.max_storage_buffer_binding_size;
    required.max_buffer_size = adapter_limits.max_buffer_size;

    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("spherekit.shadow_test"),
        required_features: wgpu::Features::empty(),
        required_limits: required,
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        memory_hints: wgpu::MemoryHints::default(),
        trace: wgpu::Trace::Off,
    }))
    .ok()?;
    Some((device, queue, name))
}

/// Renders one quad instance into a linear target and reads it back.
///
/// The target is `Rgba8Unorm`, **not** `Rgba8UnormSrgb`: the shader writes
/// linear premultiplied colour, and an sRGB target would encode it on the way
/// out and make every readback a gamma puzzle. Here the byte is the coverage.
fn render_instance(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    instance: QuadInstance,
) -> Vec<[u8; 4]> {
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let mut cache = PipelineCache::new(device);
    // Cloned out of the cache so the layouts stay reachable: `get` borrows the
    // cache mutably, and the bind groups below need it immutably.
    let pipeline = cache
        .get(device, PipelineKey { kind: PipelineKind::Quad, format, samples: 1 })
        .expect("quad pipeline")
        .clone();

    let uniforms = PassUniforms {
        viewport: [SIZE as f32, SIZE as f32],
        scale_factor: 1.0,
        time: 0.0,
        target_origin: [0.0, 0.0],
        _pad: [0.0, 0.0],
    };
    let buffer = |contents: &[u8], usage| {
        use wgpu::util::DeviceExt;
        device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: None,
            contents,
            usage,
        })
    };
    let pass_buf = buffer(bytemuck::bytes_of(&uniforms), wgpu::BufferUsages::UNIFORM);
    let transform_buf =
        buffer(bytemuck::bytes_of(&GpuTransform::IDENTITY), wgpu::BufferUsages::STORAGE);
    // One clip with no radii, which `clip_coverage` reads as "no clipping".
    let clip_buf = buffer(bytemuck::bytes_of(&GpuClip::default()), wgpu::BufferUsages::STORAGE);
    let gradient_buf = buffer(
        bytemuck::bytes_of(&spherekit_render::GpuGradient::default()),
        wgpu::BufferUsages::STORAGE,
    );
    let quad_buf = buffer(bytemuck::bytes_of(&instance), wgpu::BufferUsages::VERTEX);

    let frame_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("frame"),
        layout: &cache.layouts().frame,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: pass_buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: transform_buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 2, resource: clip_buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 3, resource: gradient_buf.as_entire_binding() },
        ],
    });

    // The quad pipeline always binds a texture group, even for a shadow that
    // never samples it.
    let dummy = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("dummy"),
        size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor::default());
    let texture_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("texture"),
        layout: &cache.layouts().texture,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(
                    &dummy.create_view(&wgpu::TextureViewDescriptor::default()),
                ),
            },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
        ],
    });

    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("target"),
        size: wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());

    // 256 pixels of RGBA8 is exactly the 256-byte row alignment a texture copy
    // requires, which is why the target is that size.
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (SIZE * SIZE * 4) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("shadow"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
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
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &frame_bind, &[0]);
        pass.set_bind_group(1, &texture_bind, &[]);
        pass.set_vertex_buffer(0, quad_buf.slice(..));
        pass.draw(0..4, 0..1);
    }
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(SIZE * 4),
                rows_per_image: Some(SIZE),
            },
        },
        wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
    );
    queue.submit([encoder.finish()]);

    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = slice.get_mapped_range().expect("mapped range");
    let pixels = data.chunks_exact(4).map(|c| [c[0], c[1], c[2], c[3]]).collect();
    drop(data);
    readback.unmap();
    pixels
}

/// A white shadow instance centred in the target, so the red channel reads back
/// as coverage directly.
fn shadow_instance(radii: [f32; 4], sigma: f32) -> QuadInstance {
    let centre = SIZE as f32 * 0.5;
    QuadInstance {
        bounds: [centre - HALF, centre - HALF, HALF * 2.0, HALF * 2.0],
        radii,
        color: [1.0, 1.0, 1.0, 1.0],
        border_color: [0.0; 4],
        border_width: 0.0,
        blur_sigma: sigma,
        flags: quad_flags::SHADOW | quad_flags::FILL_SOLID,
        gradient: u32::MAX,
        transform_index: 0,
        clip_index: 0,
        _pad: [0; 2],
    }
}

/// The normal CDF, which is what a blurred edge's coverage profile is.
fn phi(z: f32) -> f32 {
    0.5 * (1.0 + libm_erf(z / core::f32::consts::SQRT_2))
}

/// `erf`, to well under one 8-bit step. Abramowitz and Stegun 7.1.26, the same
/// series the shader uses — which is fine here, because what is under test is
/// the *integral*, not the error function.
fn libm_erf(x: f32) -> f32 {
    let s = x.signum();
    let a = x.abs();
    let mut r = 1.0 + (0.278393 + (0.230389 + 0.078108 * a * a) * a) * a;
    r *= r;
    r *= r;
    s - s / r
}

/// Coverage down the vertical centre line of the target.
fn column(pixels: &[[u8; 4]], x: u32) -> Vec<f32> {
    (0..SIZE).map(|y| pixels[(y * SIZE + x) as usize][0] as f32 / 255.0).collect()
}

#[test]
fn a_box_shadow_falls_off_as_a_gaussian() {
    let Some((device, queue, adapter)) = headless() else {
        eprintln!("no GPU adapter available; skipping");
        return;
    };
    println!("adapter: {adapter}");

    let pixels = render_instance(&device, &queue, shadow_instance([0.0; 4], SIGMA));
    let centre = SIZE as f32 * 0.5;
    let col = column(&pixels, SIZE / 2);

    // The bottom edge of the caster, and the profile below it.
    let edge = centre + HALF;
    for sigmas in [0.0f32, 1.0, 2.0, 3.0] {
        let y = (edge + sigmas * SIGMA).round() as usize;
        if y >= SIZE as usize {
            continue;
        }
        let expected = phi(-sigmas);
        let actual = col[y];
        assert!(
            (actual - expected).abs() < 0.02,
            "at {sigmas}σ below the edge the shadow was {actual:.3}, not {expected:.3}"
        );
    }

    // And deep inside it is solid, because the whole kernel is over the box.
    assert!(
        col[centre as usize] > 0.99,
        "the middle of the shadow was {:.3}",
        col[centre as usize]
    );
}

#[test]
fn a_box_shadow_never_brightens_on_the_way_out() {
    // The defect that prompted this file showed up as a falloff that stalled
    // and then dropped, which is what a normalised-by-sample-weight sum does
    // when the sample window straddles the edge. Monotonicity is the cheapest
    // statement of "this is a Gaussian tail and not a staircase".
    let Some((device, queue, _)) = headless() else {
        eprintln!("no GPU adapter available; skipping");
        return;
    };
    let pixels = render_instance(&device, &queue, shadow_instance([0.0; 4], SIGMA));
    let col = column(&pixels, SIZE / 2);
    let edge = (SIZE as f32 * 0.5 + HALF) as usize;

    let mut previous = 1.0f32;
    for (y, v) in col.iter().copied().enumerate().skip(edge) {
        assert!(
            v <= previous + 0.004,
            "coverage rose from {previous:.3} to {v:.3} at y = {y}, {} px below the edge",
            y - edge
        );
        previous = v;
    }
}

#[test]
fn a_shadow_reaches_three_sigma_and_stops() {
    let Some((device, queue, _)) = headless() else {
        eprintln!("no GPU adapter available; skipping");
        return;
    };
    let pixels = render_instance(&device, &queue, shadow_instance([0.0; 4], SIGMA));
    let col = column(&pixels, SIZE / 2);
    let edge = (SIZE as f32 * 0.5 + HALF) as usize;

    // Something must still be visible at two sigma — this is what "the shadow
    // is missing" would look like — and nothing at four.
    let two = col[edge + (SIGMA * 2.0) as usize];
    assert!(two > 0.01, "two sigma out the shadow had already vanished ({two:.4})");
    let four = col[edge + (SIGMA * 4.0) as usize];
    assert!(four < 0.01, "four sigma out the shadow was still {four:.4}");
}

#[test]
fn a_radius_cuts_the_corner_off_the_shadow() {
    // The discriminator is exact. A Gaussian is separable, so at the corner of
    // a *sharp* box the coverage is the product of two half-planes — one half
    // times one half, exactly a quarter — while the middle of an edge is one
    // half. Round the corner off and the same point must fall well below a
    // quarter, because the shape has retreated from it.
    let Some((device, queue, _)) = headless() else {
        eprintln!("no GPU adapter available; skipping");
        return;
    };
    let centre = (SIZE as f32 * 0.5) as usize;
    let corner_px = centre + HALF as usize;
    let at =
        |pixels: &[[u8; 4]], x: usize, y: usize| pixels[y * SIZE as usize + x][0] as f32 / 255.0;

    let sharp = render_instance(&device, &queue, shadow_instance([0.0; 4], SIGMA));
    let sharp_corner = at(&sharp, corner_px, corner_px);
    let sharp_edge = at(&sharp, centre, corner_px);
    assert!(
        (sharp_corner - 0.25).abs() < 0.02,
        "a sharp corner should be a quarter covered, not {sharp_corner:.3}"
    );
    assert!(
        (sharp_edge - 0.5).abs() < 0.02,
        "the middle of an edge should be half covered, not {sharp_edge:.3}"
    );

    let rounded = render_instance(&device, &queue, shadow_instance([HALF * 0.8; 4], SIGMA));
    let rounded_corner = at(&rounded, corner_px, corner_px);
    let rounded_edge = at(&rounded, centre, corner_px);
    assert!(
        rounded_corner < 0.15,
        "the radii were ignored: the corner is {rounded_corner:.3}, near the sharp {sharp_corner:.3}"
    );
    // And the middle of the edge is untouched by a corner radius.
    assert!(
        (rounded_edge - sharp_edge).abs() < 0.03,
        "rounding the corners moved the middle of the edge: {rounded_edge:.3} vs {sharp_edge:.3}"
    );
}

#[test]
fn a_zero_blur_shadow_is_the_sharp_shape() {
    let Some((device, queue, _)) = headless() else {
        eprintln!("no GPU adapter available; skipping");
        return;
    };
    let pixels = render_instance(&device, &queue, shadow_instance([0.0; 4], 0.0));
    let col = column(&pixels, SIZE / 2);
    let edge = (SIZE as f32 * 0.5 + HALF) as usize;
    assert!(col[edge - 2] > 0.99, "inside a sharp shadow was {:.3}", col[edge - 2]);
    assert!(col[edge + 2] < 0.01, "outside a sharp shadow was {:.3}", col[edge + 2]);
}
