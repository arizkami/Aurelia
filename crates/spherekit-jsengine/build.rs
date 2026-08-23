use std::env;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_V8_URL: &str =
    "https://github.com/futureboard/SphereEngine/releases/download/2026.04/v8-windows_x64.zip";

const REQUIRED_FILES: &[&str] =
    &["include/v8.h", "include/libplatform/libplatform.h", "lib/v8_monolith.lib", "bin/icudtl.dat"];

struct BuildLock {
    path: PathBuf,
    _file: File,
}

impl Drop for BuildLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SPHEREKIT_V8_URL");
    println!("cargo:rerun-if-env-changed=SPHEREKIT_V8_OFFLINE");
    println!("cargo:rerun-if-env-changed=SPHEREKIT_V8_DIR");
    // Declared so `unexpected_cfgs` keeps working on the gate below rather than
    // treating every use of it as a typo.
    println!("cargo::rustc-check-cfg=cfg(v8_backend)");

    // When this is `None` no `v8_backend` cfg is emitted, so the crate compiles
    // its unsupported-platform stub: every method still exists, and every one
    // returns `Error::UnsupportedPlatform`.
    if let Some(backend_dir) = locate_backend() {
        link_v8(&backend_dir);
    }
}

/// Finds the V8 prebuilt, downloading it if that is possible and permitted.
///
/// Returns `None` rather than panicking when V8 cannot be had. A build script
/// that reaches the network is a liability for exactly the environments that
/// cannot tell you why they failed: docs.rs builds with networking off and
/// would show this crate as permanently broken, an offline or air-gapped build
/// would abort, and a CI runner behind a proxy would fail on a dependency it
/// never asked for. Degrading to the stub keeps `cargo build` working
/// everywhere and confines the loss to the one crate that needs V8.
fn locate_backend() -> Option<PathBuf> {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if target_os != "windows" || target_arch != "x86_64" {
        println!(
            "cargo:warning=spherekit-jsengine: the V8 prebuilt is Windows x86_64 only; \
             building the unsupported-platform stub for {target_os}-{target_arch}"
        );
        return None;
    }

    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));

    // An explicitly supplied prebuilt wins, then a checkout-local one. Both are
    // read-only here: nothing below ever writes into the manifest directory.
    for candidate in
        [env::var_os("SPHEREKIT_V8_DIR").map(PathBuf::from), Some(manifest_dir.join("v8backend"))]
            .into_iter()
            .flatten()
    {
        if has_complete_backend(&candidate) {
            return Some(candidate);
        }
    }

    // The download target is a user-level cache, never the crate directory.
    // Cargo forbids a build script from modifying its own package during
    // `cargo package`, and rightly: a 74 MB extraction inside the source tree
    // is not something a publish should be carrying, and `OUT_DIR` alone would
    // re-download it for every profile and every target directory.
    let backend_dir = match cache_dir() {
        Some(dir) => dir,
        None => {
            println!(
                "cargo:warning=spherekit-jsengine: no cache directory available; \
                 building the unsupported-platform stub"
            );
            return None;
        }
    };
    if has_complete_backend(&backend_dir) {
        return Some(backend_dir);
    }

    if env::var_os("SPHEREKIT_V8_OFFLINE").is_some() {
        println!(
            "cargo:warning=spherekit-jsengine: SPHEREKIT_V8_OFFLINE is set and no prebuilt is \
             present; building the unsupported-platform stub"
        );
        return None;
    }

    let lock = match acquire_lock(&backend_dir.with_extension("lock")) {
        Ok(lock) => lock,
        Err(error) => {
            println!("cargo:warning=spherekit-jsengine: cannot lock V8 setup: {error}");
            return None;
        }
    };

    // Another Cargo process may have finished the setup while this build waited
    // for the lock.
    if !has_complete_backend(&backend_dir) {
        let url = env::var("SPHEREKIT_V8_URL").unwrap_or_else(|_| DEFAULT_V8_URL.into());
        if let Err(error) = download_and_extract(&url, &backend_dir) {
            println!("cargo:warning=spherekit-jsengine: {error}");
        }
    }
    drop(lock);

    if has_complete_backend(&backend_dir) {
        return Some(backend_dir);
    }

    // Loud, because on Windows this is the difference between a working
    // JavaScript runtime and one whose every call returns
    // `Error::UnsupportedPlatform` — and the code still compiles either way.
    println!(
        "cargo:warning=spherekit-jsengine: V8 IS UNAVAILABLE. Every Engine method will return \
         Error::UnsupportedPlatform. Extract the prebuilt to {} or set SPHEREKIT_V8_URL, then \
         rebuild.",
        backend_dir.display()
    );
    None
}

