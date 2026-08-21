//! Headless GPU validation.
//!
//! The unit tests in `sphere-wgpu` check layouts and policy without a device.
//! This suite does the thing they cannot: it creates a real adapter, compiles
//! every WGSL module through naga, and builds every pipeline. A shader that
//! fails validation is otherwise invisible until a window opens and shows
//! nothing.
//!
//! No surface is created, so this runs anywhere an adapter exists. On a machine
//! with no GPU at all the whole suite skips rather than failing — a missing
//! adapter is an environment fact, not a defect in the code under test.

use sphere_wgpu::pipeline::{PipelineCache, PipelineKey, PipelineKind};
use sphere_wgpu::shader;

const ALL_KINDS: &[PipelineKind] = &[
    PipelineKind::Quad,
    PipelineKind::Image,
    PipelineKind::Text,
    PipelineKind::TextSubpixel,
    PipelineKind::Mesh,
    PipelineKind::MeshTextured,
    PipelineKind::Composite,
];

/// Returns a headless device, or `None` when no adapter is available.
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

    let optional_features = adapter.features() & wgpu::Features::DUAL_SOURCE_BLENDING;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("sphere.test"),
        required_features: optional_features,
        required_limits: required,
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        memory_hints: wgpu::MemoryHints::default(),
        trace: wgpu::Trace::Off,
    }))
    .ok()?;
    Some((device, queue, name))
}

#[test]
fn every_shader_module_passes_naga_validation() {
    let Some((device, _queue, adapter)) = headless() else {
        eprintln!("no GPU adapter available; skipping");
        return;
    };
    println!("adapter: {adapter}");

    for (path, _) in shader::SOURCES {
        // `common/*.wgsl` are fragments meant to be included, not standalone
        // modules, but composing them must still succeed.
        let composed = shader::compose(path).unwrap_or_else(|e| panic!("{path}: {e}"));
        if path.starts_with("common/") {
            continue;
        }
        if *path == "text_subpixel.wgsl"
            && !device.features().contains(wgpu::Features::DUAL_SOURCE_BLENDING)
        {
            continue;
        }
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let _module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(path),
            source: wgpu::ShaderSource::Wgsl(composed.into()),
        });
        if let Some(err) = pollster::block_on(scope.pop()) {
            panic!("{path} failed WGSL validation:\n{err}");
        }
    }
}

#[test]
fn every_pipeline_builds_for_the_surface_and_layer_formats() {
    let Some((device, _queue, adapter)) = headless() else {
        eprintln!("no GPU adapter available; skipping");
        return;
    };
    println!("adapter: {adapter}");

    let mut cache = PipelineCache::new(&device);
    // Bgra8UnormSrgb is the usual swapchain format; Rgba8UnormSrgb is what
    // offscreen layers use. A pipeline is bound to one colour target format,
    // so both have to build.
    // Both sample counts must build: a machine with no MSAA support still has
    // to render, and one with it must not hit a pipeline-layout mismatch.
    for samples in [1u32, 4] {
        for format in [wgpu::TextureFormat::Bgra8UnormSrgb, wgpu::TextureFormat::Rgba8UnormSrgb] {
            for kind in ALL_KINDS {
                if *kind == PipelineKind::TextSubpixel
                    && !device.features().contains(wgpu::Features::DUAL_SOURCE_BLENDING)
                {
                    continue;
                }
                let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
                cache
                    .get(&device, PipelineKey { kind: *kind, format, samples })
                    .unwrap_or_else(|e| panic!("{kind:?} / {format:?}: {e}"));
                if let Some(err) = pollster::block_on(scope.pop()) {
                    panic!("{kind:?} for {format:?} failed validation:\n{err}");
                }
            }
        }
    }

    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    cache.blur(&device, wgpu::TextureFormat::Rgba8UnormSrgb).expect("blur pipeline");
    if let Some(err) = pollster::block_on(scope.pop()) {
        panic!("blur pipeline failed validation:\n{err}");
    }

    assert!(cache.built() >= ALL_KINDS.len() as u32 * 2, "expected pipelines to actually build");
}

#[test]
fn pipelines_are_cached_not_rebuilt() {
    let Some((device, _queue, _)) = headless() else {
        eprintln!("no GPU adapter available; skipping");
        return;
    };
    let mut cache = PipelineCache::new(&device);
    let key = PipelineKey {
        kind: PipelineKind::Quad,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        samples: 1,
    };
    cache.get(&device, key).unwrap();
    let after_first = cache.built();
    for _ in 0..10 {
        cache.get(&device, key).unwrap();
    }
    assert_eq!(cache.built(), after_first, "a cached pipeline was rebuilt");
}

#[test]
fn a_different_target_format_gets_its_own_pipeline() {
    let Some((device, _queue, _)) = headless() else {
        eprintln!("no GPU adapter available; skipping");
        return;
    };
    let mut cache = PipelineCache::new(&device);
    cache
        .get(
            &device,
            PipelineKey {
                kind: PipelineKind::Quad,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                samples: 1,
            },
        )
        .unwrap();
    let after_first = cache.built();
    cache
        .get(
            &device,
            PipelineKey {
                kind: PipelineKind::Quad,
                format: wgpu::TextureFormat::Bgra8UnormSrgb,
                samples: 1,
            },
        )
        .unwrap();
    assert_eq!(cache.built(), after_first + 1, "format is part of the cache key");
}
