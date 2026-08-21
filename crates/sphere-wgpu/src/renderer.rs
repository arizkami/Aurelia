//! The wgpu rendering backend.
//!
//! Everything wgpu-specific in Sphere lives behind this module. It consumes a
//! [`CompiledFrame`] — which is pure data, produced without a GPU — and issues
//! draw calls for it.
//!
//! Three things it deliberately does *not* do:
//!
//! * Allocate per frame. Buffers grow and are reused; offscreen layer targets
//!   come from a pool keyed on size.
//! * Recreate pipelines. They are compiled on first use and cached by
//!   `(kind, target format)`.
//! * Panic on a surface problem. A minimised window, a lost surface and a
//!   resize race are all normal runtime conditions with defined recoveries.

use crate::buffer::{GrowableBuffer, align_up};
use crate::pipeline::{PipelineCache, PipelineKey, PipelineKind};
use crate::texture::TextureStore;
use sphere_core::{
    Color, DevicePx, InitError, RenderError, ScaleFactor, Size, SurfaceError, TextureId,
};
use sphere_render::{
    BackendCapabilities, Batch, BatchKind, CompiledFrame, Composite, Filter, FrameHandle,
    FrameStats, GpuClip, GpuGradient, GpuTransform, Pass, PresentPreference, RendererBackend,
    SurfaceConfig, VsyncMode,
};
use std::sync::Arc;

/// Uniforms bound once per render pass.
///
/// Larger than [`sphere_render::FrameUniforms`] because the target origin is a
/// per-pass value: an offscreen layer is allocated only as large as its
/// content, so its coordinate space is shifted.
#[derive(Copy, Clone, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct PassUniforms {
    viewport: [f32; 2],
    scale_factor: f32,
    time: f32,
    target_origin: [f32; 2],
    _pad: [f32; 2],
}

/// Uniforms for one layer composite.
#[derive(Copy, Clone, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct CompositeUniforms {
    dest: [f32; 4],
    opacity: f32,
    blend: u32,
    saturation: f32,
    _pad: f32,
}

/// An offscreen render target, pooled across frames.
struct LayerTarget {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    size: (u32, u32),
    /// Frame index this target was last used on, so stale sizes can be reaped.
    last_used: u64,
}

/// The colour format Sphere renders offscreen layers into.
///
/// sRGB so that blending inside a layer matches blending on the surface; using
/// a linear format here would make a group's internal compositing subtly
/// different from the same content drawn directly.
const LAYER_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// A wgpu-backed renderer for one surface.
pub struct WgpuRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    surface_caps_alpha: wgpu::CompositeAlphaMode,
    adapter_info: String,
    limits: wgpu::Limits,
    capabilities: BackendCapabilities,

    pipelines: PipelineCache,
    textures: TextureStore,

    quad_buffer: GrowableBuffer,
    glyph_buffer: GrowableBuffer,
    mesh_vertex_buffer: GrowableBuffer,
    mesh_index_buffer: GrowableBuffer,
    transform_buffer: GrowableBuffer,
    clip_buffer: GrowableBuffer,
    gradient_buffer: GrowableBuffer,
    pass_uniform_buffer: GrowableBuffer,
    composite_uniform_buffer: GrowableBuffer,

    frame_bind_group: Option<wgpu::BindGroup>,
    /// Rebuilt whenever a table buffer is reallocated.
    frame_bind_dirty: bool,

    layer_pool: Vec<LayerTarget>,
    linear_sampler: wgpu::Sampler,

    frame_index: u64,
    /// Scratch, reused each frame so a resize does not allocate.
    pending_view: Option<wgpu::TextureView>,
    pending_surface: Option<wgpu::SurfaceTexture>,
    last_stats: FrameStats,
    uniform_alignment: u64,
}

