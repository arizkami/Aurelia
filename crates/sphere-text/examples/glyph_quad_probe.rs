//! Renders one line of text twice — distance field on the left, whatever the
//! automatic strategy picks with RGB subpixel coverage enabled on the right —
//! and writes the pair to a PNG.
//!
//! This is a probe, not a test: it needs system fonts, which CI does not have.
//! It exists so the claims in `docs/text.md` about which rasterisation path is
//! sharp at which size are a picture and a measurement rather than an assertion.
//!
//! **It re-implements `text.wgsl` and `text_subpixel.wgsl` on the CPU.** The
//! sampling, the median, the screen range, the coverage correction and the
//! per-channel dual-source blend below are transcriptions of the shaders, not
//! captures of what the GPU produced. It shows that the geometry and the path
//! choice handed to the GPU are right; it cannot prove the GPU agreed.
//!
//! It also transcribes the *blend*, which is the part that used to be wrong.
//! Compositing happens in linear light against an sRGB surface, so this walks
//! every pixel through `alpha_from_coverage`, a premultiplied linear blend and
//! an sRGB encode, in that order. An earlier version blended straight into sRGB
//! bytes, which is a gamma-space blend — the very thing the correction exists to
//! reproduce — so it showed text a good deal crisper than the GPU was drawing
//! and hid the calibration bug it was built to catch.
//!
//! ```bash
//! cargo run -p sphere-text --example glyph_quad_probe --release -- out.png
//! SPHERE_PROBE_SIZE=13 cargo run -p sphere-text --example glyph_quad_probe --release
//! SPHERE_PROBE_COMPARE=blend SPHERE_PROBE_ZOOM=8 cargo run -p sphere-text --example glyph_quad_probe --release
//! SPHERE_PROBE_COMPARE=fit SPHERE_PROBE_SIZE=13 cargo run -p sphere-text --example glyph_quad_probe --release
//! ```
//!
//! `SPHERE_PROBE_THEME=dark` flips both panels to light-on-dark, which is the
//! direction the correction has to mirror for.

use sphere_core::{Color, linear_to_srgb};
use sphere_render::batch::{GlyphPlacement, GlyphProvider, GlyphRequest};
use sphere_render::scene::{TextRasterMode, alpha_from_coverage, coverage_contrast_for};
use sphere_text::raster::{BITMAP_MAX_DEVICE_PX, choose_raster_strategy};
use sphere_text::{TextStyle, TextSystem};

const DEFAULT_TEXT: &str = "Handgloves 0123 Preferences";
const PAD: usize = 16;

/// The two colours a panel is drawn with, and the correction that pair implies.
///
/// Held together because the correction is a function of the pair: reading it
/// off one colour, or hard-coding it, is how the previous version of this probe
/// came to validate a number the renderer never used.
#[derive(Copy, Clone)]
struct Ink {
    text: Color,
    ground: Color,
    contrast: f32,
}

impl Ink {
    fn new(text: Color, ground: Color) -> Self {
        Self { text, ground, contrast: coverage_contrast_for(text, ground) }
    }

    /// The same pair with the correction disabled, for the comparison panel.
    fn uncorrected(self) -> Self {
        Self { contrast: 1.0, ..self }
    }
}

/// A linear-light image, encoded to sRGB only on the way out.
///
/// The surface Sphere renders into is sRGB, so the hardware decodes, blends and
/// re-encodes. Holding linear floats here and encoding once at the end is that
/// same arrangement, and it is the only way a CPU transcription of the shader
/// can show what the shader produces.
struct Canvas {
    width: usize,
    height: usize,
    pixels: Vec<[f32; 3]>,
}

impl Canvas {
    fn new(width: usize, height: usize, ground: Color) -> Self {
        let g = ground.to_linear();
        Self { width, height, pixels: vec![[g.r, g.g, g.b]; width * height] }
    }

    /// Premultiplied source-over, exactly the blend state the glyph pipeline
    /// binds (`BlendState::PREMULTIPLIED_ALPHA_BLENDING`).
    fn blend(&mut self, x: usize, y: usize, color: Color, alpha: f32) {
        if x >= self.width || y >= self.height || alpha <= 0.0 {
            return;
        }
        let src = color.to_linear();
        let dst = &mut self.pixels[y * self.width + x];
        let a = (src.a * alpha).clamp(0.0, 1.0);
        for (k, s) in [src.r, src.g, src.b].into_iter().enumerate() {
            dst[k] = s * alpha + dst[k] * (1.0 - a);
        }
    }

