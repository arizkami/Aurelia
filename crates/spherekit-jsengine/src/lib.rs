//! Safe Rust access to the bundled V8 runtime.
//!
//! V8 is a C++ API with thread-affine isolates and stack-scoped local handles,
//! so the public Rust surface deliberately exposes an owned engine instead of
//! leaking `v8::Local<T>` handles across FFI. The native shim keeps all V8
//! handles inside one call and returns owned UTF-8 data to Rust.

use std::error::Error as StdError;
use std::fmt;

/// Result type used by the JavaScript engine wrapper.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by the V8 binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The current target is not supported by the checked-in V8 prebuilt.
    UnsupportedPlatform,
    /// The native V8 bridge rejected an argument or failed an operation.
    V8 { status: i32, message: String },
    /// V8 returned bytes that were not valid UTF-8.
    InvalidUtf8,
    /// The supplied ICU path contains an interior NUL byte.
    InvalidPath,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                f.write_str("spherekit-jsengine currently supports Windows x86_64 only")
            }
            Self::V8 { status, message } => write!(f, "V8 error {status}: {message}"),
            Self::InvalidUtf8 => f.write_str("V8 returned invalid UTF-8"),
            Self::InvalidPath => f.write_str("V8 ICU path contains an interior NUL byte"),
        }
    }
}

impl StdError for Error {}

#[cfg(windows)]
mod ffi {
    use std::ffi::{c_char, c_void};

    #[repr(C)]
    pub struct Engine {
        _private: [u8; 0],
    }

    unsafe extern "C" {
        pub fn spherekit_v8_engine_new(
            icu_data_path: *const c_char,
            output: *mut *mut Engine,
            error: *mut c_char,
            error_capacity: usize,
        ) -> i32;
        pub fn spherekit_v8_engine_free(engine: *mut Engine);
        pub fn spherekit_v8_engine_eval(
            engine: *mut Engine,
            source: *const u8,
            source_length: usize,
            result: *mut *mut u8,
            result_length: *mut usize,
            error: *mut c_char,
            error_capacity: usize,
        ) -> i32;
        pub fn spherekit_v8_free_buffer(buffer: *mut u8);
        pub fn spherekit_v8_version() -> *const c_char;
    }

    #[allow(dead_code)]
    fn _opaque_is_opaque(_: *mut c_void) {}
}

/// An owned V8 isolate and execution context.
///
/// An engine is intentionally used through `&mut self`: V8 isolates are
/// entered by one thread at a time, and this prevents accidental concurrent
/// calls from safe Rust without pretending the isolate is `Sync`.
#[cfg(windows)]
pub struct Engine {
    raw: std::ptr::NonNull<ffi::Engine>,
}

#[cfg(windows)]
impl Engine {
    /// Creates an engine using the ICU data path emitted by the build script.
    pub fn new() -> Result<Self> {
        Self::with_icu_data_path(option_env!("SPHEREKIT_V8_ICU_DATA_PATH"))
    }

    /// Creates an engine with an explicit `icudtl.dat` path.
    pub fn with_icu_data_path(path: Option<&str>) -> Result<Self> {
        let path = path.map(std::ffi::CString::new).transpose().map_err(|_| Error::InvalidPath)?;
        let path_ptr = path.as_ref().map_or(std::ptr::null(), |value| value.as_ptr());
        let mut raw = std::ptr::null_mut();
        let mut error = [0i8; 4096];
        let status = unsafe {
            ffi::spherekit_v8_engine_new(path_ptr, &mut raw, error.as_mut_ptr(), error.len())
        };
        if status != 0 {
            return Err(Error::V8 { status, message: c_string(&error) });
        }
        let raw = std::ptr::NonNull::new(raw)
            .ok_or_else(|| Error::V8 { status: 2, message: "V8 returned a null engine".into() })?;
        Ok(Self { raw })
    }

    /// Evaluates a UTF-8 JavaScript source string and returns its UTF-8 value.
    pub fn eval(&mut self, source: &str) -> Result<String> {
        let mut result = std::ptr::null_mut();
        let mut result_length = 0usize;
        let mut error = [0i8; 4096];
        let status = unsafe {
            ffi::spherekit_v8_engine_eval(
                self.raw.as_ptr(),
                source.as_ptr(),
                source.len(),
                &mut result,
                &mut result_length,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if status != 0 {
            return Err(Error::V8 { status, message: c_string(&error) });
        }

        let bytes = unsafe {
            let bytes = std::slice::from_raw_parts(result, result_length).to_vec();
            ffi::spherekit_v8_free_buffer(result);
            bytes
        };
        String::from_utf8(bytes).map_err(|_| Error::InvalidUtf8)
    }

    /// Returns the version compiled into the downloaded V8 prebuilt.
    pub fn version() -> String {
        unsafe {
            let version = ffi::spherekit_v8_version();
            if version.is_null() {
                return String::new();
            }
            std::ffi::CStr::from_ptr(version).to_string_lossy().into_owned()
        }
    }
}

#[cfg(windows)]
impl Drop for Engine {
    fn drop(&mut self) {
        unsafe { ffi::spherekit_v8_engine_free(self.raw.as_ptr()) }
    }
}

#[cfg(windows)]
fn c_string(bytes: &[i8]) -> String {
    let bytes =
        bytes.iter().map(|byte| *byte as u8).take_while(|byte| *byte != 0).collect::<Vec<_>>();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Returns the V8 version without creating an isolate.
#[cfg(windows)]
pub fn v8_version() -> String {
    Engine::version()
}

#[cfg(not(windows))]
/// Placeholder type on targets without the Windows V8 prebuilt.
pub struct Engine;

#[cfg(not(windows))]
impl Engine {
    pub fn new() -> Result<Self> {
        Err(Error::UnsupportedPlatform)
    }
}

#[cfg(not(windows))]
pub fn v8_version() -> String {
    String::new()
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn evaluates_javascript() {
        let mut engine = Engine::new().expect("create V8 engine");
        assert_eq!(engine.eval("1 + 2").expect("evaluate script"), "3");
    }

    #[test]
    fn reports_javascript_errors() {
        let mut engine = Engine::new().expect("create V8 engine");
        let error = engine.eval("throw new Error('boom')").expect_err("script should fail");
        assert!(error.to_string().contains("boom"), "{error}");
    }
}