impl WgpuRenderer {
    /// Creates a renderer for a window.
    ///
    /// `window` must outlive the renderer; wrapping it in an [`Arc`] is the
    /// usual way to satisfy that and is what lets the surface be `'static`.
    pub async fn new<W>(
        window: Arc<W>,
        size: Size<DevicePx>,
        scale_factor: ScaleFactor,
        present: PresentPreference,
        vsync: VsyncMode,
        transparent: bool,
    ) -> Result<Self, InitError>
    where
        W: wgpu::WasmNotSendSync
            + raw_window_handle::HasWindowHandle
            + raw_window_handle::HasDisplayHandle
            + 'static,
    {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let surface =
            instance.create_surface(window).map_err(|e| InitError::Surface(e.to_string()))?;

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
                apply_limit_buckets: false,
            })
            .await
            .map_err(|e| InitError::NoAdapter(e.to_string()))?;

        let info = adapter.get_info();
        let adapter_info = format!("{} ({:?}, {})", info.name, info.backend, info.driver);
        tracing::info!(adapter = %adapter_info, "sphere-wgpu adapter selected");

        let adapter_limits = adapter.limits();
        // Ask only for what the engine actually needs; requesting the adapter's
        // full limits would fail on any device weaker than this one.
        let mut required =
            wgpu::Limits::downlevel_defaults().using_resolution(adapter_limits.clone());
        required.max_texture_dimension_2d = adapter_limits.max_texture_dimension_2d.min(8192);
        required.max_storage_buffers_per_shader_stage =
            adapter_limits.max_storage_buffers_per_shader_stage.clamp(3, 4);
        required.max_storage_buffer_binding_size = adapter_limits.max_storage_buffer_binding_size;
        required.max_buffer_size = adapter_limits.max_buffer_size;

        let timestamps = adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY);

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("sphere.device"),
                required_features: wgpu::Features::empty(),
                required_limits: required,
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                memory_hints: wgpu::MemoryHints::Performance,
                trace: wgpu::Trace::Off,
            })
            .await
            .map_err(|e| InitError::DeviceCreation(e.to_string()))?;

        // A validation error anywhere must surface as a log line rather than a
        // silently wrong frame. This is the single most useful thing to have on
        // when a shader change goes wrong.
        device.on_uncaptured_error(std::sync::Arc::new(|e| {
            tracing::error!(target: "sphere_wgpu", "wgpu error: {e}");
        }));

        let caps = surface.get_capabilities(&adapter);
        let format = pick_surface_format(&caps);
        let alpha_mode = pick_alpha_mode(&caps, transparent);
        let present_mode = pick_present_mode(&caps, vsync);

        let (w, h) = (size.width.as_u32().max(1), size.height.as_u32().max(1));
        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: w,
            height: h,
            present_mode,
            desired_maximum_frame_latency: present.max_frame_latency(),
            alpha_mode,
            view_formats: vec![],
        };
        surface.configure(&device, &surface_config);

        let limits = device.limits();
        let uniform_alignment = limits.min_uniform_buffer_offset_alignment.max(16) as u64;

        let capabilities = BackendCapabilities {
            max_texture_size: limits.max_texture_dimension_2d,
            timestamp_queries: timestamps,
            offscreen_targets: true,
            compute: limits.max_compute_workgroup_size_x > 0,
            max_instances_per_draw: u32::MAX,
        };

        let linear_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("sphere.linear"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });

        let pipelines = PipelineCache::new(&device);
        let mut textures = TextureStore::new(&device, &queue);
        // Every untextured pipeline still binds group 1, so the 1x1 white
        // texture must exist before the first draw rather than on demand.
        textures.ensure_white(&device, &queue, &pipelines.layouts().texture);

        let _ = scale_factor;
        Ok(Self {
            quad_buffer: GrowableBuffer::new(
                &device,
                "sphere.quads",
                wgpu::BufferUsages::VERTEX,
                64 * 1024,
            ),
            glyph_buffer: GrowableBuffer::new(
                &device,
                "sphere.glyphs",
                wgpu::BufferUsages::VERTEX,
                64 * 1024,
            ),
            mesh_vertex_buffer: GrowableBuffer::new(
                &device,
                "sphere.mesh_v",
                wgpu::BufferUsages::VERTEX,
                32 * 1024,
            ),
            mesh_index_buffer: GrowableBuffer::new(
                &device,
                "sphere.mesh_i",
                wgpu::BufferUsages::INDEX,
                16 * 1024,
            ),
            transform_buffer: GrowableBuffer::new(
                &device,
                "sphere.transforms",
                wgpu::BufferUsages::STORAGE,
                8 * 1024,
            ),
            clip_buffer: GrowableBuffer::new(
                &device,
                "sphere.clips",
                wgpu::BufferUsages::STORAGE,
                8 * 1024,
            ),
            gradient_buffer: GrowableBuffer::new(
                &device,
                "sphere.gradients",
                wgpu::BufferUsages::STORAGE,
                8 * 1024,
            ),
            pass_uniform_buffer: GrowableBuffer::new(
                &device,
                "sphere.pass_uniforms",
                wgpu::BufferUsages::UNIFORM,
                8 * 1024,
            ),
            composite_uniform_buffer: GrowableBuffer::new(
                &device,
                "sphere.composite_uniforms",
                wgpu::BufferUsages::UNIFORM,
                4 * 1024,
            ),
            device,
            queue,
            surface,
            surface_config,
            surface_caps_alpha: alpha_mode,
            adapter_info,
            limits,
            capabilities,
            pipelines,
            textures,
            frame_bind_group: None,
            frame_bind_dirty: true,
            layer_pool: Vec::new(),
            linear_sampler,
            frame_index: 0,
            pending_view: None,
            pending_surface: None,
            last_stats: FrameStats::default(),
            uniform_alignment,
        })
    }

    /// The surface's colour format.
    #[inline]
    pub fn surface_format(&self) -> wgpu::TextureFormat {
        self.surface_config.format
    }

    /// The wgpu device, for callers that need to build their own resources.
    #[inline]
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// The wgpu queue.
    #[inline]
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// Uploads a glyph atlas page or any other engine-owned texture.
    #[inline]
    pub fn textures_mut(&mut self) -> &mut TextureStore {
        &mut self.textures
    }

    /// Statistics from the most recently submitted frame.
    #[inline]
    pub fn last_stats(&self) -> FrameStats {
        self.last_stats
    }

    fn upload_tables(&mut self, frame: &CompiledFrame) -> u64 {
        let mut bytes = 0u64;
        // Storage buffers may not be empty in WGSL, so every table gets at
        // least one element. An empty frame is entirely normal for a UI.
        let identity = [GpuTransform::IDENTITY];
        let transforms: &[GpuTransform] =
            if frame.transforms.is_empty() { &identity } else { &frame.transforms };
        let infinite = [GpuClip::INFINITE];
        let clips: &[GpuClip] = if frame.clips.is_empty() { &infinite } else { &frame.clips };
        let no_gradient = [GpuGradient::default()];
        let gradients: &[GpuGradient] =
            if frame.gradients.is_empty() { &no_gradient } else { &frame.gradients };

        let mut dirty = false;
        dirty |= self.transform_buffer.write(&self.device, &self.queue, transforms);
        dirty |= self.clip_buffer.write(&self.device, &self.queue, clips);
        dirty |= self.gradient_buffer.write(&self.device, &self.queue, gradients);
        bytes += (transforms.len() * 32 + clips.len() * 32) as u64;
        bytes += core::mem::size_of_val(gradients) as u64;

        if !frame.quads.is_empty() {
            self.quad_buffer.write(&self.device, &self.queue, &frame.quads);
            bytes += (frame.quads.len() * 96) as u64;
        }
        if !frame.glyphs.is_empty() {
            self.glyph_buffer.write(&self.device, &self.queue, &frame.glyphs);
            bytes += (frame.glyphs.len() * 96) as u64;
        }
        if !frame.mesh_vertices.is_empty() {
            self.mesh_vertex_buffer.write(&self.device, &self.queue, &frame.mesh_vertices);
            self.mesh_index_buffer.write(&self.device, &self.queue, &frame.mesh_indices);
            bytes += (frame.mesh_vertices.len() * 32 + frame.mesh_indices.len() * 4) as u64;
        }

        // Pass uniforms are packed at the device's minimum dynamic-offset
        // stride so one buffer and one bind group serve every pass.
        let stride = align_up(core::mem::size_of::<PassUniforms>() as u64, self.uniform_alignment);
        let mut pass_data = vec![0u8; (stride as usize) * frame.passes.len().max(1)];
        for (i, pass) in frame.passes.iter().enumerate() {
            let target = frame.targets.get(pass.target as usize);
            let (vw, vh) = target
                .map(|t| {
                    (t.size.width.as_u32().max(1) as f32, t.size.height.as_u32().max(1) as f32)
                })
                .unwrap_or((1.0, 1.0));
            let origin = target.map(|t| [t.origin.x.get(), t.origin.y.get()]).unwrap_or([0.0, 0.0]);
            let u = PassUniforms {
                viewport: [vw, vh],
                scale_factor: frame.uniforms.scale_factor,
                time: frame.uniforms.time,
                target_origin: origin,
                _pad: [0.0; 2],
            };
            let off = i * stride as usize;
            pass_data[off..off + core::mem::size_of::<PassUniforms>()]
                .copy_from_slice(bytemuck::bytes_of(&u));
        }
        dirty |= self.pass_uniform_buffer.write_bytes(&self.device, &self.queue, &pass_data);
        bytes += pass_data.len() as u64;

        let composites: Vec<&Composite> =
            frame.passes.iter().filter_map(|p| p.composite.as_ref()).collect();
        let cstride =
            align_up(core::mem::size_of::<CompositeUniforms>() as u64, self.uniform_alignment);
        let mut cdata = vec![0u8; (cstride as usize) * composites.len().max(1)];
        for (i, c) in composites.iter().enumerate() {
            let saturation = match c.filter {
                Some(Filter::Saturate(s)) => s,
                _ => 1.0,
            };
            let u = CompositeUniforms {
                dest: [
                    c.bounds.min_x().get(),
                    c.bounds.min_y().get(),
                    c.bounds.width().get(),
                    c.bounds.height().get(),
                ],
                opacity: c.opacity,
                blend: c.blend as u32,
                saturation,
                _pad: 0.0,
            };
            let off = i * cstride as usize;
            cdata[off..off + core::mem::size_of::<CompositeUniforms>()]
                .copy_from_slice(bytemuck::bytes_of(&u));
        }
        self.composite_uniform_buffer.write_bytes(&self.device, &self.queue, &cdata);
        bytes += cdata.len() as u64;

        if dirty {
            self.frame_bind_dirty = true;
        }
        bytes
    }

    fn ensure_frame_bind_group(&mut self) {
        if !self.frame_bind_dirty && self.frame_bind_group.is_some() {
            return;
        }
        let stride = align_up(core::mem::size_of::<PassUniforms>() as u64, self.uniform_alignment);
        let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sphere.frame_bind"),
            layout: &self.pipelines.layouts().frame,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: self.pass_uniform_buffer.raw(),
                        offset: 0,
                        size: core::num::NonZeroU64::new(stride),
                    }),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: self.transform_buffer.raw().as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.clip_buffer.raw().as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: self.gradient_buffer.raw().as_entire_binding(),
                },
            ],
        });
        self.frame_bind_group = Some(bg);
        self.frame_bind_dirty = false;
    }

    /// Acquires an offscreen target of the requested size from the pool.
    fn acquire_layer(&mut self, width: u32, height: u32) -> usize {
        let (w, h) = (width.max(1), height.max(1));
        if let Some(i) =
            self.layer_pool.iter().position(|t| t.size == (w, h) && t.last_used != self.frame_index)
        {
            self.layer_pool[i].last_used = self.frame_index;
            return i;
        }
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("sphere.layer"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: LAYER_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.layer_pool.push(LayerTarget {
            texture,
            view,
            size: (w, h),
            last_used: self.frame_index,
        });
        self.layer_pool.len() - 1
    }

    /// Drops pooled targets that have gone several frames without use.
    ///
    /// A window that briefly showed a blurred popup should not hold that render
    /// target for the rest of the session, but neither should a target be freed
    /// the moment one frame skips it — a menu opening and closing repeatedly
    /// would then reallocate every time.
    fn reap_layers(&mut self) {
        const KEEP_FOR_FRAMES: u64 = 60;
        let current = self.frame_index;
        let mut i = 0;
        while i < self.layer_pool.len() {
            if current.saturating_sub(self.layer_pool[i].last_used) >= KEEP_FOR_FRAMES {
                let t = self.layer_pool.swap_remove(i);
                t.texture.destroy();
            } else {
                i += 1;
            }
        }
    }
}

