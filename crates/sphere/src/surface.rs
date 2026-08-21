//! A rendered window surface: everything wired together.
//!
//! Each subsystem is independently usable, and this is what it looks like when
//! they are all used at once. A [`SphereSurface`] owns the GPU backend, the UI
//! tree, the text system, the image cache and the batch compiler for one
//! window, and turns "here is my root element" into a presented frame.
//!
//! ## The frame, in order
//!
//! ```text
//! build         reconcile elements against last frame's nodes
//! layout        skipped entirely when nothing is layout-dirty
//! paint         walk once, cull, record into a Scene
//! upload        push atlas dirty regions and new image textures
//! compile       cull, batch, tessellate -> CompiledFrame
//! render        RendererBackend
//! ```
//!
//! Nothing here is mandatory. An application that wants its own frame loop can
//! drive [`sphere_ui::UiTree`] and [`sphere_wgpu::WgpuRenderer`] directly; this
//! type exists so that the common case is short.

use rustc_hash::FxHashMap;
use sphere_core::{Color, DevicePx, ImageId, InitError, RenderError, ScaleFactor, Size, TextureId};
use sphere_image::ImageCache;
use sphere_render::{
    BatchCompiler, Canvas, FrameStats, PresentPreference, RendererBackend, Scene, SurfaceConfig,
    TextureProvider, VsyncMode,
};
use sphere_text::{GlyphFormat, TextSystem};
use sphere_ui::{AnyElement, DispatchResult, Theme, UiEvent, UiTree};
use sphere_wgpu::WgpuRenderer;
use std::sync::Arc;

/// How a surface should be created.
#[derive(Copy, Clone, Debug)]
pub struct SurfaceOptions {
    /// Latency preference.
    pub present: PresentPreference,
    /// Vsync behaviour.
    pub vsync: VsyncMode,
    /// Whether the window composites with what is behind it.
    pub transparent: bool,
    /// Byte budget for decoded images.
    pub image_budget_bytes: usize,
    /// Multisample count for tessellated path geometry.
    ///
    /// Quads and glyphs are antialiased analytically and gain nothing from it;
    /// paths have hard triangle edges and nothing else smooths them. `1`
    /// disables it, `4` is the default and falls back automatically on an
    /// adapter that cannot manage it.
    pub msaa_samples: u32,
    /// Whether to load the platform's fonts at start-up.
    ///
    /// Scanning the system font directory takes a noticeable fraction of a
    /// second, so a plug-in that ships its own font should turn this off.
    pub load_system_fonts: bool,
}

impl Default for SurfaceOptions {
    fn default() -> Self {
        Self {
            present: PresentPreference::LowLatency,
            vsync: VsyncMode::On,
            transparent: false,
            image_budget_bytes: 64 * 1024 * 1024,
            msaa_samples: sphere_render::DEFAULT_MSAA_SAMPLES,
            load_system_fonts: true,
        }
    }
}

/// Per-frame counters, for the diagnostics overlay.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct SurfaceStats {
    /// What the GPU backend reported.
    pub frame: FrameStats,
    /// What the UI tree reported.
    pub tree: sphere_ui::TreeStats,
    /// Nodes the layout engine actually laid out. Zero on a paint-only frame.
    pub nodes_laid_out: u32,
    /// Milliseconds spent building, laying out and painting.
    pub cpu_ms: f32,
    /// Atlas texels uploaded this frame.
    pub glyph_texels_uploaded: u64,
}

/// Where start-up time went.
///
/// Start-up is the one part of the frame budget that cannot be amortised, and
/// it is the part a user sees as "the window took a moment to appear". Reported
/// rather than guessed at, because the two costs below are very different
/// things and only one of them is avoidable.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct InitTiming {
    /// Adapter selection, device creation and the first surface configuration.
    pub gpu_ms: f32,
    /// Scanning and indexing the platform's fonts.
    ///
    /// Usually the larger of the two. [`SurfaceOptions::load_system_fonts`]
    /// turns it off for an application that ships its own faces.
    pub fonts_ms: f32,
}

