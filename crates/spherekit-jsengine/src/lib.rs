//! Safe Rust access to the bundled V8 runtime.
//!
//! V8 is a C++ API with thread-affine isolates and stack-scoped local handles,
//! so the public Rust surface deliberately exposes an owned engine instead of
//! leaking `v8::Local<T>` handles across FFI. The native shim keeps all V8
//! handles inside one call and returns owned UTF-8 data to Rust.
//!
//! ## Why the API is string-in, string-out
//!
//! [`Engine::bind`] and [`Engine::call_global`] both speak `&str`, not a
//! `serde` value or a `v8::Value`. That is not a placeholder. Anything richer
//! forces a shared value model across the FFI boundary, and every host that
//! embeds this crate already has one — `spherekit-bridge` sends JSON Lines.
//! Keeping the boundary at "one owned UTF-8 buffer each way" means the unsafe
//! surface is a fixed handful of pointer copies rather than a marshaller that
//! grows a case for every new type.
//!
//! ## Why microtasks and platform tasks are pumped by hand
//!
//! V8's default microtask policy runs promise continuations whenever script
//! call depth drops to zero, which puts arbitrary JavaScript in the middle of
//! whatever the host was doing — including a layout pass. The engine switches
//! the isolate to an explicit policy, so [`Engine::run_microtasks`] and
//! [`Engine::pump`] are the only points at which queued JavaScript runs, and
//! the host chooses where in its frame those points are.

#![deny(missing_docs)]

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
    V8 {
        /// Status code reported by the native shim.
        status: i32,
        /// Human-readable detail from the shim.
        message: String,
    },
    /// A JavaScript exception, with whatever position and stack V8 gave us.
    ///
    /// The fields are separate rather than pre-formatted because a REPL wants
    /// the stack on its own lines, an error overlay wants the position to jump
    /// to, and a log line wants neither.
    Exception {
        /// The exception rendered as a string, normally `Error: something`.
        message: String,
        /// The `stack` property of the thrown value, or empty if it had none.
        stack: String,
        /// One-based line of the throw site, or zero when V8 did not know.
        line: u32,
        /// One-based column of the throw site, or zero when V8 did not know.
        column: u32,
        /// Script name the throw site came from, as passed to [`Engine::eval_named`].
        resource: String,
    },
    /// V8 returned bytes that were not valid UTF-8.
    InvalidUtf8,
    /// The supplied ICU path contains an interior NUL byte.
    InvalidPath,
    /// A [`Engine::bind`] name was not a valid identifier, or was already taken.
    ///
    /// Carries the rejected name, since the caller normally built it from a
    /// table of host functions and needs to know which entry is wrong.
    InvalidBinding(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                f.write_str("spherekit-jsengine currently supports Windows x86_64 only")
            }
            Self::V8 { status, message } => write!(f, "V8 error {status}: {message}"),
            Self::Exception { message, line, column, resource, .. } => {
                if resource.is_empty() {
                    f.write_str(message)
                } else {
                    write!(f, "{resource}:{line}:{column}: {message}")
                }
            }
            Self::InvalidUtf8 => f.write_str("V8 returned invalid UTF-8"),
            Self::InvalidPath => f.write_str("V8 ICU path contains an interior NUL byte"),
            Self::InvalidBinding(name) => write!(
                f,
                "`{name}` is not a usable host function name: it must be a plain ASCII \
                 identifier that is not already defined on globalThis"
            ),
        }
    }
}

impl StdError for Error {}

/// Status codes shared with `v8_shim.cc`. They must stay in step with the
/// `k*` constants there; nothing else in the crate depends on their values.
#[cfg(v8_backend)]
mod status {
    /// The call succeeded.
    pub const OK: i32 = 0;
    /// A JavaScript exception was caught and reported in full.
    pub const JAVASCRIPT_ERROR: i32 = 4;
    /// A binding name was rejected by the shim.
    pub const INVALID_BINDING: i32 = 5;
}

#[cfg(v8_backend)]
mod ffi {
    use std::ffi::{c_char, c_void};

    /// Opaque native engine. Only ever handled behind a pointer.
    #[repr(C)]
    pub struct Engine {
        _private: [u8; 0],
    }

