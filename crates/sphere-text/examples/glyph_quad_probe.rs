//! Renders one line of text twice — distance field on the left, whatever the
//! automatic strategy picks on the right — and writes the pair to a PNG.
//!
//! This is a probe, not a test: it needs system fonts, which CI does not have.
//! It exists so the claims in `docs/text.md` about which rasterisation path is
//! sharp at which size are a picture and a measurement rather than an assertion.
//!
//! **It re-implements `text.wgsl` on the CPU.** The sampling, the median, the
//! screen range and the coverage gamma below are transcriptions of the shader,
//! not captures of what the GPU produced. It shows that the geometry and the
//! path choice handed to the GPU are right; it cannot prove the GPU agreed.
//!
//! ```bash
//! cargo run -p sphere-text --example glyph_quad_probe --release -- out.png
//! SPHERE_PROBE_SIZE=13 cargo run -p sphere-text --example glyph_quad_probe --release
//! ```

use sphere_render::batch::{GlyphPlacement, GlyphProvider, GlyphRequest};
use sphere_render::scene::TextRasterMode;
use sphere_text::raster::{BITMAP_MAX_DEVICE_PX, choose_raster_strategy};
use sphere_text::{GlyphFormat, TextStyle, TextSystem};

const DEFAULT_TEXT: &str = "Handgloves 0123 Preferences";
const PAD: usize = 16;

/// Dark text on a light ground, so the coverage correction runs downward. The
/// same number `coverage_gamma_for` produces for black on white.
const GAMMA: f32 = 0.8;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
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

    // Both panels resolved before anything borrows the atlas pixels.
    let mut panels: Vec<Vec<(f32, GlyphPlacement)>> = Vec::new();
    for mode in [TextRasterMode::Mtsdf, TextRasterMode::Auto] {
        let mut placed = Vec::new();
        for run in &line.runs {
            for g in &run.glyphs {
                let Some(p) = text.place_glyph(GlyphRequest {
                    font: run.font,
                    glyph: g.glyph,
                    font_size: run.font_size,
                    device_scale: 1.0,
                    mode,
                }) else {
                    continue;
                };
                if p.uv[2] <= p.uv[0] || p.uv[3] <= p.uv[1] {
                    continue;
                }
                placed.push((g.position.x.get(), p));
            }
        }
        placed.sort_by(|a, b| a.0.total_cmp(&b.0));
        panels.push(placed);
    }

    let w = layout.size.width.get().ceil() as usize + PAD * 2;
    let h = (baseline * 2.0).ceil() as usize + PAD * 2;
    // Opaque white ground: dark-on-light, so the letterforms read as shapes
    // rather than as glowing edges.
    let mut canvas = vec![255u8; w * h * 2 * 4];

    for (half, placed) in panels.iter().enumerate() {
        for (pen_x, p) in placed {
            draw_glyph(
                &mut canvas,
                w * 2,
                h,
                half * w,
                &text,
                p,
                *pen_x + PAD as f32,
                baseline + PAD as f32,
                size,
            );
        }
    }

    // A hairline between the halves, so the two are unmistakably separate.
    for y in 0..h {
        let i = (y * w * 2 + w) * 4;
        canvas[i..i + 3].fill(200);
    }

    // Nearest-neighbour magnification, applied *after* compositing so the
    // pixels shown are exactly the pixels rendered. Any smooth resample would
    // hide the very thing this probe exists to show.
    let zoom: usize =
        env("SPHERE_PROBE_ZOOM").and_then(|v| v.parse().ok()).unwrap_or(1).clamp(1, 16);
    let (out_w, out_h) = (w * 2 * zoom, h * zoom);
    let canvas = if zoom == 1 {
        canvas
    } else {
        let mut big = vec![255u8; out_w * out_h * 4];
        for y in 0..out_h {
            for x in 0..out_w {
                let src = ((y / zoom) * w * 2 + (x / zoom)) * 4;
                let dst = (y * out_w + x) * 4;
                big[dst..dst + 4].copy_from_slice(&canvas[src..src + 4]);
            }
        }
        big
    };

    let path = std::env::args().nth(1).unwrap_or_else(|| "glyph_quad_probe.png".into());
    match image::save_buffer(
        &path,
        &canvas,
        out_w as u32,
        out_h as u32,
        image::ExtendedColorType::Rgba8,
    ) {
        Ok(()) => println!("\nwrote {path} — left: forced MTSDF, right: what Auto chooses"),
        Err(e) => println!("\ncould not write {path}: {e}"),
    }
}

/// Composites one glyph, sampling the atlas the way `text.wgsl` does.
#[allow(clippy::too_many_arguments)]
fn draw_glyph(
    canvas: &mut [u8],
    stride_px: usize,
    height: usize,
    x_offset: usize,
    text: &TextSystem,
    p: &GlyphPlacement,
    pen_x: f32,
    baseline: f32,
    size: f32,
) {
    let atlas = text.atlas();
    let Some(format) = atlas.page_format(p.page) else { return };
    let Some(pixels) = atlas.page_pixels(p.page) else { return };
    let page = atlas.page_size() as usize;
    let bpp = if format == GlyphFormat::Mtsdf { 4 } else { 1 };

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
    let y_hi = (y0 + qh).ceil().clamp(0.0, height as f32) as usize;
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

            let coverage = if p.is_bitmap {
                s[0]
            } else {
                let sd = median3(s[0], s[1], s[2]) - 0.5;
                (sd * screen_range + 0.5).clamp(0.0, 1.0)
            };
            let alpha = coverage.powf(GAMMA);
            if alpha <= 0.0 {
                continue;
            }

            let i = (py * stride_px + px + x_offset) * 4;
            if i + 3 >= canvas.len() {
                continue;
            }
            for k in 0..3 {
                let prev = canvas[i + k] as f32 / 255.0;
                canvas[i + k] = ((prev * (1.0 - alpha)) * 255.0) as u8;
            }
            canvas[i + 3] = 255;
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