impl InitTiming {
    /// Total start-up cost in milliseconds.
    #[inline]
    pub fn total_ms(&self) -> f32 {
        self.gpu_ms + self.fonts_ms
    }
}

/// Resolves image handles to GPU textures for the batch compiler.
///
/// A separate type because the compiler takes `&mut dyn TextureProvider` and
/// the surface cannot hand out a second mutable borrow of itself while it is
/// already holding one for the scene.
struct ImageTextures<'a> {
    cache: &'a ImageCache,
}

impl TextureProvider for ImageTextures<'_> {
    fn texture_for(&mut self, image: ImageId) -> Option<TextureId> {
        self.cache.texture(image)
    }
}

/// One window's worth of Sphere.
pub struct SphereSurface {
    renderer: WgpuRenderer,
    compiler: BatchCompiler,
    scene: Scene,
    text: TextSystem,
    images: ImageCache,
    tree: UiTree,
    size: Size<DevicePx>,
    scale_factor: ScaleFactor,
    /// Multisample count requested at construction.
    msaa_samples: u32,
    /// GPU texture for each glyph atlas page.
    atlas_textures: FxHashMap<u32, TextureId>,
    /// The atlas generation the textures were built against. A bump means every
    /// placement was invalidated and the textures must be rebuilt.
    atlas_generation: u64,
    /// Scratch for expanding grayscale atlas rows to RGBA, reused per frame.
    expand_scratch: Vec<u8>,
    /// Images loaded through this surface that have no GPU texture yet.
    pending_uploads: Vec<ImageId>,
    stats: SurfaceStats,
    start: std::time::Instant,
    /// Where start-up time went.
    init_timing: InitTiming,
    /// Whether a frame has actually reached the screen.
    ///
    /// Distinct from "render was called": `render` returns `Ok(None)` for a
    /// zero-area viewport and for a transiently unavailable surface, and
    /// revealing a window on either would show the blank frame that creating it
    /// hidden was meant to avoid.
    presented: bool,
}

impl SphereSurface {
    /// Creates a surface for a window.
    ///
    /// `window` must implement the `raw-window-handle` traits and outlive the
    /// surface; an [`Arc`] is the usual way to guarantee both.
    pub async fn new<W>(
        window: Arc<W>,
        size: Size<DevicePx>,
        scale_factor: ScaleFactor,
        options: SurfaceOptions,
    ) -> Result<Self, InitError>
    where
        W: sphere_wgpu::wgpu::WasmNotSendSync
            + raw_window_handle::HasWindowHandle
            + raw_window_handle::HasDisplayHandle
            + 'static,
    {
        let gpu_start = std::time::Instant::now();
        let renderer = WgpuRenderer::new(
            window,
            size,
            scale_factor,
            options.present,
            options.vsync,
            options.transparent,
            options.msaa_samples,
        )
        .await?;
        let gpu_ms = gpu_start.elapsed().as_secs_f32() * 1000.0;

        let fonts_start = std::time::Instant::now();
        let mut text = if options.load_system_fonts {
            TextSystem::with_system_fonts()
        } else {
            TextSystem::new()
        };
        let fonts_ms = fonts_start.elapsed().as_secs_f32() * 1000.0;
        // The atlas must not be configured larger than the adapter can hold, or
        // the failure appears at upload time rather than at allocation time.
        text.set_max_texture_size(renderer.capabilities().max_texture_size);

        let viewport =
            Size::new(scale_factor.to_logical(size.width), scale_factor.to_logical(size.height));

        Ok(Self {
            renderer,
            compiler: BatchCompiler::new(),
            scene: Scene::new(viewport, scale_factor),
            text,
            images: ImageCache::new(options.image_budget_bytes),
            tree: UiTree::new(),
            size,
            scale_factor,
            msaa_samples: options.msaa_samples,
            atlas_textures: FxHashMap::default(),
            atlas_generation: 0,
            expand_scratch: Vec::new(),
            pending_uploads: Vec::new(),
            stats: SurfaceStats::default(),
            start: std::time::Instant::now(),
            init_timing: InitTiming { gpu_ms, fonts_ms },
            presented: false,
        })
    }

    /// The adapter this surface is running on, for diagnostics.
    #[inline]
    pub fn adapter_name(&self) -> &str {
        self.renderer.adapter_name()
    }

