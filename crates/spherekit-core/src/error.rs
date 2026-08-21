//! Structured error types shared across the engine.
//!
//! SphereKit does not panic for conditions a running application can legitimately
//! hit: a lost surface, a minimised window, a missing font, a full atlas. Those
//! are values, and callers decide the policy.

use thiserror::Error;

/// Engine or backend initialisation failed.
#[derive(Debug, Error)]
pub enum InitError {
    /// No GPU adapter satisfied the request.
    #[error("no suitable GPU adapter found: {0}")]
    NoAdapter(String),
    /// The adapter exists but could not produce a device.
    #[error("failed to create GPU device: {0}")]
    DeviceCreation(String),
    /// The adapter lacks a capability the engine requires.
    #[error("adapter is missing required capability: {0}")]
    MissingCapability(String),
    /// The window system refused to create a surface.
    #[error("failed to create rendering surface: {0}")]
    Surface(String),
    /// A shader failed to compile during startup.
    #[error(transparent)]
    Shader(#[from] ShaderError),
    /// The platform layer failed.
    #[error(transparent)]
    Platform(#[from] PlatformError),
}

/// A surface could not be acquired or presented.
///
/// Most variants are recoverable and the correct response is to skip the frame,
/// not to tear anything down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SurfaceError {
    /// Acquisition timed out. Skip this frame and try again.
    #[error("surface acquisition timed out")]
    Timeout,
    /// The window is minimised or fully obscured. Skip this frame.
    #[error("surface is occluded")]
    Occluded,
    /// The configuration no longer matches the window. Reconfigure and retry.
    #[error("surface configuration is outdated")]
    Outdated,
    /// The surface is gone and must be recreated from the window handle.
    #[error("surface was lost")]
    Lost,
    /// The GPU device itself was lost; everything must be rebuilt.
    #[error("GPU device was lost")]
    DeviceLost,
    /// The surface has zero area, which every backend rejects.
    #[error("surface has zero area")]
    ZeroSized,
    /// The driver reported out of memory.
    #[error("out of memory acquiring surface")]
    OutOfMemory,
}

impl SurfaceError {
    /// True when skipping the frame is sufficient and no state must be rebuilt.
    #[inline]
    pub fn is_transient(self) -> bool {
        matches!(self, SurfaceError::Timeout | SurfaceError::Occluded | SurfaceError::ZeroSized)
    }

    /// True when the surface must be reconfigured before the next attempt.
    #[inline]
    pub fn needs_reconfigure(self) -> bool {
        matches!(self, SurfaceError::Outdated | SurfaceError::Lost)
    }