/// Compiles the shim against the prebuilt and emits the link contract.
fn link_v8(backend_dir: &Path) {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let lib_dir = backend_dir.join("lib");
    let include_dir = backend_dir.join("include");
    let shim = manifest_dir.join("src").join("v8_shim.cc");

    // These defines must match the downloaded monolith. V8::Initialize checks
    // the embedder configuration at runtime; this prebuilt uses compressed
    // pointers and does not enable the sandbox.
    cc::Build::new()
        .cpp(true)
        .std("c++20")
        .include(&include_dir)
        .file(&shim)
        .define("V8_COMPRESS_POINTERS", None)
        .flag_if_supported("/EHsc")
        .flag_if_supported("/Zc:__cplusplus")
        .static_crt(true)
        .warnings(false)
        .compile("spherekit_v8_shim");

    println!("cargo:rerun-if-changed={}", shim.display());
    println!(
        "cargo:rustc-env=SPHEREKIT_V8_ICU_DATA_PATH={}",
        backend_dir.join("bin").join("icudtl.dat").display()
    );
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=v8_monolith");

    // v8_monolith.lib is a static Windows build and carries dependencies on
    // these system libraries. Emitting them here keeps consumers from having
    // to duplicate V8's platform link contract.
    for library in [
        "advapi32", "bcrypt", "dbghelp", "ole32", "shell32", "shlwapi", "user32", "userenv",
        "version", "winmm", "ws2_32",
    ] {
        println!("cargo:rustc-link-lib={library}");
    }

    // Only now, once the shim has compiled and V8 is actually linked, does the
    // real implementation get switched on.
    println!("cargo::rustc-cfg=v8_backend");
}

/// Where a downloaded prebuilt is cached, keyed by the archive it came from.
///
/// Keyed rather than fixed because changing `SPHEREKIT_V8_URL` has to produce a
/// different directory; sharing one would silently link the previous archive's
/// V8 against the new headers.
fn cache_dir() -> Option<PathBuf> {
    let url = env::var("SPHEREKIT_V8_URL").unwrap_or_else(|_| DEFAULT_V8_URL.into());
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in url.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }

    let root = env::var_os("LOCALAPPDATA")
        .or_else(|| env::var_os("XDG_CACHE_HOME"))
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(root.join("spherekit").join(format!("v8-{hash:016x}")))
}

fn has_complete_backend(backend_dir: &Path) -> bool {
    REQUIRED_FILES.iter().all(|relative| backend_dir.join(relative).is_file())
}

fn acquire_lock(path: &Path) -> io::Result<BuildLock> {
    let started = Instant::now();
    loop {
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(file) => return Ok(BuildLock { path: path.to_path_buf(), _file: file }),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if started.elapsed() > Duration::from_secs(120) {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("timed out waiting for {}", path.display()),
                    ));
                }
                thread::sleep(Duration::from_millis(250));
            }
            Err(error) => return Err(error),
        }
    }
}

fn download_and_extract(url: &str, backend_dir: &Path) -> Result<(), String> {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    let archive = out_dir.join("v8-windows_x64.zip");
    let stage = out_dir.join("v8backend-stage");

    println!("cargo:warning=V8 prebuilt is missing; downloading {url}");

    let script = r#"
$ErrorActionPreference = 'Stop'
$archive = $env:SPHEREKIT_V8_ARCHIVE
$stage = $env:SPHEREKIT_V8_STAGE
$target = $env:SPHEREKIT_V8_TARGET

if (Test-Path -LiteralPath $archive) { Remove-Item -LiteralPath $archive -Force }
if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
New-Item -ItemType Directory -Path $stage -Force | Out-Null

Invoke-WebRequest -UseBasicParsing -Uri $env:SPHEREKIT_V8_URL -OutFile $archive
Expand-Archive -LiteralPath $archive -DestinationPath $stage -Force

$payload = $stage
if (-not (Test-Path -LiteralPath (Join-Path $payload 'include'))) {
    $candidate = Get-ChildItem -LiteralPath $stage -Directory |
        Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'include') } |
        Select-Object -First 1
    if ($null -eq $candidate) { throw 'V8 archive does not contain an include directory' }
    $payload = $candidate.FullName
}

foreach ($relative in @('include/v8.h', 'include/libplatform/libplatform.h', 'lib/v8_monolith.lib', 'bin/icudtl.dat')) {
    if (-not (Test-Path -LiteralPath (Join-Path $payload $relative))) {
        throw "V8 archive is missing $relative"
    }
}

if (Test-Path -LiteralPath $target) { Remove-Item -LiteralPath $target -Recurse -Force }
New-Item -ItemType Directory -Path $target -Force | Out-Null
Copy-Item -Path (Join-Path $payload '*') -Destination $target -Recurse -Force
New-Item -ItemType File -Path (Join-Path $target '.gitkeep') -Force | Out-Null
Remove-Item -LiteralPath $stage -Recurse -Force
Remove-Item -LiteralPath $archive -Force
"#;

    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script])
        .env("SPHEREKIT_V8_URL", url)
        .env("SPHEREKIT_V8_ARCHIVE", &archive)
        .env("SPHEREKIT_V8_STAGE", &stage)
        .env("SPHEREKIT_V8_TARGET", backend_dir)
        .output()
        .map_err(|error| format!("could not start PowerShell for V8 setup: {error}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Reduced to one line on purpose: this travels as a `cargo:warning`,
        // and every Cargo frontend renders a multi-line one as an unreadable
        // block with `warning:` glued to the front of each line.
        let detail = stderr.lines().find(|line| !line.trim().is_empty()).unwrap_or("no detail");
        return Err(format!("V8 download failed ({}): {}", output.status, detail.trim()));
    }

    println!("cargo:warning=V8 prebuilt extracted to {}", backend_dir.display());
    Ok(())
}