    /// Where start-up time went.
    #[inline]
    pub fn init_timing(&self) -> InitTiming {
        self.init_timing
    }

    /// Where the focused editable element wants the platform's input method.
    ///
    /// Valid after [`SphereSurface::render`], because the caret's position is a
    /// paint-time fact. Apply it with
    /// [`sphere_platform::Window::set_ime_allowed`] and
    /// [`sphere_platform::Window::set_ime_cursor_area`]; `None` means nothing
    /// focused accepts text and the input method should be switched off.
    #[inline]
    pub fn ime(&self) -> Option<sphere_ui::ImeArea> {
        self.tree.ime()
    }

    /// Whether a frame has reached the screen yet.
    ///
    /// The condition for revealing a window that was created hidden. Waiting on
    /// this rather than on "`render` returned" is the difference between showing
    /// a painted window and showing the blank one that opening hidden was meant
    /// to avoid: `render` reports `Ok(None)` for a zero-area viewport and for a
    /// surface that is transiently unavailable, and neither has drawn anything.
    #[inline]
    pub fn has_presented(&self) -> bool {
        self.presented
    }

    /// The UI tree.
    #[inline]
    pub fn tree(&self) -> &UiTree {
        &self.tree
    }

    /// The UI tree, mutably.
    #[inline]
    pub fn tree_mut(&mut self) -> &mut UiTree {
        &mut self.tree
    }

    /// The text system, for loading fonts.
    #[inline]
    pub fn text_mut(&mut self) -> &mut TextSystem {
        &mut self.text
    }

    /// The image cache, for inspection.
    #[inline]
    pub fn images(&self) -> &ImageCache {
        &self.images
    }

    /// Decodes an image and queues it for GPU upload on the next frame.
    ///
    /// Loading through the surface rather than through the cache directly is
    /// what lets the upload be automatic: the cache is content-addressed and
    /// has no notion of which entries the GPU has seen.
    pub fn load_image(&mut self, bytes: &[u8]) -> Result<ImageId, sphere_core::ImageError> {
        let id = self.images.load_bytes(bytes)?;
        if self.images.texture(id).is_none() && !self.pending_uploads.contains(&id) {
            self.pending_uploads.push(id);
        }
        Ok(id)
    }

    /// Registers RGBA8 pixels as an image and queues the upload.
    pub fn load_rgba8(
        &mut self,
        width: u32,
        height: u32,
        rgba: &[u8],
        alpha: sphere_image::AlphaMode,
    ) -> Result<ImageId, sphere_core::ImageError> {
        let id = self.images.load_rgba8(width, height, rgba, alpha)?;
        if self.images.texture(id).is_none() && !self.pending_uploads.contains(&id) {
            self.pending_uploads.push(id);
        }
        Ok(id)
    }

    /// Replaces the theme. Marks paint-dirty, never layout-dirty.
    #[inline]
    pub fn set_theme(&mut self, theme: Theme) {
        self.tree.set_theme(theme);
    }

    /// Counters from the most recent frame.
    #[inline]
    pub fn stats(&self) -> SurfaceStats {
        self.stats
    }

    /// The viewport in logical pixels.
    #[inline]
    pub fn viewport(&self) -> Size<Px> {
        Size::new(
            self.scale_factor.to_logical(self.size.width),
            self.scale_factor.to_logical(self.size.height),
        )
    }

    /// Reconfigures after a resize or a DPI change.
    ///
    /// A zero extent is accepted and ignored: a minimised window reports zero
    /// and that is not an error.
    pub fn resize(
        &mut self,
        size: Size<DevicePx>,
        scale_factor: ScaleFactor,
    ) -> Result<(), RenderError> {
        self.size = size;
        self.scale_factor = scale_factor;
        self.renderer.configure_surface(SurfaceConfig {
            size,
            scale_factor,
            present: PresentPreference::LowLatency,
            vsync: VsyncMode::On,
            transparent: false,
            msaa_samples: self.msaa_samples,
        })
    }