    /// The dual-source equation used by `text_subpixel.wgsl`:
    /// `foreground + destination * (1 - coverage)` independently for R, G and
    /// B. `Color::to_linear` is premultiplied, so the destination factor also
    /// includes the text alpha exactly as source one does in the shader.
    fn blend_subpixel(&mut self, x: usize, y: usize, color: Color, coverage: [f32; 3]) {
        if x >= self.width || y >= self.height {
            return;
        }
        let src = color.to_linear();
        let dst = &mut self.pixels[y * self.width + x];
        for (k, (s, c)) in [src.r, src.g, src.b].into_iter().zip(coverage).enumerate() {
            let c = c.clamp(0.0, 1.0);
            dst[k] = s * c + dst[k] * (1.0 - src.a * c);
        }
    }

    /// Nearest-neighbour magnification applied *after* the encode, so the pixels
    /// shown are exactly the pixels rendered. Any smooth resample would hide the
    /// very thing this probe exists to show.
    fn into_rgba(self, zoom: usize) -> (Vec<u8>, u32, u32) {
        let zoom = zoom.max(1);
        let (w, h) = (self.width * zoom, self.height * zoom);
        let mut out = vec![255u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let src = self.pixels[(y / zoom) * self.width + (x / zoom)];
                let dst = (y * w + x) * 4;
                for k in 0..3 {
                    out[dst + k] = (linear_to_srgb(src[k].clamp(0.0, 1.0)) * 255.0 + 0.5) as u8;
                }
            }
        }
        (out, w as u32, h as u32)
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn zoom() -> usize {
    env("SPHERE_PROBE_ZOOM").and_then(|v| v.parse().ok()).unwrap_or(1).clamp(1, 16)
}

/// The colour pair to draw, from `SPHERE_PROBE_THEME`.
///
/// Light by default: dark-on-light is the direction whose correction is the
/// mirrored branch, and therefore the one worth looking at first.
fn ink() -> Ink {
    match env("SPHERE_PROBE_THEME").as_deref() {
        Some("dark") => Ink::new(Color::hex(0xE6E9EF), Color::hex(0x14161A)),
        _ => Ink::new(Color::hex(0x1A1D22), Color::hex(0xF5F6F8)),
    }
}

fn save(path_default: &str, rgba: &[u8], w: u32, h: u32, caption: &str) {
    let path = std::env::args().nth(1).unwrap_or_else(|| path_default.into());
    match image::save_buffer(&path, rgba, w, h, image::ExtendedColorType::Rgba8) {
        Ok(()) => println!("\nwrote {path} — {caption}"),
        Err(e) => println!("\ncould not write {path}: {e}"),
    }
}

fn main() {
    let source = env("SPHERE_PROBE_TEXT").unwrap_or_else(|| DEFAULT_TEXT.to_string());
    let family = env("SPHERE_PROBE_FONT");
    let size: f32 = env("SPHERE_PROBE_SIZE").and_then(|v| v.parse().ok()).unwrap_or(13.0);

    let mut text = TextSystem::with_system_fonts();
    let style = TextStyle {
        font_size: sphere_core::Px(size),
        font: match &family {
            Some(name) => sphere_text::FontRequest::family(name.clone()),
            None => Default::default(),
        },
        ..Default::default()
    };
    let layout = text.layout(&source, &style, None);
    let Some(line) = layout.lines.first().cloned() else {
        println!("nothing shaped; no system fonts?");
        return;
    };
    let baseline = line.baseline.get();
    let ink = ink();

    println!("system ui font: {}", sphere_text::system_ui::family());
    println!(
        "resolved:       {}",
        line.runs.first().and_then(|r| text.fonts().family_name(r.font)).unwrap_or("<none>")
    );
    let strategy =
        choose_raster_strategy(sphere_core::Px(size), sphere_core::ScaleFactor::IDENTITY);
    println!("size:           {size} device px");
    println!(
        "field minified: {:.2}x  (a bilinear tap covers 2.00x)",
        sphere_text::mtsdf::DEFAULT_EM_SIZE_PX / size
    );
    println!("stem approx:    {:.2} px, of which {:.2} px is solid", size / 9.0, size / 9.0 - 1.0);
    println!("threshold:      {BITMAP_MAX_DEVICE_PX} device px, so Auto picks {strategy:?}");
    println!("coverage:       contrast {:+.3} for this colour pair", ink.contrast);

    match env("SPHERE_PROBE_COMPARE").as_deref() {
        // Both panels are bitmaps, the left one rasterised without a grid fit
        // and the right one with. Rasterised directly rather than through the
        // atlas, because the atlas has no way to hand back an unfitted glyph —
        // fitting is not optional there.
        Some("fit") => compare_fit(&source, &style, size, ink),
        // Both panels take whatever path `Auto` picks; only the coverage
        // correction differs. This is the one that shows what the linear blend
        // does to an uncorrected edge.
        Some("blend") => compare_blend(&mut text, &line, baseline, size, ink),
        _ => compare_raster(&mut text, &line, baseline, size, ink),
    }
}

/// Resolves every glyph in a line through the atlas, in one raster mode.
fn place(
    text: &mut TextSystem,
    line: &sphere_text::TextLine,
    mode: TextRasterMode,
    subpixel: bool,
) -> Vec<(f32, GlyphPlacement)> {
    let mut placed = Vec::new();
    for run in &line.runs {
        for g in &run.glyphs {
            // Mirror the batch compiler: bake the nearest quarter-pixel
            // remainder into the cached bitmap, then draw that bitmap at its
            // integral pen. Distance fields ignore the phase and keep the
            // original fractional pen.
            let quarters = (g.position.x.get() * 4.0).round() as i32;
            let bitmap_pen_x = quarters.div_euclid(4) as f32;
            let subpixel_phase = quarters.rem_euclid(4) as u8;
            let Some(p) = text.place_glyph(GlyphRequest {
                font: run.font,
                glyph: g.glyph,
                font_size: run.font_size,
                device_scale: 1.0,
                mode,
                subpixel,
                subpixel_phase,
            }) else {
                continue;
            };
            if p.uv[2] <= p.uv[0] || p.uv[3] <= p.uv[1] {
                // A zero-area placement is a space or a control character.
                continue;
            }
            placed.push((if p.is_bitmap { bitmap_pen_x } else { g.position.x.get() }, p));
        }
    }
    placed.sort_by(|a, b| a.0.total_cmp(&b.0));
    placed
}

/// Lays two panels side by side, each drawn by `draw`, with a hairline between.
fn two_panels(
    width: usize,
    height: usize,
    ground: Color,
    mut draw: impl FnMut(&mut Canvas, usize, usize),
) -> Canvas {
    let mut canvas = Canvas::new(width * 2, height, ground);
    for half in 0..2 {
        draw(&mut canvas, half, half * width);
    }
    let rule = Color::hex(0x808080).to_linear();
    for y in 0..height {
        canvas.pixels[y * width * 2 + width] = [rule.r, rule.g, rule.b];
    }
    canvas
}

/// Forced distance field against whatever `Auto` picks: the size question.
fn compare_raster(
    text: &mut TextSystem,
    line: &sphere_text::TextLine,
    baseline: f32,
    size: f32,
    ink: Ink,
) {
    let panels = [
        place(text, line, TextRasterMode::Mtsdf, false),
        place(text, line, TextRasterMode::Auto, true),
    ];
    let subpixel_glyphs = panels[1].iter().filter(|(_, p)| p.is_subpixel).count();
    println!("subpixel:        {subpixel_glyphs} RGB glyphs in the right panel");
    let width = line.width.get().ceil() as usize + PAD * 2;
    let height = (baseline * 2.0).ceil() as usize + PAD * 2;

    let canvas = two_panels(width, height, ink.ground, |canvas, half, x_offset| {
        for (pen_x, p) in &panels[half] {
            draw_glyph(
                canvas,
                x_offset,
                text,
                p,
                *pen_x + PAD as f32,
                baseline + PAD as f32,
                size,
                ink,
            );
        }
    });

    let (rgba, w, h) = canvas.into_rgba(zoom());
    save(
        "glyph_quad_probe.png",
        &rgba,
        w,
        h,
        "left: forced MTSDF, right: Auto with RGB subpixel coverage",
    );
}

/// Uncorrected coverage against corrected: the blend-space question.
///
/// Both panels take the same rasterisation path, so every difference on screen
/// is the coverage correction alone. The left panel is what a physically linear
/// blend produces from raw coverage, which is what this engine drew before
/// `coverage_contrast_for` was calibrated against the sRGB encode rather than
/// guessed at.
fn compare_blend(
    text: &mut TextSystem,
    line: &sphere_text::TextLine,
    baseline: f32,
    size: f32,
    ink: Ink,
) {
    let placed = place(text, line, TextRasterMode::Auto, true);
    let width = line.width.get().ceil() as usize + PAD * 2;
    let height = (baseline * 2.0).ceil() as usize + PAD * 2;
    let inks = [ink.uncorrected(), ink];

    let canvas = two_panels(width, height, ink.ground, |canvas, half, x_offset| {
        for (pen_x, p) in &placed {
            draw_glyph(
                canvas,
                x_offset,
                text,
                p,
                *pen_x + PAD as f32,
                baseline + PAD as f32,
                size,
                inks[half],
            );
        }
    });

    let (rgba, w, h) = canvas.into_rgba(zoom());
    save(
        "blend_probe.png",
        &rgba,
        w,
        h,
        "left: raw coverage, right: corrected for the linear blend",
    );
}

/// Composites one glyph, sampling the atlas the way `text.wgsl` does.
#[allow(clippy::too_many_arguments)]
fn draw_glyph(
    canvas: &mut Canvas,
    x_offset: usize,
    text: &TextSystem,
    p: &GlyphPlacement,
    pen_x: f32,
    baseline: f32,
    size: f32,
    ink: Ink,
) {
    let atlas = text.atlas();
    let Some(format) = atlas.page_format(p.page) else { return };
    let Some(pixels) = atlas.page_pixels(p.page) else { return };
    let page = atlas.page_size() as usize;
    let bpp = format.bytes_per_pixel();

    // The quad covers the ink box outset by the field's range. `range_em` is
    // zero on the bitmap path, so this is unconditional.
    let pad = p.range_em * size;
    let mut x0 = pen_x + p.bounds_em[0] * size - pad;
    let mut y0 = baseline + p.bounds_em[1] * size - pad;
    let mut qw = p.bounds_em[2] * size + 2.0 * pad;
    let mut qh = p.bounds_em[3] * size + 2.0 * pad;
    if p.is_bitmap {
        // Snapped exactly as the batch compiler snaps it: origin onto the pixel
        // grid, extent to the texel count. That 1:1 alignment is most of why
        // this path is sharp at small sizes.
        x0 = x0.round();
        y0 = y0.round();
        qw = p.texel_size[0] as f32;
        qh = p.texel_size[1] as f32;
    }
    if qw <= 0.0 || qh <= 0.0 {
        return;
    }
    let screen_range = (p.range_em * size).max(1.0);

    let y_lo = y0.floor().max(0.0) as usize;
    let y_hi = (y0 + qh).ceil().max(0.0) as usize;
    let x_lo = x0.floor().max(0.0) as usize;
    let x_hi = (x0 + qw).ceil().max(0.0) as usize;

    for py in y_lo..y_hi {
        for px in x_lo..x_hi {
            let fx = (px as f32 + 0.5 - x0) / qw;
            let fy = (py as f32 + 0.5 - y0) / qh;
            if !(0.0..1.0).contains(&fx) || !(0.0..1.0).contains(&fy) {
                continue;
            }
            let u = (p.uv[0] + (p.uv[2] - p.uv[0]) * fx) * page as f32 - 0.5;
            let v = (p.uv[1] + (p.uv[3] - p.uv[1]) * fy) * page as f32 - 0.5;
            let s = bilinear(pixels, page, bpp, u, v);

            if p.is_subpixel {
                let coverage = [
                    alpha_from_coverage(s[0], ink.contrast),
                    alpha_from_coverage(s[1], ink.contrast),
                    alpha_from_coverage(s[2], ink.contrast),
                ];
                canvas.blend_subpixel(px + x_offset, py, ink.text, coverage);
                continue;
            }

            let coverage = if p.is_bitmap {
                s[0]
            } else {
                let sd = median3(s[0], s[1], s[2]) - 0.5;
                (sd * screen_range + 0.5).clamp(0.0, 1.0)
            };
            canvas.blend(px + x_offset, py, ink.text, alpha_from_coverage(coverage, ink.contrast));
        }
    }
}

/// Bilinear sample of an atlas page, matching the sampler the glyph pass binds.
fn bilinear(pixels: &[u8], page: usize, bpp: usize, u: f32, v: f32) -> [f32; 4] {
    let (x0, y0) = (u.floor(), v.floor());
    let (fx, fy) = (u - x0, v - y0);
    let texel = |x: f32, y: f32| -> [f32; 4] {
        let xi = (x as isize).clamp(0, page as isize - 1) as usize;
        let yi = (y as isize).clamp(0, page as isize - 1) as usize;
        let i = (yi * page + xi) * bpp;
        if i + bpp > pixels.len() {
            return [0.0; 4];
        }
        let mut out = [0.0f32; 4];
        for (k, slot) in out.iter_mut().enumerate().take(bpp) {
            *slot = pixels[i + k] as f32 / 255.0;
        }
        out
    };
    let (a, b, c, d) =
        (texel(x0, y0), texel(x0 + 1.0, y0), texel(x0, y0 + 1.0), texel(x0 + 1.0, y0 + 1.0));
    let mut out = [0.0f32; 4];
    for k in 0..4 {
        let top = a[k] + (b[k] - a[k]) * fx;
        let bottom = c[k] + (d[k] - c[k]) * fx;
        out[k] = top + (bottom - top) * fy;
    }
    out
}

fn median3(a: f32, b: f32, c: f32) -> f32 {
    a.max(b).min(a.min(b).max(c))
}

/// Renders unfitted against fitted bitmaps, straight from the rasteriser.
fn compare_fit(source: &str, style: &TextStyle, size: f32, ink: Ink) {
    use sphere_text::font::FontDatabase;
    use sphere_text::hint::{GridFit, VerticalZones};
    use sphere_text::mtsdf::extract_shape;
    use sphere_text::raster::{rasterize_glyph, rasterize_shape};

    let mut db = FontDatabase::with_system_fonts();
    let Some(font) = db.resolve(&style.font) else {
        println!("no face for that request");
        return;
    };
    println!("comparing unfitted against fitted bitmaps at {size} px");

    // (unfitted, fitted, ink-left in em) per character.
    let mut cells: Vec<(sphere_text::GlyphImage, sphere_text::GlyphImage, f32)> = Vec::new();
    let mut pen = 0.0f32;
    for c in source.chars() {
        let Some(glyph) = db.glyph_index(font, c) else { continue };
        let advance = db
            .with_face(font, |face| {
                let upem = (face.units_per_em() as f32).max(1.0);
                face.glyph_hor_advance(ttf_parser::GlyphId(glyph.0)).map(|a| f32::from(a) / upem)
            })
            .flatten()
            .unwrap_or(0.5);
        let pair = db.with_face(font, |face| {
            let shape = extract_shape(face, glyph).ok()?;
            let plain = rasterize_shape(&shape, size);
            let fitted = rasterize_glyph(face, glyph, size).ok()?;
            VerticalZones::from_face(face).and_then(|z| GridFit::new(&z, size))?;
            Some((plain, fitted))
        });
        if let Some(Some((plain, fitted))) = pair {
            cells.push((plain, fitted, pen));
        }
        pen += advance;
    }
    if cells.is_empty() {
        println!("nothing to fit at this size");
        return;
    }

    let width = (pen * size).ceil() as usize + PAD * 2;
    let height = (size * 2.2).ceil() as usize + PAD * 2;
    let baseline = (size * 1.4) as i32 + PAD as i32;

    let canvas = two_panels(width, height, ink.ground, |canvas, half, x_offset| {
        for (plain, fitted, pen_em) in &cells {
            let image = if half == 0 { plain } else { fitted };
            let ox = (pen_em * size).round() as i32 + PAD as i32 + x_offset as i32;
            let oy = baseline + (image.bounds_em.origin.y * size).round() as i32;
            for row in 0..image.height as i32 {
                for col in 0..image.width as i32 {
                    let (x, y) = (ox + col, oy + row);
                    if x < 0 || y < 0 {
                        continue;
                    }
                    let coverage =
                        f32::from(image.data[(row * image.width as i32 + col) as usize]) / 255.0;
                    canvas.blend(
                        x as usize,
                        y as usize,
                        ink.text,
                        alpha_from_coverage(coverage, ink.contrast),
                    );
                }
            }
        }
    });

    let (rgba, w, h) = canvas
        .into_rgba(env("SPHERE_PROBE_ZOOM").and_then(|v| v.parse().ok()).unwrap_or(4).clamp(1, 16));
    save("grid_fit.png", &rgba, w, h, "left: unfitted, right: vertically grid-fitted");
}