fn pick_surface_format(caps: &wgpu::SurfaceCapabilities) -> wgpu::TextureFormat {
    // An sRGB target means the hardware does the encode and blending happens in
    // linear space, which is the whole premise of the colour pipeline.
    caps.formats
        .iter()
        .copied()
        .find(|f| *f == wgpu::TextureFormat::Bgra8UnormSrgb)
        .or_else(|| caps.formats.iter().copied().find(|f| f.is_srgb()))
        .unwrap_or(caps.formats[0])
}

fn pick_alpha_mode(
    caps: &wgpu::SurfaceCapabilities,
    transparent: bool,
) -> wgpu::CompositeAlphaMode {
    if transparent {
        for m in [wgpu::CompositeAlphaMode::PreMultiplied, wgpu::CompositeAlphaMode::PostMultiplied]
        {
            if caps.alpha_modes.contains(&m) {
                return m;
            }
        }
    }
    caps.alpha_modes.first().copied().unwrap_or(wgpu::CompositeAlphaMode::Auto)
}

fn pick_present_mode(caps: &wgpu::SurfaceCapabilities, vsync: VsyncMode) -> wgpu::PresentMode {
    let want: &[wgpu::PresentMode] = match vsync {
        // Mailbox first: it presents the newest frame without tearing, which is
        // the best latency/quality trade when the driver offers it.
        VsyncMode::On => &[wgpu::PresentMode::Fifo],
        VsyncMode::Mailbox => &[wgpu::PresentMode::Mailbox, wgpu::PresentMode::Fifo],
        VsyncMode::Off => {
            &[wgpu::PresentMode::Immediate, wgpu::PresentMode::Mailbox, wgpu::PresentMode::Fifo]
        }
    };
    want.iter()
        .copied()
        .find(|m| caps.present_modes.contains(m))
        // Fifo is the only mode every backend is required to support.
        .unwrap_or(wgpu::PresentMode::Fifo)
}