    /// Dispatches an input event to the UI tree.
    #[inline]
    pub fn dispatch(&mut self, event: &UiEvent) -> DispatchResult {
        // The text-aware form: a text field turns a click into a caret index by
        // shaping the string, and the shaping cache makes that a lookup rather
        // than work.
        self.tree.dispatch_with_text(event, &mut self.text)
    }

    /// True when a repaint is needed.
    #[inline]
    pub fn needs_paint(&self) -> bool {
        self.tree.needs_paint()
    }

    /// Builds, lays out, paints and presents one frame.
    ///
    /// Returns `Ok(None)` when the frame was skipped for a recoverable reason —
    /// a minimised window, a transient acquisition failure. That is a normal
    /// outcome, not an error, and the caller should simply try again.
    pub fn render(
        &mut self,
        root: AnyElement,
        clear: Color,
    ) -> Result<Option<SurfaceStats>, RenderError> {
        let cpu_start = std::time::Instant::now();
        let viewport = self.viewport();
        if viewport.is_empty() {
            return Ok(None);
        }
        let time = self.start.elapsed().as_secs_f32();

        self.text.begin_frame();
        self.images.begin_frame();

        self.tree.build(root);
        self.tree
            .compute_layout_with_text(viewport, &mut self.text)
            .map_err(|e| RenderError::Backend(format!("layout failed: {e}")))?;

        self.scene.reset(viewport, self.scale_factor);
        {
            let mut canvas = Canvas::new(&mut self.scene);
            self.tree.paint(&mut canvas, &mut self.text, viewport, time);
        }

        // Glyphs are rasterised during compilation, so the atlas has to be
        // uploaded *after* the compiler has run, not before. Compile first,
        // upload, then draw.
        self.compiler.set_time(time);
        {
            let mut textures = ImageTextures { cache: &self.images };
            self.compiler.compile(&self.scene, &mut self.text, &mut textures);
        }
        let texels = self.upload_atlas()?;
        self.upload_pending_images()?;

        let cpu_ms = cpu_start.elapsed().as_secs_f32() * 1000.0;

        let mut handle = match self.renderer.begin_frame() {
            Ok(h) => h,
            Err(e) if e.is_transient() => return Ok(None),
            Err(e) if e.needs_reconfigure() => {
                self.resize(self.size, self.scale_factor)?;
                return Ok(None);
            }
            Err(e) => return Err(RenderError::Surface(e)),
        };

        // The compiled frame is borrowed from the compiler, which the atlas
        // upload above no longer touches.
        let compiled = self.compiler.frame().clone();
        self.renderer.render(&mut handle, &compiled, clear)?;
        let frame_stats = self.renderer.end_frame(handle)?;
        self.presented = true;

        self.tree.end_frame();
        self.stats = SurfaceStats {
            frame: frame_stats,
            tree: self.tree.stats(),
            nodes_laid_out: self.tree.stats().nodes_laid_out,
            cpu_ms,
            glyph_texels_uploaded: texels,
        };
        Ok(Some(self.stats))
    }

