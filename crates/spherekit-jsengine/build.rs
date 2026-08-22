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

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        println!(
            "cargo:warning=spherekit-jsengine V8 prebuilt is currently available for Windows only"
        );
        return;
    }

    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if target_arch != "x86_64" {
        panic!("spherekit-jsengine currently supports only the Windows x86_64 V8 prebuilt");
    }

    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let backend_dir = manifest_dir.join("v8backend");

    if !has_complete_backend(&backend_dir) {
        let lock = acquire_lock(&manifest_dir.join(".v8backend.lock"))
            .unwrap_or_else(|error| panic!("cannot lock V8 backend setup: {error}"));

        // Another Cargo process may have finished the setup while this build
        // waited for the lock.
        if !has_complete_backend(&backend_dir) {
            let url = env::var("SPHEREKIT_V8_URL").unwrap_or_else(|_| DEFAULT_V8_URL.into());
            download_and_extract(&url, &backend_dir);
        }

        drop(lock);
    }

    if !has_complete_backend(&backend_dir) {
        panic!("V8 backend setup finished without the required files in {}", backend_dir.display());
    }

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

fn download_and_extract(url: &str, backend_dir: &Path) {
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
        .unwrap_or_else(|error| panic!("failed to start PowerShell for V8 setup: {error}"));

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!(
            "V8 download/extract failed with {}\nstdout:\n{}\nstderr:\n{}",
            output.status, stdout, stderr
        );
    }

    println!("cargo:warning=V8 prebuilt extracted to {}", backend_dir.display());
}