    /// Error detail filled in by the shim.
    ///
    /// Must be passed in zeroed. On a non-zero status the three buffers belong
    /// to Rust and are released with [`spherekit_v8_free_buffer`]; on success
    /// they stay null and nothing needs freeing.
    #[repr(C)]
    pub struct RawError {
        pub message: *mut u8,
        pub message_length: usize,
        pub stack: *mut u8,
        pub stack_length: usize,
        pub resource: *mut u8,
        pub resource_length: usize,
        pub line: u32,
        pub column: u32,
    }

    impl RawError {
        /// A zeroed record, ready to be handed to the shim.
        pub const fn zeroed() -> Self {
            Self {
                message: std::ptr::null_mut(),
                message_length: 0,
                stack: std::ptr::null_mut(),
                stack_length: 0,
                resource: std::ptr::null_mut(),
                resource_length: 0,
                line: 0,
                column: 0,
            }
        }
    }

    /// The shape a bound Rust closure crosses the boundary as.
    ///
    /// `user_data` points at a `HostFunction` owned by [`crate::Engine`] and
    /// stays valid until the engine is dropped. `argument` is borrowed for the
    /// duration of the call. Returning zero means `result` was filled with a
    /// buffer from [`spherekit_v8_alloc`]; anything else means `error` was, and
    /// the shim throws it as a JavaScript `Error`. The shim frees whichever
    /// buffers were produced.
    pub type HostCallback = unsafe extern "C" fn(
        user_data: *mut c_void,
        argument: *const u8,
        argument_length: usize,
        result: *mut *mut u8,
        result_length: *mut usize,
        error: *mut *mut u8,
        error_length: *mut usize,
    ) -> i32;

    unsafe extern "C" {
        pub fn spherekit_v8_alloc(size: usize) -> *mut u8;
        pub fn spherekit_v8_free_buffer(buffer: *mut u8);
        pub fn spherekit_v8_version() -> *const c_char;

        pub fn spherekit_v8_engine_new(
            icu_data_path: *const c_char,
            output: *mut *mut Engine,
            error: *mut RawError,
        ) -> i32;
        pub fn spherekit_v8_engine_free(engine: *mut Engine);
        pub fn spherekit_v8_engine_eval(
            engine: *mut Engine,
            source: *const u8,
            source_length: usize,
            name: *const u8,
            name_length: usize,
            result: *mut *mut u8,
            result_length: *mut usize,
            error: *mut RawError,
        ) -> i32;
        pub fn spherekit_v8_engine_bind(
            engine: *mut Engine,
            name: *const u8,
            name_length: usize,
            callback: HostCallback,
            user_data: *mut c_void,
            error: *mut RawError,
        ) -> i32;
        pub fn spherekit_v8_engine_call_global(
            engine: *mut Engine,
            name: *const u8,
            name_length: usize,
            argument: *const u8,
            argument_length: usize,
            found: *mut i32,
            result: *mut *mut u8,
            result_length: *mut usize,
            error: *mut RawError,
        ) -> i32;
        pub fn spherekit_v8_engine_has_global(
            engine: *mut Engine,
            name: *const u8,
            name_length: usize,
            out: *mut i32,
            error: *mut RawError,
        ) -> i32;
        pub fn spherekit_v8_engine_run_microtasks(engine: *mut Engine, error: *mut RawError)
        -> i32;
        pub fn spherekit_v8_engine_pump(engine: *mut Engine, error: *mut RawError) -> i32;
    }
}

/// A Rust closure reachable from JavaScript.
///
/// Boxed twice on purpose: the outer box gives the record an address that stays
/// put once V8 has captured it in a `v8::External`, and the inner box erases the
/// closure's type so one `extern "C"` thunk serves every binding.
#[cfg(v8_backend)]
struct HostFunction {
    handler: Box<dyn FnMut(&str) -> std::result::Result<String, String>>,
}

/// Writes `bytes` into a fresh shim-allocated buffer, which the shim then frees.
///
/// # Safety
///
/// `out` and `out_length` must be valid for writes.
#[cfg(v8_backend)]
unsafe fn emit(bytes: &[u8], out: *mut *mut u8, out_length: *mut usize) -> bool {
    let buffer = unsafe { ffi::spherekit_v8_alloc(bytes.len()) };
    if buffer.is_null() {
        return false;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, bytes.len());
        *out = buffer;
        *out_length = bytes.len();
    }
    true
}

