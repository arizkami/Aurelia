//! A minimal, deterministic WGSL composition system.
//!
//! WGSL has no `#include`, and every shader Sphere ships needs the same
//! rounded-box SDF, the same clip evaluation and the same frame bindings.
//! Copying them into six files would guarantee they drift.
//!
//! So there is exactly one directive:
//!
//! ```wgsl
//! //!include common/math.wgsl
//! ```
//!
//! It is resolved once at startup against a table of `include_str!`-embedded
//! sources, so shaders ship inside the binary and there is no runtime file I/O
//! and no shader search path to get wrong. Each file is included at most once
//! per module, in first-seen order, which makes the composed output byte-stable
//! — a property that matters because a pipeline cache keyed on source text is
//! only useful if the same shader produces the same text every run.
//!
//! This is deliberately not a preprocessor. There are no conditionals, no
//! macros and no expression evaluation, because a large custom shader language
//! is exactly what the engine should not grow.

use rustc_hash::FxHashSet;
use sphere_core::ShaderError;

/// The include directive prefix.
const INCLUDE: &str = "//!include ";

/// Maximum include depth, which also bounds cycle recovery.
const MAX_DEPTH: usize = 8;

macro_rules! shader_table {
    ($($path:literal),* $(,)?) => {
        /// Every shader source embedded in the binary, as `(path, source)`.
        pub const SOURCES: &[(&str, &str)] = &[
            $(($path, include_str!(concat!("../../../shaders/", $path)))),*
        ];
    };
}

shader_table! {
    "common/math.wgsl",
    "common/frame.wgsl",
    "quad.wgsl",
    "text.wgsl",
    "text_subpixel.wgsl",
    "mesh.wgsl",
    "composite.wgsl",
    "blur.wgsl",
}

/// Looks up an embedded shader source by path.
pub fn source(path: &str) -> Option<&'static str> {
    SOURCES.iter().find(|(p, _)| *p == path).map(|(_, s)| *s)
}

/// Resolves every `//!include` in `path`'s source, returning composed WGSL.
///
/// Includes are emitted before the including file's own body, so a helper is
/// always declared before use. A file already emitted is skipped rather than
/// duplicated, which is what allows two shaders to include the same helper.
pub fn compose(path: &str) -> Result<String, ShaderError> {
    let mut out = String::with_capacity(8 * 1024);
    // WGSL enable directives have to precede every declaration. Includes are
    // deliberately emitted before their parent source, so a directive inside
    // `text_subpixel.wgsl` would otherwise land too late. Keep module preludes
    // here, at the one point that can guarantee their position.
    if path == "text_subpixel.wgsl" {
        out.push_str("enable dual_source_blending;\n");
    }
    let mut seen = FxHashSet::default();
    expand(path, &mut out, &mut seen, 0)?;
    Ok(out)
}

