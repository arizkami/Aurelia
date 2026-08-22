//! `spherekit` project and build command-line interface.

use serde_json::Value;
use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REPOSITORY_ROOT: &str = env!("CARGO_MANIFEST_DIR");

type Result<T> = std::result::Result<T, CliError>;

#[derive(Debug)]
struct CliError(String);

impl CliError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<io::Error> for CliError {
    fn from(error: io::Error) -> Self {
        Self(error.to_string())
    }
}

fn main() {
    if let Err(error) = run(env::args_os().skip(1).collect()) {
        eprintln!("spherekit: {error}");
        std::process::exit(1);
    }
}

fn run(args: Vec<OsString>) -> Result<()> {
    let command = args.first().and_then(|value| value.to_str()).unwrap_or("help");
    match command {
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        "version" | "--version" | "-V" => {
            println!("spherekit {VERSION}");
            Ok(())
        }
        "react" => scaffold_react(Options::parse(&args[1..])?),
        "build" => build_project(Options::parse(&args[1..])?),
        other => {
            Err(CliError::new(format!("unknown command `{other}`; run `spherekit help` for usage")))
        }
    }
}

#[derive(Debug, Default)]
struct Options {
    positionals: Vec<String>,
    values: BTreeMap<String, String>,
    flags: BTreeMap<String, bool>,
}

impl Options {
    fn parse(args: &[OsString]) -> Result<Self> {
        let mut options = Self::default();
        let mut index = 0;
        while index < args.len() {
            let value = args[index].to_string_lossy();
            if let Some(option) = value.strip_prefix("--") {
                if let Some((key, value)) = option.split_once('=') {
                    options.values.insert(key.to_owned(), value.to_owned());
                } else if matches!(
                    option,
                    "force" | "skip-install" | "dry-run" | "release" | "no-react" | "no-rust"
                ) {
                    options.flags.insert(option.to_owned(), true);
                } else {
                    let next = args.get(index + 1).ok_or_else(|| {
                        CliError::new(format!("option `--{option}` needs a value"))
                    })?;
                    if next.to_string_lossy().starts_with('-') {
                        return Err(CliError::new(format!("option `--{option}` needs a value")));
                    }
                    options.values.insert(option.to_owned(), next.to_string_lossy().into_owned());
                    index += 1;
                }
            } else if value.starts_with('-') {
                return Err(CliError::new(format!("unknown option `{value}`")));
            } else {
                options.positionals.push(value.into_owned());
            }
            index += 1;
        }
        Ok(options)
    }

    fn value(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    fn flag(&self, key: &str) -> bool {
        self.flags.get(key).copied().unwrap_or(false)
    }
}

fn scaffold_react(options: Options) -> Result<()> {
    let name = options
        .positionals
        .first()
        .cloned()
        .ok_or_else(|| CliError::new("usage: spherekit react <name> [options]"))?;
    validate_project_name(&name)?;

    let root = options
        .value("path")
        .map(PathBuf::from)
        .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(&name));
    let target = options.value("target").unwrap_or("current");
    validate_target(target)?;
    let package_manager = options.value("package-manager").unwrap_or("auto");
    validate_package_manager(package_manager)?;

    if root.exists() {
        let has_entries = fs::read_dir(&root)?.next().transpose()?.is_some();
        if has_entries && !options.flag("force") {
            return Err(CliError::new(format!(
                "destination {} is not empty; use `--force` to overwrite generated files",
                root.display()
            )));
        }
    } else {
        fs::create_dir_all(&root)?;
    }

    let crate_name = rust_crate_name(&name);
    if crate_name.is_empty() {
        return Err(CliError::new("project name must contain at least one letter or digit"));
    }
    let (npm_dependency, cargo_react_dependency, cargo_bridge_dependency) =
        local_dependencies(&root);
    for &(relative, bytes) in TEMPLATE_FILES {
        let relative = Path::new(relative);
        let destination = root.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let source = String::from_utf8(bytes.to_vec()).map_err(|_| {
            CliError::new(format!("template file {} is not UTF-8", relative.display()))
        })?;
        let rendered = source
            .replace("{{PROJECT_NAME}}", &name)
            .replace("{{RUST_CRATE_NAME}}", &crate_name)
            .replace("{{TARGET}}", target)
            .replace("{{SPHEREKIT_VERSION}}", VERSION)
            .replace("{{SPHEREKIT_REACT_NPM_DEP}}", &npm_dependency)
            .replace("{{SPHEREKIT_REACT_CARGO_DEP}}", &cargo_react_dependency)
            .replace("{{SPHEREKIT_BRIDGE_CARGO_DEP}}", &cargo_bridge_dependency);
        fs::write(&destination, rendered)?;
    }