    /// True when the whole device and every GPU resource must be recreated.
    #[inline]
    pub fn is_fatal(self) -> bool {
        matches!(self, SurfaceError::DeviceLost | SurfaceError::OutOfMemory)
    }
}

/// Rendering a frame failed.
#[derive(Debug, Error)]
pub enum RenderError {
    /// The surface could not be acquired.
    #[error(transparent)]
    Surface(#[from] SurfaceError),
    /// A GPU allocation failed.
    #[error("GPU out of memory allocating {what}: {bytes} bytes")]
    OutOfMemory {
        /// What was being allocated.
        what: &'static str,
        /// How many bytes were requested.
        bytes: u64,
    },
    /// A shader failed to compile or validate.
    #[error(transparent)]
    Shader(#[from] ShaderError),
    /// The scene referenced a resource that no longer exists.
    #[error("scene referenced a stale resource handle: {0}")]
    StaleResource(&'static str),
    /// The backend rejected a command.
    #[error("backend rejected command: {0}")]
    Backend(String),
}

/// A shader failed to compile, validate or link.
#[derive(Debug, Error)]
#[error("shader `{name}` failed to compile: {message}")]
pub struct ShaderError {
    /// The shader module's name, for locating the source.
    pub name: String,
    /// The backend's diagnostic.
    pub message: String,
}

/// Font loading, discovery or shaping failed.
#[derive(Debug, Error)]
pub enum FontError {
    /// No face matched the requested family and style, and no fallback existed.
    #[error("no font matched family `{0}`")]
    NoMatch(String),
    /// The font data could not be parsed.
    #[error("failed to parse font `{name}`: {reason}")]
    Parse {
        /// The face's name or path.
        name: String,
        /// Why parsing failed.
        reason: String,
    },
    /// The font file could not be read.
    #[error("failed to read font file `{path}`: {source}")]
    Io {
        /// The path that failed.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The requested glyph is not present in the face.
    #[error("glyph {0:?} not present in face")]
    MissingGlyph(crate::id::GlyphId),
    /// The glyph atlas could not fit another glyph.
    #[error("glyph atlas is full: {0}")]
    AtlasFull(String),
}

/// Image decoding or upload failed.
#[derive(Debug, Error)]
pub enum ImageError {
    /// The container or codec is not supported.
    #[error("unsupported image format: {0}")]
    UnsupportedFormat(String),
    /// The image data could not be decoded.
    #[error("failed to decode image: {0}")]
    Decode(String),
    /// The image could not be read from disk.
    #[error("failed to read image `{path}`: {source}")]
    Io {
        /// The path that failed.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The image exceeds what the GPU can hold.
    #[error("image is {width}x{height}, exceeding the maximum texture dimension {max}")]
    TooLarge {
        /// Decoded width.
        width: u32,
        /// Decoded height.
        height: u32,
        /// The device limit.
        max: u32,
    },
}

/// Layout computation failed.
#[derive(Debug, Error)]
pub enum LayoutError {
    /// A node id did not resolve.
    #[error("layout node not found: {0:?}")]
    NodeNotFound(crate::id::NodeId),
    /// The tree contains a cycle, so layout cannot terminate.
    #[error("layout tree contains a cycle at {0:?}")]
    Cycle(crate::id::NodeId),
    /// The backing layout algorithm reported an error.
    #[error("layout engine error: {0}")]
    Engine(String),
    /// A measure callback panicked or returned a non-finite size.
    #[error("measure function produced an invalid size for {0:?}")]
    InvalidMeasure(crate::id::NodeId),
}

/// SVG parsing or conversion failed.
#[derive(Debug, Error)]
pub enum SvgError {
    /// The document could not be parsed.
    #[error("failed to parse SVG: {0}")]
    Parse(String),
    /// The document uses a feature SphereKit does not implement.
    #[error("unsupported SVG feature: {0}")]
    Unsupported(String),
    /// The file could not be read.
    #[error("failed to read SVG `{path}`: {source}")]
    Io {
        /// The path that failed.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// A platform or windowing operation failed.
#[derive(Debug, Error)]
pub enum PlatformError {
    /// The window could not be created.
    #[error("failed to create window: {0}")]
    WindowCreation(String),
    /// The event loop could not be created or driven.
    #[error("event loop error: {0}")]
    EventLoop(String),
    /// The window handle was not usable for surface creation.
    #[error("invalid window handle: {0}")]
    InvalidHandle(String),
    /// Clipboard access failed.
    #[error("clipboard error: {0}")]
    Clipboard(String),
    /// The operation is not implemented on this platform.
    #[error("`{0}` is not supported on this platform")]
    Unsupported(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surface_error_recovery_classes_are_disjoint_and_total() {
        // Every variant must fall into exactly one recovery class, or the frame
        // loop will either spin forever or tear down state it did not need to.
        let all = [
            SurfaceError::Timeout,
            SurfaceError::Occluded,
            SurfaceError::Outdated,
            SurfaceError::Lost,
            SurfaceError::DeviceLost,
            SurfaceError::ZeroSized,
            SurfaceError::OutOfMemory,
        ];
        for e in all {
            let n = [e.is_transient(), e.needs_reconfigure(), e.is_fatal()]
                .iter()
                .filter(|b| **b)
                .count();
            assert_eq!(n, 1, "{e:?} is in {n} recovery classes, expected exactly 1");
        }
    }

    #[test]
    fn a_minimised_window_is_transient_not_fatal() {
        // Regression guard: minimising a window must never tear down the device.
        assert!(SurfaceError::Occluded.is_transient());
        assert!(!SurfaceError::Occluded.is_fatal());
        assert!(SurfaceError::ZeroSized.is_transient());
        assert!(!SurfaceError::ZeroSized.is_fatal());
    }

    #[test]
    fn errors_render_a_useful_message() {
        let e = RenderError::OutOfMemory { what: "instance buffer", bytes: 1024 };
        assert!(e.to_string().contains("instance buffer"));
        assert!(e.to_string().contains("1024"));
    }

    #[test]
    fn surface_errors_convert_into_render_errors() {
        let e: RenderError = SurfaceError::Lost.into();
        assert!(matches!(e, RenderError::Surface(SurfaceError::Lost)));
    }
}