fn expand(
    path: &str,
    out: &mut String,
    seen: &mut FxHashSet<String>,
    depth: usize,
) -> Result<(), ShaderError> {
    if depth > MAX_DEPTH {
        return Err(ShaderError {
            name: path.to_string(),
            message: format!("include depth exceeded {MAX_DEPTH}; likely a cycle"),
        });
    }
    if !seen.insert(path.to_string()) {
        return Ok(());
    }
    let src = source(path).ok_or_else(|| ShaderError {
        name: path.to_string(),
        message: "no such embedded shader source".to_string(),
    })?;

    // Two passes: pull in every include first, then emit this file's body. That
    // ordering is what guarantees a helper is declared before the code using it
    // regardless of where the directive sits in the file.
    for line in src.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix(INCLUDE) {
            let target = rest.trim();
            if target.is_empty() {
                return Err(ShaderError {
                    name: path.to_string(),
                    message: "empty //!include directive".to_string(),
                });
            }
            expand(target, out, seen, depth + 1)?;
        }
    }

    out.push_str("// ---- begin ");
    out.push_str(path);
    out.push_str(" ----\n");
    for line in src.lines() {
        if line.trim_start().starts_with(INCLUDE) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("// ---- end ");
    out.push_str(path);
    out.push_str(" ----\n");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_embedded_shader_composes() {
        for (path, _) in SOURCES {
            let composed = compose(path).unwrap_or_else(|e| panic!("{path}: {e}"));
            assert!(!composed.is_empty(), "{path} composed to nothing");
        }
    }

    #[test]
    fn every_pipeline_shader_parses_and_validates() {
        for (path, _) in SOURCES.iter().filter(|(path, _)| !path.starts_with("common/")) {
            let composed = compose(path).unwrap_or_else(|e| panic!("{path}: {e}"));
            let module = wgpu::naga::front::wgsl::parse_str(&composed)
                .unwrap_or_else(|e| panic!("{path}: {}", e.emit_to_string(&composed)));
            wgpu::naga::valid::Validator::new(
                wgpu::naga::valid::ValidationFlags::all(),
                wgpu::naga::valid::Capabilities::all(),
            )
            .validate(&module)
            .unwrap_or_else(|e| panic!("{path}: {e}"));
        }
    }

    #[test]
    fn includes_are_pulled_in_before_the_including_body() {
        let composed = compose("quad.wgsl").unwrap();
        let math = composed.find("fn sd_rounded_box").expect("math helper missing");
        let body = composed.find("fn vs_main").expect("quad body missing");
        assert!(math < body, "helper must be declared before use");
    }

    #[test]
    fn a_shared_helper_is_emitted_exactly_once() {
        // quad.wgsl includes math and frame; frame also uses math's helpers.
        // Emitting math twice would be a duplicate-definition error in naga.
        let composed = compose("quad.wgsl").unwrap();
        assert_eq!(composed.matches("fn sd_rounded_box").count(), 1);
        assert_eq!(composed.matches("fn median3").count(), 1);
    }

    #[test]
    fn directives_do_not_survive_into_the_output() {
        for (path, _) in SOURCES {
            let composed = compose(path).unwrap();
            assert!(!composed.contains(INCLUDE), "{path} leaked an include directive");
        }
    }

    #[test]
    fn composition_is_byte_stable_across_runs() {
        // A pipeline cache keyed on source text is only useful if the same
        // input yields the same bytes every time.
        let a = compose("text.wgsl").unwrap();
        let b = compose("text.wgsl").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn dual_source_extension_precedes_subpixel_shader_declarations() {
        let composed = compose("text_subpixel.wgsl").unwrap();
        assert!(composed.starts_with("enable dual_source_blending;\n"));
        let output = composed.find("struct SubpixelFragmentOutput").expect("output missing");
        assert!(output > "enable dual_source_blending;\n".len());
    }

    #[test]
    fn a_missing_include_target_is_an_error_not_a_panic() {
        let e = expand_from_str("//!include does/not/exist.wgsl\n").unwrap_err();
        assert!(e.message.contains("no such embedded shader"), "{e}");
    }

    fn expand_from_str(src: &str) -> Result<String, ShaderError> {
        // Exercises the same directive parsing the real path uses.
        let mut out = String::new();
        let mut seen = FxHashSet::default();
        for line in src.lines() {
            if let Some(rest) = line.trim_start().strip_prefix(INCLUDE) {
                expand(rest.trim(), &mut out, &mut seen, 0)?;
            }
        }
        Ok(out)
    }

    #[test]
    fn shader_flag_constants_match_the_rust_side() {
        // The shaders hard-code these because WGSL has no way to import them.
        // If a flag is renumbered on the Rust side this test is what catches it
        // before it turns into a silently miscoloured frame.
        let quad = source("quad.wgsl").unwrap();
        for (name, value) in [
            ("FILL_SOLID", sphere_render::quad_flags::FILL_SOLID),
            ("FILL_GRADIENT", sphere_render::quad_flags::FILL_GRADIENT),
            ("FILL_TEXTURE", sphere_render::quad_flags::FILL_TEXTURE),
            ("SHADOW", sphere_render::quad_flags::SHADOW),
            ("SHADOW_INSET", sphere_render::quad_flags::SHADOW_INSET),
            ("CLIP_ROUNDED", sphere_render::quad_flags::CLIP_ROUNDED),
            ("BORDER", sphere_render::quad_flags::BORDER),
        ] {
            let needle = format!("const {name}: u32 = {value}u;");
            assert!(quad.contains(&needle), "quad.wgsl is missing `{needle}`");
        }

        let text = source("text.wgsl").unwrap();
        for (name, value) in [
            ("BITMAP", sphere_render::glyph_flags::BITMAP),
            ("OUTLINE", sphere_render::glyph_flags::OUTLINE),
            ("CLIP_ROUNDED", sphere_render::glyph_flags::CLIP_ROUNDED),
            ("SUBPIXEL", sphere_render::glyph_flags::SUBPIXEL),
        ] {
            let needle = format!("const {name}: u32 = {value}u;");
            assert!(text.contains(&needle), "text.wgsl is missing `{needle}`");
        }
    }

    #[test]
    fn gradient_stop_count_matches_the_rust_side() {
        let frame = source("common/frame.wgsl").unwrap();
        let needle =
            format!("const MAX_GRADIENT_STOPS: u32 = {}u;", sphere_render::MAX_GRADIENT_STOPS);
        assert!(frame.contains(&needle), "frame.wgsl is missing `{needle}`");
    }
}