/// The one C entry point every binding shares.
///
/// A panic must not unwind into C++, so the closure runs inside
/// [`std::panic::catch_unwind`] and a panic is reported to JavaScript as a
/// thrown error instead. Under `panic = "abort"` that is dead weight; under the
/// test profile it is the difference between a failing assert and undefined
/// behaviour.
#[cfg(v8_backend)]
unsafe extern "C" fn host_thunk(
    user_data: *mut std::ffi::c_void,
    argument: *const u8,
    argument_length: usize,
    result: *mut *mut u8,
    result_length: *mut usize,
    error: *mut *mut u8,
    error_length: *mut usize,
) -> i32 {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let function = unsafe { &mut *user_data.cast::<HostFunction>() };
        let bytes = if argument_length == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(argument, argument_length) }
        };
        match std::str::from_utf8(bytes) {
            Ok(text) => (function.handler)(text),
            Err(_) => Err("host function argument was not valid UTF-8".to_owned()),
        }
    }));

    let message = match outcome {
        Ok(Ok(value)) => {
            if unsafe { emit(value.as_bytes(), result, result_length) } {
                return 0;
            }
            "host function result allocation failed".to_owned()
        }
        Ok(Err(message)) => message,
        Err(_) => "host function panicked".to_owned(),
    };
    unsafe { emit(message.as_bytes(), error, error_length) };
    1
}

/// True when `name` can be reached from JavaScript as a bare identifier.
///
/// Deliberately stricter than the ECMAScript grammar, which allows Unicode
/// `ID_Start`: host function names are chosen by Rust code, and the check exists
/// to catch `""` or `"app.log"` — both of which V8 would happily accept as
/// property keys and neither of which is callable as `app.log(x)` from script —
/// not to re-implement the identifier grammar.
///
/// Reserved words are *not* rejected. `bind("delete", ..)` produces a global
/// that script can only reach as `globalThis["delete"](x)`, which is a strange
/// thing to want but not a broken one, and the alternative is carrying a copy of
/// the reserved-word table around for a mistake nobody has made yet.
#[cfg(v8_backend)]
fn is_binding_name(name: &str) -> bool {
    let mut characters = name.chars();
    let Some(first) = characters.next() else { return false };
    if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    characters
        .all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '$')
}