    println!("created SphereKit React app at {}", root.display());
    println!("target: {target}");
    let manager = if package_manager == "auto" {
        detect_package_manager().unwrap_or_else(|| "bun".into())
    } else {
        package_manager.to_owned()
    };
    if options.flag("skip-install") {
        println!("next: {manager} install && spherekit build");
    } else {
        run_tool(&manager, &["install"], &root, false)?;
        println!("next: spherekit build");
    }
    Ok(())
}

fn build_project(options: Options) -> Result<()> {
    let root = options.value("path").map(PathBuf::from).unwrap_or(env::current_dir()?);
    let root = root.canonicalize().map_err(|error| {
        CliError::new(format!("cannot open project {}: {error}", root.display()))
    })?;
    let target = options.value("target").unwrap_or("current");
    validate_target(target)?;
    let manager_name = options.value("package-manager").unwrap_or("auto");
    validate_package_manager(manager_name)?;
    let manager = if manager_name == "auto" {
        detect_package_manager_in(&root).ok_or_else(|| {
            CliError::new("no package manager found; install Bun or npm, or pass --package-manager")
        })?
    } else {
        manager_name.to_owned()
    };
    let dry_run = options.flag("dry-run");
    let release = options.flag("release");
    let no_react = options.flag("no-react");
    let no_rust = options.flag("no-rust");
    let mut ran = false;

    let package_json = root.join("package.json");
    if !no_react && package_json.is_file() {
        run_tool(&manager, &["run", "typecheck"], &root, dry_run)?;
        if package_has_script(&package_json, "build")? {
            run_tool(&manager, &["run", "build"], &root, dry_run)?;
        } else {
            println!("spherekit: package.json has no build script; typecheck completed");
        }
        ran = true;
    }

    if !no_rust {
        let manifest = find_rust_manifest(&root)?;
        let mut args =
            vec!["build".to_owned(), "--manifest-path".into(), manifest.display().to_string()];
        if release {
            args.push("--release".into());
        }
        if let Some(triple) = target_triple(target)
            .map_err(|_| CliError::new(format!("unsupported target `{target}`")))?
        {
            args.push("--target".into());
            args.push(triple.to_owned());
        }
        let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        let mut command = Command::new("cargo");
        command.args(&arg_refs).current_dir(&root);
        if let Some(out_dir) = options.value("out-dir") {
            let out_dir = PathBuf::from(out_dir);
            fs::create_dir_all(&out_dir)?;
            command.env("CARGO_TARGET_DIR", out_dir);
        }
        println!("$ cargo {}", args.join(" "));
        if !dry_run {
            ensure_success(command.status()?, "cargo build")?;
        }
        ran = true;
    }

    if !ran {
        return Err(CliError::new(
            "nothing to build; expected package.json and/or src/app/Cargo.toml",
        ));
    }
    println!("SphereKit build complete");
    Ok(())
}

fn find_rust_manifest(root: &Path) -> Result<PathBuf> {
    let candidates = [root.join("src/app/Cargo.toml"), root.join("Cargo.toml")];
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| CliError::new(format!("no Rust manifest found under {}", root.display())))
}

fn package_has_script(path: &Path, script: &str) -> Result<bool> {
    let text = fs::read_to_string(path)?;
    let json: Value = serde_json::from_str(&text)
        .map_err(|error| CliError::new(format!("invalid {}: {error}", path.display())))?;
    Ok(json
        .get("scripts")
        .and_then(Value::as_object)
        .is_some_and(|scripts| scripts.contains_key(script)))
}

fn run_tool(tool: &str, args: &[&str], cwd: &Path, dry_run: bool) -> Result<()> {
    println!("$ {} {}", tool, args.join(" "));
    if dry_run {
        return Ok(());
    }
    let status = Command::new(tool).args(args).current_dir(cwd).status().map_err(|error| {
        CliError::new(format!(
            "could not run `{tool}`: {error}; install it or choose another --package-manager"
        ))
    })?;
    ensure_success(status, tool)
}

fn ensure_success(status: ExitStatus, command: &str) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        Err(CliError::new(format!("`{command}` exited with {status}")))
    }
}

fn detect_package_manager() -> Option<String> {
    detect_package_manager_in(&env::current_dir().ok()?)
}

