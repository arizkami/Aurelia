//! Renders a line of text twice — once with the quad rule the batch compiler
//! used to apply, once with the one it applies now — and writes the pair to a
//! PNG.
//!
//! This is a probe, not a test: it needs system fonts, which CI does not have.
//! It exists so the claim in `docs/text.md` about the quad-sizing defect is a
//! picture and a table of measured numbers rather than an assertion.
//!
//! **It re-implements `text.wgsl` on the CPU.** The sampling, the median, the
//! screen range and the coverage gamma below are transcriptions of the shader,
//! not captures of what the GPU produced. It shows that the geometry handed to
//! the GPU is right; it cannot prove the GPU agreed.

use sphere_render::batch::{GlyphPlacement, GlyphProvider, GlyphRequest};
use sphere_render::scene::{DEFAULT_COVERAGE_GAMMA, TextRasterMode};
use sphere_text::{GlyphFormat, TextStyle, TextSystem};

const TEXT: &str = "Preferences Handgloves 0123";
const SIZE: f32 = 32.0;
const PAD: usize = 16;

fn main() {
    let mut text = TextSystem::with_system_fonts();
    let style = TextStyle { font_size: sphere_core::Px(SIZE), ..Default::default() };
    let layout = text.layout(TEXT, &style, None);

    let Some(line) = layout.lines.first() else {
        println!("nothing shaped; no system fonts?");
        return;
    };
    let baseline = line.baseline.get();

    // Resolve every glyph first: `place_glyph` mutates the atlas, and the render
    // below needs to borrow the page pixels immutably.
    let mut placed: Vec<(f32, GlyphPlacement)> = Vec::new();
    for run in &line.runs {
        for g in &run.glyphs {
            let Some(p) = text.place_glyph(GlyphRequest {
                font: run.font,
                glyph: g.glyph,
                font_size: run.font_size,
                device_scale: 1.0,
                mode: TextRasterMode::Mtsdf,
            }) else {
                continue;
            };
            if p.uv[2] <= p.uv[0] || p.uv[3] <= p.uv[1] {
                continue;
            }
            placed.push((g.position.x.get(), p));
        }
    }
    report(&placed);

    let w = layout.size.width.get().ceil() as usize + PAD * 2;
    let h = (baseline * 2.0).ceil() as usize + PAD * 2;
    // Opaque white ground: this is dark-on-light so the letterforms read as
    // shapes rather than as glowing edges.
    let mut canvas = vec![255u8; w * h * 2 * 4];

    // Left half: the ink box alone, which is what the compiler used to emit.
    // Right half: the ink box outset by the field's range, which is what
    // `generate_mtsdf` documents as the quad the caller must draw.
    for (half, outset) in [false, true].into_iter().enumerate() {
        for (pen_x, p) in &placed {
            draw_glyph(
                &mut canvas,
                w * 2,
                h,
                half * w,
                &text,
                p,
                *pen_x + PAD as f32,
                baseline + PAD as f32,
                outset,
            );
        }
    }

    // A hairline between the halves, so the two are unmistakably separate.
    for y in 0..h {
        let i = (y * w * 2 + w) * 4;
        canvas[i..i + 3].fill(200);
    }

    let path = std::env::args().nth(1).unwrap_or_else(|| "glyph_quad_probe.png".into());
    match image::save_buffer(
        &path,
        &canvas,
        (w * 2) as u32,
        h as u32,
        image::ExtendedColorType::Rgba8,
    ) {
        Ok(()) => println!("\nwrote {path} — left: ink box only, right: padded field"),
        Err(e) => println!("\ncould not write {path}: {e}"),
    }
}