/// Takes ownership of one shim-allocated string, freeing the native buffer.
///
/// Lossy on purpose: this only ever runs on an error path, and losing the
/// diagnostic because the diagnostic itself was malformed helps nobody.
///
/// # Safety
///
/// `pointer` must be null or a live buffer of `length` bytes from the shim.
#[cfg(v8_backend)]
unsafe fn take_string(pointer: &mut *mut u8, length: &mut usize) -> String {
    if pointer.is_null() {
        return String::new();
    }
    let bytes = unsafe { std::slice::from_raw_parts(*pointer, *length) }.to_vec();
    unsafe { ffi::spherekit_v8_free_buffer(*pointer) };
    *pointer = std::ptr::null_mut();
    *length = 0;
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Converts a failed shim call into an [`Error`], releasing the native buffers.
#[cfg(v8_backend)]
fn take_error(status: i32, raw: &mut ffi::RawError) -> Error {
    let message = unsafe { take_string(&mut raw.message, &mut raw.message_length) };
    let stack = unsafe { take_string(&mut raw.stack, &mut raw.stack_length) };
    let resource = unsafe { take_string(&mut raw.resource, &mut raw.resource_length) };
    match status {
        status::JAVASCRIPT_ERROR => {
            Error::Exception { message, stack, line: raw.line, column: raw.column, resource }
        }
        status::INVALID_BINDING => Error::InvalidBinding(message),
        _ => Error::V8 { status, message },
    }
}

/// Takes ownership of a shim-allocated result buffer as a `String`.
#[cfg(v8_backend)]
fn take_result(pointer: *mut u8, length: usize) -> Result<String> {
    if pointer.is_null() {
        return Ok(String::new());
    }
    let bytes = unsafe {
        let bytes = std::slice::from_raw_parts(pointer, length).to_vec();
        ffi::spherekit_v8_free_buffer(pointer);
        bytes
    };
    String::from_utf8(bytes).map_err(|_| Error::InvalidUtf8)
}

/// An owned V8 isolate and execution context.
///
/// An engine is intentionally used through `&mut self`: V8 isolates are
/// entered by one thread at a time, and this prevents accidental concurrent
/// calls from safe Rust without pretending the isolate is `Sync`.
#[cfg(v8_backend)]
pub struct Engine {
    raw: std::ptr::NonNull<ffi::Engine>,
    /// Boxed handlers whose addresses V8 holds. Owned here, not by the shim, so
    /// that the Rust type that knows how to drop them is the one that does.
    bindings: Vec<*mut HostFunction>,
}

#[cfg(v8_backend)]
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
        let mut error = ffi::RawError::zeroed();
        let status = unsafe { ffi::spherekit_v8_engine_new(path_ptr, &mut raw, &mut error) };
        if status != status::OK {
            return Err(take_error(status, &mut error));
        }
        let raw = std::ptr::NonNull::new(raw)
            .ok_or_else(|| Error::V8 { status: 2, message: "V8 returned a null engine".into() })?;
        Ok(Self { raw, bindings: Vec::new() })
    }

    /// Evaluates a UTF-8 JavaScript source string and returns its UTF-8 value.
    ///
    /// The script is named `<eval>`. V8's own default for an unnamed script is
    /// the literal string `undefined`, which is worse than useless in a stack
    /// trace, so even the anonymous case gets an honest name.
    pub fn eval(&mut self, source: &str) -> Result<String> {
        self.eval_named(source, "<eval>")
    }

    /// Evaluates with a script name, so exceptions and stack traces name a real file.
    pub fn eval_named(&mut self, source: &str, name: &str) -> Result<String> {
        let mut result = std::ptr::null_mut();
        let mut result_length = 0usize;
        let mut error = ffi::RawError::zeroed();
        let status = unsafe {
            ffi::spherekit_v8_engine_eval(
                self.raw.as_ptr(),
                source.as_ptr(),
                source.len(),
                name.as_ptr(),
                name.len(),
                &mut result,
                &mut result_length,
                &mut error,
            )
        };
        if status != status::OK {
            return Err(take_error(status, &mut error));
        }
        take_result(result, result_length)
    }

    /// Installs a Rust closure as a global function taking one string and
    /// returning one string.
    ///
    /// The JavaScript side sees `globalThis[name](argument) -> string`. A
    /// returned `Err` becomes a thrown JavaScript `Error` carrying the message,
    /// which is what makes a host failure catchable by script instead of a
    /// silent `undefined`.
    ///
    /// The closure outlives the call that installed it, so it is kept alive by
    /// the engine and dropped with it. Binding a name that already exists on
    /// `globalThis` fails rather than shadowing it.
    pub fn bind(
        &mut self,
        name: &str,
        handler: impl FnMut(&str) -> std::result::Result<String, String> + 'static,
    ) -> Result<()> {
        if !is_binding_name(name) {
            return Err(Error::InvalidBinding(name.to_owned()));
        }

        let boxed = Box::into_raw(Box::new(HostFunction { handler: Box::new(handler) }));
        let mut error = ffi::RawError::zeroed();
        let status = unsafe {
            ffi::spherekit_v8_engine_bind(
                self.raw.as_ptr(),
                name.as_ptr(),
                name.len(),
                host_thunk,
                boxed.cast(),
                &mut error,
            )
        };
        if status != status::OK {
            // V8 never saw the record, so this is the last chance to reclaim it.
            drop(unsafe { Box::from_raw(boxed) });
            let reported = take_error(status, &mut error);
            return Err(match reported {
                Error::InvalidBinding(_) => Error::InvalidBinding(name.to_owned()),
                other => other,
            });
        }
        self.bindings.push(boxed);
        Ok(())
    }

    /// Calls a global function by name with one string argument, returning its
    /// string result.
    ///
    /// Returns `Ok(None)` when no such global exists, so an optional JavaScript
    /// callback is not an error. A global that exists but is not callable *is*
    /// an error: that is a mistake in the script, not an absent hook.
    pub fn call_global(&mut self, name: &str, argument: &str) -> Result<Option<String>> {
        let mut found = 0i32;
        let mut result = std::ptr::null_mut();
        let mut result_length = 0usize;
        let mut error = ffi::RawError::zeroed();
        let status = unsafe {
            ffi::spherekit_v8_engine_call_global(
                self.raw.as_ptr(),
                name.as_ptr(),
                name.len(),
                argument.as_ptr(),
                argument.len(),
                &mut found,
                &mut result,
                &mut result_length,
                &mut error,
            )
        };
        if status != status::OK {
            return Err(take_error(status, &mut error));
        }
        if found == 0 {
            return Ok(None);
        }
        take_result(result, result_length).map(Some)
    }

    /// True when `globalThis[name]` is callable.
    pub fn has_global(&mut self, name: &str) -> Result<bool> {
        let mut out = 0i32;
        let mut error = ffi::RawError::zeroed();
        let status = unsafe {
            ffi::spherekit_v8_engine_has_global(
                self.raw.as_ptr(),
                name.as_ptr(),
                name.len(),
                &mut out,
                &mut error,
            )
        };
        if status != status::OK {
            return Err(take_error(status, &mut error));
        }
        Ok(out != 0)
    }

    /// Drains the microtask queue — resolved promise callbacks run here.
    pub fn run_microtasks(&mut self) -> Result<()> {
        let mut error = ffi::RawError::zeroed();
        let status =
            unsafe { ffi::spherekit_v8_engine_run_microtasks(self.raw.as_ptr(), &mut error) };
        if status != status::OK {
            return Err(take_error(status, &mut error));
        }
        Ok(())
    }

    /// Pumps V8's foreground platform tasks. Call once per frame alongside
    /// [`Engine::run_microtasks`].
    ///
    /// Bounded rather than drained to exhaustion: this runs on the thread that
    /// draws, and a task that reposts itself would otherwise hold the frame
    /// hostage.
    pub fn pump(&mut self) -> Result<()> {
        let mut error = ffi::RawError::zeroed();
        let status = unsafe { ffi::spherekit_v8_engine_pump(self.raw.as_ptr(), &mut error) };
        if status != status::OK {
            return Err(take_error(status, &mut error));
        }
        Ok(())
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

#[cfg(v8_backend)]
impl Drop for Engine {
    fn drop(&mut self) {
        // The isolate goes first. Once it is disposed no script can be running,
        // which is what makes reclaiming the handler boxes afterwards safe.
        unsafe { ffi::spherekit_v8_engine_free(self.raw.as_ptr()) };
        for binding in self.bindings.drain(..) {
            drop(unsafe { Box::from_raw(binding) });
        }
    }
}

/// Returns the V8 version without creating an isolate.
#[cfg(v8_backend)]
pub fn v8_version() -> String {
    Engine::version()
}

/// Placeholder engine on targets without the Windows V8 prebuilt.
///
/// The whole API is mirrored rather than compiled out so that a host crate can
/// be written once and fail at runtime on an unsupported target instead of
/// failing to build there.
#[cfg(not(v8_backend))]
pub struct Engine;

#[cfg(not(v8_backend))]
impl Engine {
    /// Always fails: no V8 prebuilt exists for this target.
    pub fn new() -> Result<Self> {
        Err(Error::UnsupportedPlatform)
    }

    /// Always fails: no V8 prebuilt exists for this target.
    pub fn with_icu_data_path(_path: Option<&str>) -> Result<Self> {
        Err(Error::UnsupportedPlatform)
    }

    /// Always fails: no V8 prebuilt exists for this target.
    pub fn eval(&mut self, _source: &str) -> Result<String> {
        Err(Error::UnsupportedPlatform)
    }

    /// Always fails: no V8 prebuilt exists for this target.
    pub fn eval_named(&mut self, _source: &str, _name: &str) -> Result<String> {
        Err(Error::UnsupportedPlatform)
    }

    /// Always fails: no V8 prebuilt exists for this target.
    pub fn bind(
        &mut self,
        _name: &str,
        _handler: impl FnMut(&str) -> std::result::Result<String, String> + 'static,
    ) -> Result<()> {
        Err(Error::UnsupportedPlatform)
    }

    /// Always fails: no V8 prebuilt exists for this target.
    pub fn call_global(&mut self, _name: &str, _argument: &str) -> Result<Option<String>> {
        Err(Error::UnsupportedPlatform)
    }

    /// Always fails: no V8 prebuilt exists for this target.
    pub fn has_global(&mut self, _name: &str) -> Result<bool> {
        Err(Error::UnsupportedPlatform)
    }

    /// Always fails: no V8 prebuilt exists for this target.
    pub fn run_microtasks(&mut self) -> Result<()> {
        Err(Error::UnsupportedPlatform)
    }

    /// Always fails: no V8 prebuilt exists for this target.
    pub fn pump(&mut self) -> Result<()> {
        Err(Error::UnsupportedPlatform)
    }

    /// The empty string: there is no linked V8 to ask.
    pub fn version() -> String {
        String::new()
    }
}

/// Returns the V8 version without creating an isolate.
#[cfg(not(v8_backend))]
pub fn v8_version() -> String {
    String::new()
}

#[cfg(all(test, v8_backend))]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// The script the crate ships as its smoke test, evaluated for real below.
    const WELCOME: &str = include_str!("../test/welcome.js");

    fn engine() -> Engine {
        Engine::new().expect("create V8 engine")
    }

    fn exception(error: &Error) -> (&str, &str, u32, u32, &str) {
        match error {
            Error::Exception { message, stack, line, column, resource } => {
                (message.as_str(), stack.as_str(), *line, *column, resource.as_str())
            }
            other => panic!("expected a JavaScript exception, got {other:?}"),
        }
    }

    #[test]
    fn evaluates_javascript() {
        assert_eq!(engine().eval("1 + 2").expect("evaluate script"), "3");
    }

    #[test]
    fn reports_javascript_errors() {
        let error = engine().eval("throw new Error('boom')").expect_err("script should fail");
        assert!(error.to_string().contains("boom"), "{error}");
    }

    #[test]
    fn eval_named_puts_the_script_name_in_the_exception() {
        let error = engine()
            .eval_named("const a = 1;\n\nthrow new Error('boom');", "welcome.js")
            .expect_err("script should fail");
        let (message, _, line, _, resource) = exception(&error);
        assert_eq!(resource, "welcome.js");
        assert_eq!(line, 3);
        assert!(message.contains("boom"), "{message}");
    }

    #[test]
    fn a_thrown_error_carries_a_stack_and_a_plausible_position() {
        let error = engine()
            .eval_named("function inner() { throw new Error('deep'); }\ninner();", "deep.js")
            .expect_err("script should fail");
        let (_, stack, line, column, _) = exception(&error);
        assert!(stack.contains("deep"), "{stack}");
        assert!(stack.contains("deep.js"), "{stack}");
        assert_eq!(line, 1);
        assert!(column > 0, "column should be one-based, got {column}");
    }

    #[test]
    fn a_binding_round_trips_a_string() {
        let mut engine = engine();
        engine.bind("shout", |value| Ok(value.to_uppercase())).expect("bind shout");
        assert_eq!(engine.eval("shout('hello')").expect("call shout"), "HELLO");
    }

    #[test]
    fn a_binding_that_fails_throws_catchable_in_javascript() {
        let mut engine = engine();
        engine.bind("refuse", |_| Err("no thanks".to_owned())).expect("bind refuse");
        let caught = engine
            .eval("try { refuse('x'); 'not thrown' } catch (error) { error.message }")
            .expect("catch the host error");
        assert_eq!(caught, "no thanks");
    }

    #[test]
    fn a_binding_keeps_its_captured_state_across_calls() {
        let mut engine = engine();
        let mut count = 0u32;
        engine
            .bind("tick", move |_| {
                count += 1;
                Ok(count.to_string())
            })
            .expect("bind tick");
        assert_eq!(engine.eval("tick('')").expect("first"), "1");
        assert_eq!(engine.eval("tick('')").expect("second"), "2");
        assert_eq!(engine.eval("[tick(''), tick('')].join(',')").expect("third"), "3,4");
    }

    #[test]
    fn a_binding_sees_what_javascript_passed() {
        let seen = Rc::new(RefCell::new(String::new()));
        let recorder = Rc::clone(&seen);
        let mut engine = engine();
        engine
            .bind("record", move |value| {
                recorder.borrow_mut().push_str(value);
                Ok(String::new())
            })
            .expect("bind record");
        engine.eval("record('payload')").expect("call record");
        assert_eq!(seen.borrow().as_str(), "payload");
    }

    #[test]
    fn binding_an_unusable_name_is_rejected() {
        let mut engine = engine();
        assert_eq!(
            engine.bind("app.log", |_| Ok(String::new())).expect_err("dotted name"),
            Error::InvalidBinding("app.log".to_owned())
        );
        assert_eq!(
            engine.bind("", |_| Ok(String::new())).expect_err("empty name"),
            Error::InvalidBinding(String::new())
        );
        assert_eq!(
            engine.bind("Math", |_| Ok(String::new())).expect_err("taken name"),
            Error::InvalidBinding("Math".to_owned())
        );
    }

    #[test]
    fn binding_the_same_name_twice_is_rejected() {
        let mut engine = engine();
        engine.bind("once", |_| Ok(String::new())).expect("first bind");
        assert_eq!(
            engine.bind("once", |_| Ok(String::new())).expect_err("second bind"),
            Error::InvalidBinding("once".to_owned())
        );
    }

    #[test]
    fn call_global_on_a_missing_global_is_not_an_error() {
        let mut engine = engine();
        assert_eq!(engine.call_global("onFrame", "{}").expect("missing global"), None);
        assert!(!engine.has_global("onFrame").expect("missing global"));
    }

    #[test]
    fn call_global_reaches_a_function_defined_by_a_previous_eval() {
        let mut engine = engine();
        engine.eval("globalThis.greet = (name) => `hi ${name}`;").expect("define greet");
        assert!(engine.has_global("greet").expect("greet exists"));
        assert_eq!(
            engine.call_global("greet", "sphere").expect("call greet"),
            Some("hi sphere".to_owned())
        );
    }

    #[test]
    fn call_global_propagates_an_exception_from_the_callee() {
        let mut engine = engine();
        engine
            .eval_named("globalThis.explode = () => { throw new Error('inside'); };", "host.js")
            .expect("define explode");
        let error = engine.call_global("explode", "").expect_err("callee throws");
        let (message, _, _, _, resource) = exception(&error);
        assert!(message.contains("inside"), "{message}");
        assert_eq!(resource, "host.js");
    }

    #[test]
    fn a_non_callable_global_is_an_error_rather_than_absent() {
        let mut engine = engine();
        engine.eval("globalThis.notAFunction = 42;").expect("define value");
        assert!(!engine.has_global("notAFunction").expect("not callable"));
        assert!(engine.call_global("notAFunction", "").is_err());
    }

    #[test]
    fn run_microtasks_runs_a_queued_promise_continuation() {
        let mut engine = engine();
        engine
            .eval(
                "globalThis.resolved = false;\
                 Promise.resolve().then(() => { globalThis.resolved = true; });",
            )
            .expect("queue a continuation");
        assert_eq!(engine.eval("resolved").expect("still pending"), "false");
        engine.run_microtasks().expect("drain microtasks");
        assert_eq!(engine.eval("resolved").expect("now resolved"), "true");
    }

    #[test]
    fn a_binding_can_be_called_from_a_promise_continuation() {
        let mut engine = engine();
        engine.bind("stamp", |value| Ok(format!("[{value}]"))).expect("bind stamp");
        engine
            .eval("globalThis.out = ''; Promise.resolve('x').then((v) => { out = stamp(v); });")
            .expect("queue a continuation");
        engine.run_microtasks().expect("drain microtasks");
        assert_eq!(engine.eval("out").expect("read out"), "[x]");
    }

    #[test]
    fn pump_is_safe_to_call_with_an_empty_task_queue() {
        let mut engine = engine();
        engine.pump().expect("first pump");
        engine.pump().expect("second pump");
    }

    #[test]
    fn the_welcome_script_runs_and_exposes_its_entry_points() {
        let mut engine = engine();
        let greeting = engine.eval_named(WELCOME, "welcome.js").expect("run welcome.js");
        assert!(greeting.contains("SphereKit"), "{greeting}");
        assert!(engine.has_global("welcome").expect("welcome is callable"));
        assert_eq!(
            engine.call_global("welcome", "world").expect("call welcome"),
            Some("Hello, world! Running on V8 via SphereKit.".to_owned())
        );
        assert_eq!(engine.eval("microtaskRan").expect("not yet"), "false");
        engine.run_microtasks().expect("drain microtasks");
        assert_eq!(engine.eval("microtaskRan").expect("now"), "true");
    }

    #[test]
    fn the_version_is_reported() {
        assert!(Engine::version().starts_with(char::is_numeric), "{}", Engine::version());
        assert_eq!(v8_version(), Engine::version());
    }
}