impl RendererBackend for WgpuRenderer {
    fn adapter_name(&self) -> &str {
        &self.adapter_info
    }

    fn capabilities(&self) -> BackendCapabilities {
        self.capabilities
    }

    fn configure_surface(&mut self, config: SurfaceConfig) -> Result<(), RenderError> {
        let w = config.size.width.as_u32();
        let h = config.size.height.as_u32();
        // A minimised window legitimately reports zero. Reconfiguring to zero
        // is invalid on every backend, so skip and keep the last good config;
        // `begin_frame` will report `Occluded` until the window comes back.
        if w == 0 || h == 0 {
            return Ok(());
        }
        let caps_alpha = self.surface_caps_alpha;
        self.surface_config.width = w;
        self.surface_config.height = h;
        self.surface_config.alpha_mode = caps_alpha;
        self.surface_config.desired_maximum_frame_latency = config.present.max_frame_latency();
        self.surface.configure(&self.device, &self.surface_config);
        Ok(())
    }

    fn begin_frame(&mut self) -> Result<FrameHandle, SurfaceError> {
        if self.surface_config.width == 0 || self.surface_config.height == 0 {
            return Err(SurfaceError::ZeroSized);
        }
        let acquired = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                // Usable this frame, but the surface wants reconfiguring; do it
                // now so the next frame is optimal again.
                self.surface.configure(&self.device, &self.surface_config);
                t
            }
            wgpu::CurrentSurfaceTexture::Timeout => return Err(SurfaceError::Timeout),
            wgpu::CurrentSurfaceTexture::Occluded => return Err(SurfaceError::Occluded),
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.surface_config);
                return Err(SurfaceError::Outdated);
            }
            wgpu::CurrentSurfaceTexture::Lost => return Err(SurfaceError::Lost),
            wgpu::CurrentSurfaceTexture::Validation => return Err(SurfaceError::Outdated),
        };

        let view = acquired.texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.frame_index += 1;
        self.pending_view = Some(view);
        self.pending_surface = Some(acquired);

        Ok(FrameHandle {
            index: self.frame_index,
            size: Size::new(
                DevicePx(self.surface_config.width as i32),
                DevicePx(self.surface_config.height as i32),
            ),
            slot: 0,
        })
    }

    fn render(
        &mut self,
        frame: &mut FrameHandle,
        compiled: &CompiledFrame,
        clear: Color,
    ) -> Result<(), RenderError> {
        let Some(surface_view) = self.pending_view.take() else {
            return Err(RenderError::Surface(SurfaceError::Lost));
        };

        let bytes = self.upload_tables(compiled);
        self.ensure_frame_bind_group();
        self.textures.flush(&self.queue);

        // Reserve an offscreen texture for every non-surface target before any
        // pass runs, so a nested layer never has to allocate mid-pass.
        let mut layer_slots: Vec<Option<usize>> = Vec::with_capacity(compiled.targets.len());
        for t in &compiled.targets {
            if t.is_surface {
                layer_slots.push(None);
            } else {
                let slot = self.acquire_layer(t.size.width.as_u32(), t.size.height.as_u32());
                layer_slots.push(Some(slot));
            }
        }

        let surface_format = self.surface_config.format;
        let pass_stride =
            align_up(core::mem::size_of::<PassUniforms>() as u64, self.uniform_alignment) as u32;
        let composite_stride =
            align_up(core::mem::size_of::<CompositeUniforms>() as u64, self.uniform_alignment)
                as u32;

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("sphere.frame"),
        });

        let mut draw_calls = 0u32;
        let mut pipeline_switches = 0u32;
        let mut composite_index = 0u32;

        // If the scene produced no passes at all, still clear the surface so a
        // window does not show uninitialised memory.
        let passes: Vec<&Pass> = compiled.passes.iter().collect();
        if passes.is_empty() {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("sphere.clear_only"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &surface_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(to_wgpu_color(clear)),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_scissor_rect(0, 0, self.surface_config.width, self.surface_config.height);
        }

        for (pass_index, pass) in passes.iter().enumerate() {
            let is_surface =
                compiled.targets.get(pass.target as usize).map(|t| t.is_surface).unwrap_or(true);
            let (target_w, target_h) = compiled
                .targets
                .get(pass.target as usize)
                .map(|t| (t.size.width.as_u32().max(1), t.size.height.as_u32().max(1)))
                .unwrap_or((self.surface_config.width, self.surface_config.height));
            let format = if is_surface { surface_format } else { LAYER_FORMAT };

            // Compile every pipeline this pass needs before opening the render
            // pass; `PipelineCache::get` needs `&mut self`, and a render pass
            // borrows the encoder for its whole lifetime.
            for bi in pass.batches.clone() {
                if let Some(batch) = compiled.batches.get(bi as usize) {
                    let kind = pipeline_kind_for(batch);
                    self.pipelines
                        .get(&self.device, PipelineKey { kind, format })
                        .map_err(RenderError::Shader)?;
                }
            }
            if pass.composite.is_some() {
                self.pipelines
                    .get(&self.device, PipelineKey { kind: PipelineKind::Composite, format })
                    .map_err(RenderError::Shader)?;
            }

            {
                let view = if is_surface {
                    &surface_view
                } else {
                    let slot = layer_slots[pass.target as usize].expect("offscreen slot");
                    &self.layer_pool[slot].view
                };

                let load = if pass.clear {
                    // Offscreen layers must start fully transparent, or content
                    // from a previous frame's use of the pooled texture bleeds
                    // through the composite.
                    let c =
                        if is_surface { to_wgpu_color(clear) } else { wgpu::Color::TRANSPARENT };
                    wgpu::LoadOp::Clear(c)
                } else {
                    wgpu::LoadOp::Load
                };

                let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("sphere.pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations { load, store: wgpu::StoreOp::Store },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });

                let frame_bind = self.frame_bind_group.as_ref().expect("frame bind group");
                let dynamic_offset = pass_index as u32 * pass_stride;
                rp.set_bind_group(0, frame_bind, &[dynamic_offset]);

                let mut last_kind: Option<PipelineKind> = None;
                for bi in pass.batches.clone() {
                    let Some(batch) = compiled.batches.get(bi as usize) else { continue };
                    if batch.range.is_empty() {
                        continue;
                    }
                    let kind = pipeline_kind_for(batch);
                    let pipeline = self
                        .pipelines
                        .get(&self.device, PipelineKey { kind, format })
                        .expect("pipeline was precompiled above");
                    if last_kind != Some(kind) {
                        rp.set_pipeline(pipeline);
                        pipeline_switches += 1;
                        last_kind = Some(kind);
                    }

                    // A scissor rectangle outside the attachment is a
                    // validation error, so clamp rather than trusting the
                    // compiler's logical-space arithmetic.
                    let (sx, sy, sw, sh) =
                        clamp_scissor(batch, compiled, pass.target, target_w, target_h);
                    if sw == 0 || sh == 0 {
                        continue;
                    }
                    rp.set_scissor_rect(sx, sy, sw, sh);

                    match batch.kind {
                        BatchKind::Quad => {
                            rp.set_bind_group(1, self.textures.white_bind_group(), &[]);
                            rp.set_vertex_buffer(0, self.quad_buffer.raw().slice(..));
                            rp.draw(0..4, batch.range.clone());
                            draw_calls += 1;
                        }
                        BatchKind::Image { texture } => {
                            let bg = self
                                .textures
                                .bind_group(texture)
                                .unwrap_or_else(|| self.textures.white_bind_group());
                            rp.set_bind_group(1, bg, &[]);
                            rp.set_vertex_buffer(0, self.quad_buffer.raw().slice(..));
                            rp.draw(0..4, batch.range.clone());
                            draw_calls += 1;
                        }
                        BatchKind::Glyph { page, .. } => {
                            let Some(bg) = self.textures.atlas_bind_group(page) else { continue };
                            rp.set_bind_group(1, bg, &[]);
                            rp.set_vertex_buffer(0, self.glyph_buffer.raw().slice(..));
                            rp.draw(0..4, batch.range.clone());
                            draw_calls += 1;
                        }
                        BatchKind::Mesh { texture } => {
                            let bg = texture
                                .and_then(|t| self.textures.bind_group(t))
                                .unwrap_or_else(|| self.textures.white_bind_group());
                            rp.set_bind_group(1, bg, &[]);
                            rp.set_vertex_buffer(0, self.mesh_vertex_buffer.raw().slice(..));
                            rp.set_index_buffer(
                                self.mesh_index_buffer.raw().slice(..),
                                wgpu::IndexFormat::Uint32,
                            );
                            rp.draw_indexed(batch.range.clone(), batch.base_vertex as i32, 0..1);
                            draw_calls += 1;
                        }
                    }
                }
            }

            if let Some(c) = &pass.composite {
                let dest_is_surface = compiled
                    .targets
                    .get(c.destination as usize)
                    .map(|t| t.is_surface)
                    .unwrap_or(true);
                let dest_format = if dest_is_surface { surface_format } else { LAYER_FORMAT };
                let source_slot = layer_slots[c.source as usize].expect("composite source");

                let composite_bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("sphere.composite_bind"),
                    layout: &self.pipelines.layouts().composite,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(
                                &self.layer_pool[source_slot].view,
                            ),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(&self.linear_sampler),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                buffer: self.composite_uniform_buffer.raw(),
                                offset: 0,
                                size: core::num::NonZeroU64::new(core::mem::size_of::<
                                    CompositeUniforms,
                                >()
                                    as u64),
                            }),
                        },
                    ],
                });

                let pipeline = self
                    .pipelines
                    .get(
                        &self.device,
                        PipelineKey { kind: PipelineKind::Composite, format: dest_format },
                    )
                    .map_err(RenderError::Shader)?;

                let dest_view = if dest_is_surface {
                    &surface_view
                } else {
                    let slot = layer_slots[c.destination as usize].expect("composite destination");
                    &self.layer_pool[slot].view
                };

                // Find which pass writes the destination next so the dynamic
                // offset matches that target's uniforms, not this layer's.
                let dest_pass =
                    passes.iter().position(|p| p.target == c.destination).unwrap_or(0) as u32;

                let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("sphere.composite"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: dest_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            // Never clear: the destination already holds the
                            // content this layer composites on top of.
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                rp.set_pipeline(pipeline);
                let frame_bind = self.frame_bind_group.as_ref().expect("frame bind group");
                rp.set_bind_group(0, frame_bind, &[dest_pass * pass_stride]);
                rp.set_bind_group(1, &composite_bind, &[composite_index * composite_stride]);
                rp.draw(0..4, 0..1);
                draw_calls += 1;
                pipeline_switches += 1;
                composite_index += 1;
            }
        }

        self.queue.submit(Some(encoder.finish()));
        self.pending_view = Some(surface_view);

        self.last_stats = FrameStats {
            cpu_ms: 0.0,
            gpu_ms: None,
            draw_calls,
            pipeline_switches,
            quads: compiled.stats.quads,
            glyphs: compiled.stats.glyphs,
            triangles: compiled.stats.triangles,
            bytes_uploaded: bytes,
            layers: compiled.stats.layers,
        };
        let _ = frame;
        Ok(())
    }

    fn end_frame(&mut self, frame: FrameHandle) -> Result<FrameStats, RenderError> {
        self.pending_view = None;
        if let Some(surface_texture) = self.pending_surface.take() {
            self.queue.present(surface_texture);
        }
        self.reap_layers();
        let _ = frame;
        Ok(self.last_stats)
    }

    fn upload_texture(
        &mut self,
        id: Option<TextureId>,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<TextureId, RenderError> {
        let max = self.limits.max_texture_dimension_2d;
        if width == 0 || height == 0 || width > max || height > max {
            return Err(RenderError::Backend(format!(
                "texture {width}x{height} is outside the supported range 1..={max}"
            )));
        }
        self.textures.upload(
            &self.device,
            &self.queue,
            &self.pipelines.layouts().texture,
            id,
            width,
            height,
            rgba,
        )
    }

    fn upload_texture_region(
        &mut self,
        id: TextureId,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<(), RenderError> {
        self.textures.upload_region(&self.queue, id, x, y, width, height, rgba)
    }

    fn destroy_texture(&mut self, id: TextureId) {
        self.textures.destroy(id);
    }

    fn memory_usage(&self) -> u64 {
        self.textures.memory_bytes()
            + self.quad_buffer.capacity()
            + self.glyph_buffer.capacity()
            + self.mesh_vertex_buffer.capacity()
            + self.mesh_index_buffer.capacity()
            + self.transform_buffer.capacity()
            + self.clip_buffer.capacity()
            + self.gradient_buffer.capacity()
            + self.layer_pool.iter().map(|t| (t.size.0 as u64) * (t.size.1 as u64) * 4).sum::<u64>()
    }
}

fn pipeline_kind_for(batch: &Batch) -> PipelineKind {
    match batch.kind {
        BatchKind::Quad => PipelineKind::Quad,
        BatchKind::Image { .. } => PipelineKind::Image,
        BatchKind::Glyph { .. } => PipelineKind::Text,
        BatchKind::Mesh { texture: None } => PipelineKind::Mesh,
        BatchKind::Mesh { texture: Some(_) } => PipelineKind::MeshTextured,
    }
}

/// Clamps a batch's scissor into its attachment.
///
/// The batch compiler works in scene-absolute logical space; an offscreen
/// target's own space is shifted by its origin. Converting and clamping here,
/// once, keeps that knowledge out of the compiler.
fn clamp_scissor(
    batch: &Batch,
    frame: &CompiledFrame,
    target_index: u32,
    target_w: u32,
    target_h: u32,
) -> (u32, u32, u32, u32) {
    let scale = frame.uniforms.scale_factor.max(1e-3);
    let (ox, oy) = frame
        .targets
        .get(target_index as usize)
        .map(|t| (t.origin.x.get() * scale, t.origin.y.get() * scale))
        .unwrap_or((0.0, 0.0));

    let x0 = (batch.scissor.min_x().get() as f32 - ox).floor().max(0.0) as u32;
    let y0 = (batch.scissor.min_y().get() as f32 - oy).floor().max(0.0) as u32;
    let x1 = (batch.scissor.max_x().get() as f32 - ox).ceil().max(0.0) as u32;
    let y1 = (batch.scissor.max_y().get() as f32 - oy).ceil().max(0.0) as u32;

    let x0 = x0.min(target_w);
    let y0 = y0.min(target_h);
    let x1 = x1.min(target_w);
    let y1 = y1.min(target_h);
    (x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0))
}