    /// Pushes changed atlas regions to the GPU.
    ///
    /// Returns how many texels were uploaded, which is the number to watch when
    /// text performance looks wrong: in steady state it should fall to zero
    /// once every glyph on screen has been rasterised once.
    fn upload_atlas(&mut self) -> Result<u64, RenderError> {
        // A generation bump means every placement was invalidated — a config
        // change, a font swap — so the GPU copies are stale wholesale.
        if self.text.atlas().generation() != self.atlas_generation {
            for (_, texture) in self.atlas_textures.drain() {
                self.renderer.destroy_texture(texture);
            }
            self.atlas_generation = self.text.atlas().generation();
        }

        // Make sure every page has a texture before draining the regions, since
        // draining borrows the atlas immutably for the loop's duration.
        let page_size = self.text.atlas().page_size();
        let pages: Vec<(u32, GlyphFormat)> = (0..self.text.atlas().page_count() as u32)
            .filter_map(|p| self.text.atlas().page_format(p).map(|f| (p, f)))
            .collect();
        for (page, _) in &pages {
            if !self.atlas_textures.contains_key(page) {
                let blank = vec![0u8; (page_size as usize) * (page_size as usize) * 4];
                let id = self.renderer.upload_texture(None, page_size, page_size, &blank)?;
                self.atlas_textures.insert(*page, id);
                self.renderer.textures_mut().set_atlas_page(*page, id);
            }
        }

        let mut uploaded = 0u64;
        // Collect first: the regions borrow the atlas, and the upload needs a
        // mutable borrow of the renderer, which also lives in `self`.
        let regions: Vec<(u32, GlyphFormat, sphere_core::Rect<u32>, u32, Vec<u8>)> = self
            .text
            .take_dirty_regions()
            .into_iter()
            .map(|r| (r.page, r.format, r.rect, r.bytes_per_row, r.data.to_vec()))
            .collect();

        for (page, format, rect, bytes_per_row, data) in regions {
            let Some(texture) = self.atlas_textures.get(&page).copied() else { continue };
            let width = rect.size.width;
            let height = rect.size.height;
            if width == 0 || height == 0 {
                continue;
            }

            // Grayscale pages are expanded to RGBA rather than given their own
            // texture format and bind group layout. A bitmap page is small and
            // rare — it only holds the sub-12-pixel fallback — so the 4x memory
            // costs far less than a second pipeline variant would.
            let rgba: &[u8] = match format {
                GlyphFormat::Mtsdf | GlyphFormat::ColorBitmap => &data,
                GlyphFormat::Grayscale => {
                    self.expand_scratch.clear();
                    self.expand_scratch.reserve((width as usize) * (height as usize) * 4);
                    for row in 0..height as usize {
                        let start = row * bytes_per_row as usize + rect.origin.x as usize;
                        let end = (start + width as usize).min(data.len());
                        for &coverage in &data[start.min(data.len())..end] {
                            // Coverage in red; the text shader reads `.r` for
                            // bitmap glyphs.
                            self.expand_scratch
                                .extend_from_slice(&[coverage, coverage, coverage, 255]);
                        }
                    }
                    &self.expand_scratch
                }
            };

            // For an RGBA page the region spans the full page width, so the
            // rows are already contiguous and start at x = 0.
            let x = match format {
                GlyphFormat::Grayscale => rect.origin.x,
                _ => 0,
            };
            let expected = (width as usize) * (height as usize) * 4;
            if rgba.len() < expected {
                continue;
            }
            self.renderer.upload_texture_region(
                texture,
                x,
                rect.origin.y,
                width,
                height,
                &rgba[..expected],
            )?;
            uploaded += (width as u64) * (height as u64);
        }
        Ok(uploaded)
    }

    /// Uploads images queued by [`SphereSurface::load_image`].
    fn upload_pending_images(&mut self) -> Result<(), RenderError> {
        if self.pending_uploads.is_empty() {
            return Ok(());
        }
        // Drain into a local so the cache can be borrowed while the renderer is
        // borrowed mutably.
        let pending = core::mem::take(&mut self.pending_uploads);
        for id in pending {
            let Some(image) = self.images.peek(id) else { continue };
            let (w, h) = image.dimensions();
            let data = image.data().to_vec();
            let texture = self.renderer.upload_texture(None, w, h, &data)?;
            self.images.set_texture(id, texture);
        }
        Ok(())
    }
}

use sphere_core::Px;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_timing_totals_its_parts() {
        let t = InitTiming { gpu_ms: 700.0, fonts_ms: 20.0 };
        assert_eq!(t.total_ms(), 720.0);
        assert_eq!(InitTiming::default().total_ms(), 0.0);
    }

    #[test]
    fn default_options_favour_latency() {
        // A plug-in editor whose knob lags the mouse feels broken, so the
        // default has to be the low-latency one even though it costs throughput.
        let o = SurfaceOptions::default();
        assert_eq!(o.present, PresentPreference::LowLatency);
        assert_eq!(o.present.max_frame_latency(), 1);
        assert!(o.load_system_fonts);
    }

    #[test]
    fn surface_stats_default_to_zero() {
        let s = SurfaceStats::default();
        assert_eq!(s.nodes_laid_out, 0);
        assert_eq!(s.glyph_texels_uploaded, 0);
        assert_eq!(s.frame.draw_calls, 0);
    }
}