/// Prints the ink-to-quad ratio per glyph: the factor by which drawing the ink
/// box alone shrinks a glyph while its advance stays correct.
fn report(placed: &[(f32, GlyphPlacement)]) {
    println!("{TEXT:?} at {SIZE} px");
    println!("{:>9} {:>9} {:>9}", "ink w", "quad w", "ink/quad");
    let (mut worst, mut sum, mut n) = (1.0f32, 0.0f32, 0usize);
    for (_, p) in placed {
        let ink = p.bounds_em[2] * SIZE;
        let quad = (p.bounds_em[2] + 2.0 * p.range_em) * SIZE;
        if quad <= 0.0 {
            continue;
        }
        let r = ink / quad;
        worst = worst.min(r);
        sum += r;
        n += 1;
        if n <= 8 {
            println!("{ink:>9.3} {quad:>9.3} {r:>9.3}");
        }
    }
    if n > 0 {
        println!("\nmean ink/quad {:.3}, worst {:.3} over {n} glyphs", sum / n as f32, worst);
        println!("Drawing the ink box renders each glyph at that fraction of its true size,");
        println!("with its advance unchanged — small text, spaced as though it were large.");
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
    outset: bool,
) {
    let atlas = text.atlas();
    if atlas.page_format(p.page) != Some(GlyphFormat::Mtsdf) {
        return;
    }
    let Some(pixels) = atlas.page_pixels(p.page) else { return };
    let page = atlas.page_size() as usize;

    let pad = if outset { p.range_em * SIZE } else { 0.0 };
    let x0 = pen_x + p.bounds_em[0] * SIZE - pad;
    let y0 = baseline + p.bounds_em[1] * SIZE - pad;
    let qw = p.bounds_em[2] * SIZE + 2.0 * pad;
    let qh = p.bounds_em[3] * SIZE + 2.0 * pad;
    if qw <= 0.0 || qh <= 0.0 {
        return;
    }

    // The shader derives its smoothing band from the field's range in
    // destination pixels. That figure is only true when the quad matches the
    // image, so the left half gets the band wrong for the same reason it gets
    // the size wrong — which is part of what the picture shows.
    let screen_range = (p.range_em * SIZE).max(1.0);

    let y_lo = y0.floor().max(0.0) as usize;
    let y_hi = (y0 + qh).ceil().clamp(0.0, height as f32) as usize;
    let x_lo = x0.floor().max(0.0) as usize;
    let x_hi = (x0 + qw).ceil().max(0.0) as usize;

    for py in y_lo..y_hi {
        for px in x_lo..x_hi {
            // Pixel centre in quad-local [0, 1], then into the atlas page.
            let fx = (px as f32 + 0.5 - x0) / qw;
            let fy = (py as f32 + 0.5 - y0) / qh;
            if !(0.0..1.0).contains(&fx) || !(0.0..1.0).contains(&fy) {
                continue;
            }
            let u = (p.uv[0] + (p.uv[2] - p.uv[0]) * fx) * page as f32 - 0.5;
            let v = (p.uv[1] + (p.uv[3] - p.uv[1]) * fy) * page as f32 - 0.5;
            let s = bilinear(pixels, page, u, v);

            let sd = median3(s[0], s[1], s[2]) - 0.5;
            let coverage = (sd * screen_range + 0.5).clamp(0.0, 1.0);
            let alpha = coverage.powf(DEFAULT_COVERAGE_GAMMA);
            if alpha <= 0.0 {
                continue;
            }

            let i = (py * stride_px + px + x_offset) * 4;
            if i + 3 >= canvas.len() {
                continue;
            }
            // Black over whatever is there, so overlapping quads accumulate the
            // way the blend state on the real glyph pass does.
            for k in 0..3 {
                let prev = canvas[i + k] as f32 / 255.0;
                canvas[i + k] = ((prev * (1.0 - alpha)) * 255.0) as u8;
            }
            canvas[i + 3] = 255;
        }
    }
}

/// Bilinear sample of an RGBA8 page, matching the sampler the glyph pass binds.
fn bilinear(pixels: &[u8], page: usize, u: f32, v: f32) -> [f32; 4] {
    let (x0, y0) = (u.floor(), v.floor());
    let (fx, fy) = (u - x0, v - y0);
    let texel = |x: f32, y: f32| -> [f32; 4] {
        let xi = (x as isize).clamp(0, page as isize - 1) as usize;
        let yi = (y as isize).clamp(0, page as isize - 1) as usize;
        let i = (yi * page + xi) * 4;
        if i + 3 >= pixels.len() {
            return [0.0; 4];
        }
        [
            pixels[i] as f32 / 255.0,
            pixels[i + 1] as f32 / 255.0,
            pixels[i + 2] as f32 / 255.0,
            pixels[i + 3] as f32 / 255.0,
        ]
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
