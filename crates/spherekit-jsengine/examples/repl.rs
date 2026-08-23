//! Evaluates a JavaScript file with the bundled V8 engine.
//!
//! Useful as the smallest possible embedder: it does exactly what a real host
//! does per frame — evaluate, then drain microtasks and platform tasks — so a
//! script whose work happens in a promise continuation still finishes.
//!
//! ```text
//! cargo run -p spherekit-jsengine --example repl -- crates/spherekit-jsengine/test/welcome.js
//! ```

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use spherekit_jsengine::{Engine, Error};
    use std::process::ExitCode;

    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: repl <script.js>");
        return ExitCode::FAILURE;
    };

    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("cannot read {path}: {error}");
            return ExitCode::FAILURE;
        }
    };

    // V8 only ever sees the file name, not the full path: that is what ends up
    // in every stack frame, and an absolute Windows path makes traces unreadable.
    let name = std::path::Path::new(&path)
        .file_name()
        .map_or_else(|| path.clone(), |name| name.to_string_lossy().into_owned());

    let mut engine = match Engine::new() {
        Ok(engine) => engine,
        Err(error) => {
            eprintln!("cannot start V8: {error}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("V8 {}", Engine::version());

    let outcome = engine
        .eval_named(&source, &name)
        .and_then(|value| {
            println!("{value}");
            engine.run_microtasks()
        })
        .and_then(|()| engine.pump());

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(Error::Exception { message, stack, line, column, resource }) => {
            eprintln!("{resource}:{line}:{column}: {message}");
            if !stack.is_empty() {
                eprintln!("{stack}");
            }
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("spherekit-jsengine currently supports Windows x86_64 only");
}