fn detect_package_manager_in(root: &Path) -> Option<String> {
    if root.join("bun.lock").is_file() || root.join("bun.lockb").is_file() {
        Some("bun".into())
    } else if root.join("pnpm-lock.yaml").is_file() {
        Some("pnpm".into())
    } else if root.join("yarn.lock").is_file() {
        Some("yarn".into())
    } else if root.join("package-lock.json").is_file() {
        Some("npm".into())
    } else if command_exists("bun") {
        Some("bun".into())
    } else if command_exists("npm") {
        Some("npm".into())
    } else {
        None
    }
}

fn command_exists(command: &str) -> bool {
    Command::new(command).arg("--version").output().is_ok_and(|output| output.status.success())
}

fn validate_project_name(name: &str) -> Result<()> {
    if name == "." || name == ".." || name.is_empty() || name.contains(['/', '\\']) {
        return Err(CliError::new("project name must be a non-empty directory name"));
    }
    if name.starts_with('.') {
        return Err(CliError::new("project name must not start with `.`"));
    }
    Ok(())
}

fn validate_package_manager(manager: &str) -> Result<()> {
    if matches!(manager, "auto" | "bun" | "npm" | "pnpm" | "yarn") {
        Ok(())
    } else {
        Err(CliError::new(format!("unsupported package manager `{manager}`")))
    }
}

fn validate_target(target: &str) -> Result<()> {
    if target_triple(target).is_ok() {
        Ok(())
    } else {
        Err(CliError::new(format!(
            "unsupported target `{target}`; use current, windows, macos, linux, or a Rust target triple"
        )))
    }
}

fn target_triple(target: &str) -> std::result::Result<Option<&'static str>, ()> {
    match target {
        "current" | "host" => Ok(None),
        "windows" => Ok(Some("x86_64-pc-windows-msvc")),
        "macos" | "darwin" => Ok(Some("aarch64-apple-darwin")),
        "linux" => Ok(Some("x86_64-unknown-linux-gnu")),
        "x86_64-pc-windows-msvc"
        | "aarch64-pc-windows-msvc"
        | "x86_64-apple-darwin"
        | "aarch64-apple-darwin"
        | "x86_64-unknown-linux-gnu"
        | "aarch64-unknown-linux-gnu" => Ok(Some(match target {
            "x86_64-pc-windows-msvc" => "x86_64-pc-windows-msvc",
            "aarch64-pc-windows-msvc" => "aarch64-pc-windows-msvc",
            "x86_64-apple-darwin" => "x86_64-apple-darwin",
            "aarch64-apple-darwin" => "aarch64-apple-darwin",
            "x86_64-unknown-linux-gnu" => "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu" => "aarch64-unknown-linux-gnu",
            _ => unreachable!(),
        })),
        _ => Err(()),
    }
}

fn rust_crate_name(name: &str) -> String {
    let mut crate_name =
        name.chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() { character.to_ascii_lowercase() } else { '_' }
            })
            .collect::<String>()
            .trim_matches('_')
            .to_owned();
    if crate_name.starts_with(|character: char| character.is_ascii_digit()) {
        crate_name.insert(0, '_');
    }
    crate_name
}

fn local_dependencies(project_root: &Path) -> (String, String, String) {
    let Some(repository_root) = Path::new(REPOSITORY_ROOT).parent().and_then(Path::parent) else {
        return registry_dependencies();
    };
    let react_source = repository_root.join("crates/spherekit-react");
    let bridge_source = repository_root.join("crates/spherekit-bridge");
    if !react_source.is_dir() || !bridge_source.is_dir() {
        return registry_dependencies();
    }

    let npm_path = relative_path(project_root, &react_source)
        .unwrap_or_else(|| react_source.to_string_lossy().into_owned());
    let cargo_base = project_root.join("src/app");
    let react_path = relative_path(&cargo_base, &react_source)
        .unwrap_or_else(|| react_source.to_string_lossy().into_owned());
    let bridge_path = relative_path(&cargo_base, &bridge_source)
        .unwrap_or_else(|| bridge_source.to_string_lossy().into_owned());

    (
        format!("\"file:{npm_path}\""),
        format!("{{ path = \"{react_path}\" }}"),
        format!("{{ path = \"{bridge_path}\" }}"),
    )
}

fn registry_dependencies() -> (String, String, String) {
    (format!("\"^{VERSION}\""), format!("\"^{VERSION}\""), format!("\"^{VERSION}\""))
}

