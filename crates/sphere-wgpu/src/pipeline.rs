//! Bind group layouts, vertex layouts and the pipeline cache.
//!
//! Pipelines are keyed by `(kind, target format)` because an offscreen layer
//! and the swapchain can differ in format, and a pipeline is bound to exactly
//! one colour target format. Building them lazily and caching them means a
//! window that never uses a layer never pays for the layer pipelines.

use crate::shader;
use rustc_hash::FxHashMap;
use sphere_core::ShaderError;
use sphere_render::{GlyphInstance, MeshVertex, QuadInstance};

/// Which pipeline a draw needs.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum PipelineKind {
    /// Rectangles, rounded rectangles, borders, gradients and shadows.
    Quad,
    /// Textured quads. Shares `quad.wgsl` but binds a real texture.
    Image,
    /// MTSDF and bitmap glyphs.
    Text,
    /// Vertex-coloured triangles.
    Mesh,
    /// Textured triangles.
    MeshTextured,
    /// Layer compositing.
    Composite,
}

/// A cache key for one built pipeline.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct PipelineKey {
    /// Which pipeline.
    pub kind: PipelineKind,
    /// The colour target format it writes.
    pub format: wgpu::TextureFormat,
    /// Multisample count of the attachment it writes.
    ///
    /// Part of the key because a pipeline is bound to one sample count. The
    /// analytic primitives — quads, glyphs — are already antialiased by their
    /// own shaders and gain nothing from multisampling, but *tessellated paths*
    /// have no analytic edge at all and are the reason this exists.
    pub samples: u32,
}

/// The layouts every Sphere pipeline shares.
pub struct Layouts {
    /// Group 0: frame uniforms plus the transform, clip and gradient tables.
    pub frame: wgpu::BindGroupLayout,
    /// Group 1: one sampled texture plus its sampler.
    pub texture: wgpu::BindGroupLayout,
    /// Group 1 for compositing: layer texture, sampler and composite params.
    pub composite: wgpu::BindGroupLayout,
    /// Group 0 for the blur passes.
    pub blur: wgpu::BindGroupLayout,
}

impl Layouts {
    /// Builds every bind group layout.
    pub fn new(device: &wgpu::Device) -> Self {
        let frame = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sphere.frame"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        // One buffer holds every pass's uniforms; a dynamic
                        // offset selects the pass. That is one allocation and
                        // one bind group for the whole frame instead of one per
                        // offscreen layer.
                        has_dynamic_offset: true,
                        min_binding_size: None,
                    },
                    count: None,
                },
                storage_entry(1, wgpu::ShaderStages::VERTEX_FRAGMENT),
                storage_entry(2, wgpu::ShaderStages::VERTEX_FRAGMENT),
                storage_entry(3, wgpu::ShaderStages::FRAGMENT),
            ],
        });

        let texture = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sphere.texture"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let composite = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sphere.composite"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let blur = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sphere.blur"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        Self { frame, texture, composite, blur }
    }
}

