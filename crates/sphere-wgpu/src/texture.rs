//! GPU texture storage.
//!
//! Holds every texture the renderer owns: images, glyph atlas pages, and the
//! 1x1 white texture that lets untextured pipelines share one bind group layout
//! with textured ones. Without that white texture every solid-colour draw would
//! need either a second pipeline or a null-binding special case.

use rustc_hash::FxHashMap;
use sphere_core::{GenerationalStore, RenderError, TextureId};

struct Entry {
    texture: wgpu::Texture,
    #[allow(dead_code)]
    view: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
    bytes: u64,
}

/// Owns textures and their bind groups.
pub struct TextureStore {
    entries: GenerationalStore<TextureId, Entry>,
    /// Glyph atlas pages, keyed by page index rather than by handle so the text
    /// stack can address them positionally.
    atlas_pages: FxHashMap<u32, TextureId>,
    white: Option<TextureId>,
    sampler: wgpu::Sampler,
    nearest_sampler: wgpu::Sampler,
    bytes: u64,
    /// Region uploads queued this frame, flushed before rendering.
    pending: Vec<(TextureId, u32, u32, u32, u32, Vec<u8>)>,
}

impl TextureStore {
    /// Creates an empty store with its shared samplers.
    pub fn new(device: &wgpu::Device, _queue: &wgpu::Queue) -> Self {
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("sphere.tex.linear"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        let nearest_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("sphere.tex.nearest"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        Self {
            entries: GenerationalStore::new(),
            atlas_pages: FxHashMap::default(),
            white: None,
            sampler,
            nearest_sampler,
            bytes: 0,
            pending: Vec::new(),
        }
    }

    /// The nearest-neighbour sampler, for pixel-exact image drawing.
    #[inline]
    pub fn nearest_sampler(&self) -> &wgpu::Sampler {
        &self.nearest_sampler
    }

    /// Ensures the 1x1 white texture exists and returns its bind group.
    ///
    /// Called on the render path, so it must never allocate after the first
    /// frame.
    pub fn ensure_white(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        layout: &wgpu::BindGroupLayout,
    ) {
        if self.white.is_some() {
            return;
        }
        let id = self
            .upload(device, queue, layout, None, 1, 1, &[255, 255, 255, 255])
            .expect("1x1 white texture");
        self.white = Some(id);
    }

    /// The bind group for the 1x1 white texture.
    ///
    /// # Panics
    ///
    /// Panics if [`TextureStore::ensure_white`] has not run. The renderer calls
    /// it during construction, so this is a programming error rather than a
    /// runtime condition.
    pub fn white_bind_group(&self) -> &wgpu::BindGroup {
        let id = self.white.expect("ensure_white must run before rendering");
        &self.entries.get(id).expect("white texture was destroyed").bind_group
    }

    /// The bind group for a texture handle, or `None` when it is stale.
    pub fn bind_group(&self, id: TextureId) -> Option<&wgpu::BindGroup> {
        self.entries.get(id).map(|e| &e.bind_group)
    }

    /// The bind group for a glyph atlas page.
    pub fn atlas_bind_group(&self, page: u32) -> Option<&wgpu::BindGroup> {
        let id = *self.atlas_pages.get(&page)?;
        self.bind_group(id)
    }

    /// Registers a texture as a glyph atlas page.
    pub fn set_atlas_page(&mut self, page: u32, id: TextureId) {
        self.atlas_pages.insert(page, id);
    }

    /// How many atlas pages are registered.
    #[inline]
    pub fn atlas_page_count(&self) -> usize {
        self.atlas_pages.len()
    }

    /// Total texture memory held, in bytes.
    #[inline]
    pub fn memory_bytes(&self) -> u64 {
        self.bytes
    }

    /// Creates or replaces a texture from RGBA8 pixels.
    #[allow(clippy::too_many_arguments)]
    pub fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        layout: &wgpu::BindGroupLayout,
        id: Option<TextureId>,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<TextureId, RenderError> {
        let expected = (width as usize) * (height as usize) * 4;
        if rgba.len() < expected {
            return Err(RenderError::Backend(format!(
                "texture upload is {} bytes but {width}x{height} RGBA needs {expected}",
                rgba.len()
            )));
        }

        // Replacing an existing texture of the same size reuses the allocation;
        // a glyph atlas page that grows would otherwise leak the old one.
        if let Some(existing) = id
            && let Some(e) = self.entries.get(existing)
            && e.width == width
            && e.height == height
        {
            write_texture(queue, &e.texture, 0, 0, width, height, &rgba[..expected]);
            return Ok(existing);
        }
        if let Some(existing) = id {
            self.destroy(existing);
        }

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("sphere.texture"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // Non-sRGB storage: glyph atlases hold distance fields, which are
            // geometry, not colour, and must not be gamma-decoded on sample.
            // Images carry their own sRGB handling in the shader.
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        write_texture(queue, &texture, 0, 0, width, height, &rgba[..expected]);

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sphere.texture_bind"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });

        let bytes = expected as u64;
        self.bytes += bytes;
        Ok(self.entries.insert(Entry { texture, view, bind_group, width, height, bytes }))
    }

    /// Queues a sub-rectangle upload, applied by [`TextureStore::flush`].
    ///
    /// Deferring lets the glyph atlas add many glyphs during scene building and
    /// pay for one flush, rather than interleaving uploads with recording.
    pub fn upload_region(
        &mut self,
        _queue: &wgpu::Queue,
        id: TextureId,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<(), RenderError> {
        let Some(e) = self.entries.get(id) else {
            return Err(RenderError::StaleResource("texture"));
        };
        if x + width > e.width || y + height > e.height {
            return Err(RenderError::Backend(format!(
                "region {width}x{height} at ({x},{y}) is outside a {}x{} texture",
                e.width, e.height
            )));
        }
        let expected = (width as usize) * (height as usize) * 4;
        if rgba.len() < expected {
            return Err(RenderError::Backend(format!(
                "region upload is {} bytes but needs {expected}",
                rgba.len()
            )));
        }
        self.pending.push((id, x, y, width, height, rgba[..expected].to_vec()));
        Ok(())
    }

    /// Applies every queued region upload.
    pub fn flush(&mut self, queue: &wgpu::Queue) {
        if self.pending.is_empty() {
            return;
        }
        // Drain into a local so the borrow of `self.pending` ends before the
        // loop needs `&self.entries`.
        let pending = core::mem::take(&mut self.pending);
        for (id, x, y, w, h, data) in &pending {
            if let Some(e) = self.entries.get(*id) {
                write_texture(queue, &e.texture, *x, *y, *w, *h, data);
            }
        }
        // Reuse the allocation for next frame.
        self.pending = pending;
        self.pending.clear();
    }

    /// Releases a texture.
    pub fn destroy(&mut self, id: TextureId) {
        if let Some(e) = self.entries.remove(id) {
            self.bytes = self.bytes.saturating_sub(e.bytes);
            e.texture.destroy();
        }
        self.atlas_pages.retain(|_, v| *v != id);
    }

    /// Number of live textures.
    #[inline]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no textures are held.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn write_texture(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    rgba: &[u8],
) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d { x, y, z: 0 },
            aspect: wgpu::TextureAspect::All,
        },
        rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width * 4),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
    );
}