fn relative_path(from: &Path, to: &Path) -> Option<String> {
    let from = normalized_path(&canonical_or_virtual(from)?);
    let to = normalized_path(&canonical_or_virtual(to)?);
    let from_parts = from.split('/').map(str::to_owned).collect::<Vec<_>>();
    let to_parts = to.split('/').map(str::to_owned).collect::<Vec<_>>();
    let common = from_parts
        .iter()
        .zip(&to_parts)
        .take_while(|(left, right)| left.eq_ignore_ascii_case(right))
        .count();
    if common == 0 {
        return Some(to);
    }
    let mut parts = Vec::new();
    parts.extend(std::iter::repeat_n("..".to_owned(), from_parts.len().saturating_sub(common)));
    parts.extend(to_parts[common..].iter().cloned());
    if parts.is_empty() { Some(".".into()) } else { Some(parts.join("/")) }
}

fn canonical_or_virtual(path: &Path) -> Option<PathBuf> {
    if path.exists() {
        return path.canonicalize().ok();
    }
    let parent = canonical_or_virtual(path.parent()?)?;
    Some(parent.join(path.file_name()?))
}

fn normalized_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .trim_start_matches("//?/")
        .trim_end_matches('/')
        .to_owned()
}

fn print_help() {
    println!(
        "SphereKit {VERSION}\n\n\
Usage:\n  spherekit react <name> [options]\n  spherekit build [options]\n\n\
Commands:\n  react     Create a cross-platform React + Rust SphereKit app\n  build     Typecheck/compile React and compile the native Rust app\n\n\
react options:\n  --path <dir>                 Destination directory\n  --target <platform>         current, windows, macos, linux, or Rust triple\n  --package-manager <name>     auto, bun, npm, pnpm, or yarn\n  --skip-install               Create files without installing packages\n  --force                      Overwrite generated files in an existing folder\n\n\
build options:\n  --path <dir>                 Project directory (default: current directory)\n  --target <platform>         current, windows, macos, linux, or Rust triple\n  --package-manager <name>     auto, bun, npm, pnpm, or yarn\n  --release                    Build the native app with Cargo release profile\n  --out-dir <dir>              Cargo target directory\n  --no-react / --no-rust       Skip one side of the build\n  --dry-run                    Print commands without executing them"
    );
}

type Template = (&'static str, &'static [u8]);

const TEMPLATE_FILES: &[Template] = &[
    ("package.json", include_bytes!("../../../template/spherekit-app-react/package.json")),
    ("README.md", include_bytes!("../../../template/spherekit-app-react/README.md")),
    ("spherekit.toml", include_bytes!("../../../template/spherekit-app-react/spherekit.toml")),
    ("tsconfig.json", include_bytes!("../../../template/spherekit-app-react/tsconfig.json")),
    (
        "tsconfig.build.json",
        include_bytes!("../../../template/spherekit-app-react/tsconfig.build.json"),
    ),
    (
        "src/renderer/App.tsx",
        include_bytes!("../../../template/spherekit-app-react/src/renderer/App.tsx"),
    ),
    (
        "src/renderer/main.tsx",
        include_bytes!("../../../template/spherekit-app-react/src/renderer/main.tsx"),
    ),
    (
        "src/renderer/index.ts",
        include_bytes!("../../../template/spherekit-app-react/src/renderer/index.ts"),
    ),
    (
        "src/app/Cargo.toml",
        include_bytes!("../../../template/spherekit-app-react/src/app/Cargo.toml"),
    ),
    (
        "src/app/src/main.rs",
        include_bytes!("../../../template/spherekit-app-react/src/app/src/main.rs"),
    ),
    (
        "src/app/src/lib.rs",
        include_bytes!("../../../template/spherekit-app-react/src/app/src/lib.rs"),
    ),
    (".gitignore", include_bytes!("../../../template/spherekit-app-react/.gitignore")),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flags_and_values() {
        let options = Options::parse(&[
            OsString::from("demo"),
            OsString::from("--target"),
            OsString::from("linux"),
            OsString::from("--skip-install"),
        ])
        .expect("options parse");
        assert_eq!(options.positionals, ["demo"]);
        assert_eq!(options.value("target"), Some("linux"));
        assert!(options.flag("skip-install"));
    }

    #[test]
    fn accepts_supported_machine_targets() {
        for target in ["current", "windows", "macos", "linux", "aarch64-apple-darwin"] {
            validate_target(target).expect("target should be accepted");
        }
    }

    #[test]
    fn rejects_unsafe_project_names() {
        assert!(validate_project_name("../outside").is_err());
        assert!(validate_project_name(".hidden").is_err());
        assert!(validate_project_name("my-app").is_ok());
    }

    #[test]
    fn every_embedded_template_file_exists() {
        assert!(TEMPLATE_FILES.len() >= 10);
    }
}