fn storage_entry(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

/// Vertex attribute layout for [`QuadInstance`].
///
/// The offsets mirror the struct field order exactly; a mismatch here shows up
/// as garbled geometry rather than a compile error, so the tests below assert
/// the total against `size_of`.
pub const QUAD_ATTRS: [wgpu::VertexAttribute; 6] = wgpu::vertex_attr_array![
    0 => Float32x4,  // bounds
    1 => Float32x4,  // radii
    2 => Float32x4,  // color
    3 => Float32x4,  // border_color / source rect
    4 => Float32x2,  // border_width, blur_sigma
    5 => Uint32x4,   // flags, gradient, transform_index, clip_index
];

/// Vertex attribute layout for [`GlyphInstance`].
pub const GLYPH_ATTRS: [wgpu::VertexAttribute; 6] = wgpu::vertex_attr_array![
    0 => Float32x4,  // bounds
    1 => Float32x4,  // uv
    2 => Float32x4,  // color
    3 => Float32x4,  // outline_color
    4 => Float32x4,  // px_range, outline_width, coverage_contrast, flags(bitcast)
    5 => Uint32x4,   // atlas_page, transform_index, clip_index, pad
];

/// Vertex attribute layout for [`MeshVertex`].
pub const MESH_ATTRS: [wgpu::VertexAttribute; 3] = wgpu::vertex_attr_array![
    0 => Float32x2,  // position
    1 => Float32x2,  // uv
    2 => Float32x4,  // color
];

/// Builds and caches render pipelines.
pub struct PipelineCache {
    pipelines: FxHashMap<PipelineKey, wgpu::RenderPipeline>,
    modules: FxHashMap<&'static str, wgpu::ShaderModule>,
    layouts: Layouts,
    quad_layout: wgpu::PipelineLayout,
    composite_layout: wgpu::PipelineLayout,
    blur_layout: wgpu::PipelineLayout,
    blur_pipeline: Option<wgpu::RenderPipeline>,
    /// How many pipelines have been built, for diagnostics.
    built: u32,
}

impl PipelineCache {
    /// Creates the cache and its shared pipeline layouts.
    pub fn new(device: &wgpu::Device) -> Self {
        let layouts = Layouts::new(device);
        let quad_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sphere.pipeline_layout"),
            bind_group_layouts: &[Some(&layouts.frame), Some(&layouts.texture)],
            immediate_size: 0,
        });
        let composite_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sphere.composite_layout"),
            bind_group_layouts: &[Some(&layouts.frame), Some(&layouts.composite)],
            immediate_size: 0,
        });
        let blur_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sphere.blur_layout"),
            bind_group_layouts: &[Some(&layouts.blur)],
            immediate_size: 0,
        });
        Self {
            pipelines: FxHashMap::default(),
            modules: FxHashMap::default(),
            layouts,
            quad_layout,
            composite_layout,
            blur_layout,
            blur_pipeline: None,
            built: 0,
        }
    }

    /// The shared bind group layouts.
    #[inline]
    pub fn layouts(&self) -> &Layouts {
        &self.layouts
    }

    /// How many pipelines have been compiled.
    #[inline]
    pub fn built(&self) -> u32 {
        self.built
    }

    /// Compiles (or returns) a shader module.
    ///
    /// Takes the module map rather than `&mut self` so that callers can hold an
    /// immutable borrow of a *different* field — the pipeline layout — at the
    /// same time. Going through `&mut self` would make those two borrows
    /// conflict even though they touch disjoint fields.
    fn module_for<'a>(
        modules: &'a mut FxHashMap<&'static str, wgpu::ShaderModule>,
        device: &wgpu::Device,
        path: &'static str,
    ) -> Result<&'a wgpu::ShaderModule, ShaderError> {
        if !modules.contains_key(path) {
            let src = shader::compose(path)?;
            let m = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(path),
                source: wgpu::ShaderSource::Wgsl(src.into()),
            });
            modules.insert(path, m);
        }
        Ok(&modules[path])
    }

    /// Returns a pipeline, compiling it on first use.
    pub fn get(
        &mut self,
        device: &wgpu::Device,
        key: PipelineKey,
    ) -> Result<&wgpu::RenderPipeline, ShaderError> {
        if self.pipelines.contains_key(&key) {
            return Ok(&self.pipelines[&key]);
        }

        let (path, vs, fs, stride, attrs, step): (
            &'static str,
            &str,
            &str,
            u64,
            &[wgpu::VertexAttribute],
            wgpu::VertexStepMode,
        ) = match key.kind {
            PipelineKind::Quad | PipelineKind::Image => (
                "quad.wgsl",
                "vs_main",
                "fs_main",
                core::mem::size_of::<QuadInstance>() as u64,
                &QUAD_ATTRS,
                wgpu::VertexStepMode::Instance,
            ),
            PipelineKind::Text => (
                "text.wgsl",
                "vs_main",
                "fs_main",
                core::mem::size_of::<GlyphInstance>() as u64,
                &GLYPH_ATTRS,
                wgpu::VertexStepMode::Instance,
            ),
            PipelineKind::Mesh => (
                "mesh.wgsl",
                "vs_main",
                "fs_main",
                core::mem::size_of::<MeshVertex>() as u64,
                &MESH_ATTRS,
                wgpu::VertexStepMode::Vertex,
            ),
            PipelineKind::MeshTextured => (
                "mesh.wgsl",
                "vs_main",
                "fs_textured",
                core::mem::size_of::<MeshVertex>() as u64,
                &MESH_ATTRS,
                wgpu::VertexStepMode::Vertex,
            ),
            PipelineKind::Composite => {
                ("composite.wgsl", "vs_main", "fs_main", 0, &[], wgpu::VertexStepMode::Vertex)
            }
        };

        // Disjoint field borrows: the layout is read from one field while the
        // module cache is filled through another.
        let layout = match key.kind {
            PipelineKind::Composite => &self.composite_layout,
            _ => &self.quad_layout,
        };
        let module = Self::module_for(&mut self.modules, device, path)?;

        let buffers: Vec<Option<wgpu::VertexBufferLayout<'_>>> = if stride == 0 {
            Vec::new()
        } else {
            vec![Some(wgpu::VertexBufferLayout {
                array_stride: stride,
                step_mode: step,
                attributes: attrs,
            })]
        };

        let topology = match key.kind {
            PipelineKind::Mesh | PipelineKind::MeshTextured => {
                wgpu::PrimitiveTopology::TriangleList
            }
            _ => wgpu::PrimitiveTopology::TriangleStrip,
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("sphere.pipeline"),
            layout: Some(layout),
            vertex: wgpu::VertexState {
                module,
                entry_point: Some(vs),
                compilation_options: Default::default(),
                buffers: &buffers,
            },
            fragment: Some(wgpu::FragmentState {
                module,
                entry_point: Some(fs),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: key.format,
                    // Every colour Sphere produces is premultiplied, so
                    // source-over is `src + dst * (1 - src.a)`.
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                // UI geometry is two-dimensional and can legitimately be wound
                // either way, particularly after a mirroring transform.
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: key.samples.max(1),
                mask: !0,
                // Alpha-to-coverage would quantise every translucent primitive
                // to the sample count, which is far worse than the analytic
                // alpha the shaders already produce.
                alpha_to_coverage_enabled: false,
            },
            multiview_mask: None,
            cache: None,
        });

        self.built += 1;
        self.pipelines.insert(key, pipeline);
        Ok(&self.pipelines[&key])
    }

    /// Returns the blur pipeline, compiling it on first use.
    pub fn blur(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
    ) -> Result<&wgpu::RenderPipeline, ShaderError> {
        if self.blur_pipeline.is_none() {
            let layout = &self.blur_layout;
            let module = Self::module_for(&mut self.modules, device, "blur.wgsl")?;
            let p = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("sphere.blur"),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });
            self.built += 1;
            self.blur_pipeline = Some(p);
        }
        Ok(self.blur_pipeline.as_ref().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attr_span(attrs: &[wgpu::VertexAttribute]) -> u64 {
        attrs.iter().map(|a| a.offset + vertex_format_size(a.format)).max().unwrap_or(0)
    }

    fn vertex_format_size(f: wgpu::VertexFormat) -> u64 {
        match f {
            wgpu::VertexFormat::Float32x4 | wgpu::VertexFormat::Uint32x4 => 16,
            wgpu::VertexFormat::Float32x2 | wgpu::VertexFormat::Uint32x2 => 8,
            wgpu::VertexFormat::Float32 | wgpu::VertexFormat::Uint32 => 4,
            other => panic!("unhandled format {other:?}"),
        }
    }

    #[test]
    fn quad_attributes_fit_inside_the_instance_struct() {
        // If an attribute runs past the struct, the GPU reads the next
        // instance's bytes and the frame is subtly wrong rather than broken.
        assert!(
            attr_span(&QUAD_ATTRS) <= core::mem::size_of::<QuadInstance>() as u64,
            "attributes span {} bytes but QuadInstance is {}",
            attr_span(&QUAD_ATTRS),
            core::mem::size_of::<QuadInstance>()
        );
    }

    #[test]
    fn glyph_attributes_fit_inside_the_instance_struct() {
        assert!(
            attr_span(&GLYPH_ATTRS) <= core::mem::size_of::<GlyphInstance>() as u64,
            "attributes span {} bytes but GlyphInstance is {}",
            attr_span(&GLYPH_ATTRS),
            core::mem::size_of::<GlyphInstance>()
        );
    }

    #[test]
    fn mesh_attributes_exactly_cover_the_vertex() {
        assert_eq!(attr_span(&MESH_ATTRS), core::mem::size_of::<MeshVertex>() as u64);
    }

    #[test]
    fn glyph_attribute_offsets_match_the_rust_field_order() {
        // Four vec4s, then the float block, then the index block. A silent
        // drift here shows as garbled text rather than a compile error.
        assert_eq!(GLYPH_ATTRS[0].offset, 0);
        assert_eq!(GLYPH_ATTRS[1].offset, 16);
        assert_eq!(GLYPH_ATTRS[2].offset, 32);
        assert_eq!(GLYPH_ATTRS[3].offset, 48);
        assert_eq!(GLYPH_ATTRS[4].offset, 64);
        assert_eq!(GLYPH_ATTRS[5].offset, 80);
    }

    #[test]
    fn quad_attribute_offsets_match_the_rust_field_order() {
        // bounds, radii, color, border_color are four vec4s back to back.
        assert_eq!(QUAD_ATTRS[0].offset, 0);
        assert_eq!(QUAD_ATTRS[1].offset, 16);
        assert_eq!(QUAD_ATTRS[2].offset, 32);
        assert_eq!(QUAD_ATTRS[3].offset, 48);
        // then border_width + blur_sigma, then the four u32 indices.
        assert_eq!(QUAD_ATTRS[4].offset, 64);
        assert_eq!(QUAD_ATTRS[5].offset, 72);
    }

    #[test]
    fn shader_locations_are_contiguous_from_zero() {
        for attrs in [&QUAD_ATTRS[..], &GLYPH_ATTRS[..], &MESH_ATTRS[..]] {
            for (i, a) in attrs.iter().enumerate() {
                assert_eq!(a.shader_location, i as u32);
            }
        }
    }
}