fn to_wgpu_color(c: Color) -> wgpu::Color {
    // The surface is sRGB, so the hardware encodes on write; feed it linear.
    let l = c.to_linear();
    wgpu::Color { r: l.r as f64, g: l.g as f64, b: l.b as f64, a: l.a as f64 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sphere_core::{Point, Rect, px, rect};
    use sphere_render::RenderTarget;

    fn frame_with_target(origin: (f32, f32), scale: f32) -> CompiledFrame {
        let mut f = CompiledFrame::default();
        f.uniforms.scale_factor = scale;
        f.targets.push(RenderTarget {
            is_surface: false,
            size: Size::new(DevicePx(100), DevicePx(100)),
            origin: Point::new(px(origin.0), px(origin.1)),
        });
        f
    }

    fn batch_with_scissor(r: Rect<DevicePx>) -> Batch {
        Batch { kind: BatchKind::Quad, scissor: r, range: 0..1, base_vertex: 0, target: 0 }
    }

    #[test]
    fn scissor_is_shifted_into_offscreen_target_space() {
        let f = frame_with_target((50.0, 20.0), 1.0);
        let b = batch_with_scissor(rect(DevicePx(60), DevicePx(30), DevicePx(10), DevicePx(10)));
        assert_eq!(clamp_scissor(&b, &f, 0, 100, 100), (10, 10, 10, 10));
    }

    #[test]
    fn scissor_origin_shift_accounts_for_dpi() {
        let f = frame_with_target((50.0, 0.0), 2.0);
        // 50 logical px of origin is 100 device px.
        let b = batch_with_scissor(rect(DevicePx(120), DevicePx(0), DevicePx(10), DevicePx(10)));
        assert_eq!(clamp_scissor(&b, &f, 0, 200, 200).0, 20);
    }

    #[test]
    fn a_scissor_reaching_past_the_attachment_is_clamped() {
        // An out-of-range scissor is a wgpu validation error, not a warning.
        let f = frame_with_target((0.0, 0.0), 1.0);
        let b = batch_with_scissor(rect(DevicePx(90), DevicePx(90), DevicePx(500), DevicePx(500)));
        let (x, y, w, h) = clamp_scissor(&b, &f, 0, 100, 100);
        assert!(x + w <= 100 && y + h <= 100, "{x},{y},{w},{h}");
        assert_eq!((w, h), (10, 10));
    }

    #[test]
    fn a_scissor_entirely_outside_the_attachment_collapses_to_zero() {
        let f = frame_with_target((0.0, 0.0), 1.0);
        let b = batch_with_scissor(rect(DevicePx(500), DevicePx(500), DevicePx(10), DevicePx(10)));
        let (_, _, w, h) = clamp_scissor(&b, &f, 0, 100, 100);
        assert_eq!((w, h), (0, 0), "a zero-area scissor must be skipped, not submitted");
    }

    #[test]
    fn a_negative_scissor_origin_is_clamped_to_zero() {
        let f = frame_with_target((0.0, 0.0), 1.0);
        let b =
            batch_with_scissor(rect(DevicePx(-50), DevicePx(-50), DevicePx(100), DevicePx(100)));
        let (x, y, w, h) = clamp_scissor(&b, &f, 0, 100, 100);
        assert_eq!((x, y), (0, 0));
        assert_eq!((w, h), (50, 50));
    }

    #[test]
    fn present_mode_falls_back_to_fifo_when_nothing_else_is_offered() {
        let caps = wgpu::SurfaceCapabilities {
            present_modes: vec![wgpu::PresentMode::Fifo],
            ..Default::default()
        };
        assert_eq!(pick_present_mode(&caps, VsyncMode::Off), wgpu::PresentMode::Fifo);
        assert_eq!(pick_present_mode(&caps, VsyncMode::Mailbox), wgpu::PresentMode::Fifo);
    }

    #[test]
    fn vsync_off_prefers_immediate_when_available() {
        let caps = wgpu::SurfaceCapabilities {
            present_modes: vec![
                wgpu::PresentMode::Fifo,
                wgpu::PresentMode::Mailbox,
                wgpu::PresentMode::Immediate,
            ],
            ..Default::default()
        };
        assert_eq!(pick_present_mode(&caps, VsyncMode::Off), wgpu::PresentMode::Immediate);
        assert_eq!(pick_present_mode(&caps, VsyncMode::Mailbox), wgpu::PresentMode::Mailbox);
        assert_eq!(pick_present_mode(&caps, VsyncMode::On), wgpu::PresentMode::Fifo);
    }

    #[test]
    fn an_srgb_surface_format_is_preferred() {
        let caps = wgpu::SurfaceCapabilities {
            formats: vec![wgpu::TextureFormat::Rgba8Unorm, wgpu::TextureFormat::Bgra8UnormSrgb],
            ..Default::default()
        };
        assert_eq!(pick_surface_format(&caps), wgpu::TextureFormat::Bgra8UnormSrgb);
    }

    #[test]
    fn a_non_srgb_only_adapter_still_yields_a_usable_format() {
        let caps = wgpu::SurfaceCapabilities {
            formats: vec![wgpu::TextureFormat::Rgba8Unorm],
            ..Default::default()
        };
        assert_eq!(pick_surface_format(&caps), wgpu::TextureFormat::Rgba8Unorm);
    }

    #[test]
    fn clear_colour_is_converted_to_linear() {
        // The surface is sRGB, so a mid-grey clear must not be passed through
        // as 0.5 or the window will look washed out.
        let c = to_wgpu_color(Color::rgb(0.5, 0.5, 0.5));
        assert!((c.r - 0.2140).abs() < 1e-3, "{}", c.r);
    }

    #[test]
    fn pipeline_kind_mapping_covers_every_batch_kind() {
        let b = |k| Batch { kind: k, scissor: Rect::ZERO, range: 0..1, base_vertex: 0, target: 0 };
        assert_eq!(pipeline_kind_for(&b(BatchKind::Quad)), PipelineKind::Quad);
        assert_eq!(
            pipeline_kind_for(&b(BatchKind::Glyph { page: 0, bitmap: false })),
            PipelineKind::Text
        );
        assert_eq!(pipeline_kind_for(&b(BatchKind::Mesh { texture: None })), PipelineKind::Mesh);
        assert_eq!(
            pipeline_kind_for(&b(BatchKind::Mesh { texture: Some(TextureId::new(0, 1)) })),
            PipelineKind::MeshTextured
        );
    }
}
