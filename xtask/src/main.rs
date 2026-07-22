use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use sha2::{Digest, Sha256};

mod closeout_impact;
mod test_estate;
mod verify;

const PERSON_SHA: &str = "a65415f0da868f59014777ace1b702f6d7c6274c18e5af3e344cf710c37526ea";
const EMPTY_SHA: &str = "2d4c35233e497d1c81d2a08187e856b5aba84acaf4f10cb47ccd33b9b5edee63";
const MODEL_SHA: &str = "9de513de589ac98bb92d3bca53b5af7b9acfa9b0bacb831f7999d0f7afaee8f0";
const MEDIAMTX_VERSION: &str = "v1.19.1";
const ZIG_VERSION: &str = "0.13.0";

fn main() -> ExitCode {
    match run(env::args_os().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(2)
        }
    }
}

fn run(args: Vec<OsString>) -> Result<(), String> {
    match args.first().and_then(|arg| arg.to_str()) {
        Some("setup-harness") => setup_harness(),
        Some("setup-runtime-harness") => setup_runtime_harness(),
        Some("test-estate-check") => test_estate::check(&args[1..]),
        Some("test-estate-proposal") => test_estate::propose_ledger(&args[1..]),
        Some("documentation-contract-proposal") => {
            test_estate::propose_documentation_contracts(&args[1..])
        }
        Some("test-contract-proposal") => test_estate::propose_test_contracts(&args[1..]),
        Some("test-baseline-proposal") => test_estate::propose_test_baseline(&args[1..]),
        Some("closeout-impact") => closeout_impact::run(&args[1..]),
        Some("verify") => verify::run(&args[1..]),
        _ => Err(
            "usage: cargo xtask <setup-harness|setup-runtime-harness|test-estate-check [--docs PATH] [--nextest-json SHAPE=PATH]...|test-estate-proposal|documentation-contract-proposal [--docs PATH]|test-contract-proposal|test-baseline-proposal [--root PATH]|closeout-impact --base SHA --head SHA --json PATH|verify <change|dev-closeout|release> ...>"
                .to_string(),
        ),
    }
}

/// Install only the process-test prerequisites used by the default and feature
/// lanes. The full harness also prepares cross-musl Zig linkers; making the
/// cheap runtime subset explicit keeps fast CI from relying on a stale cached
/// MediaMTX binary without paying the cross-build setup cost.
pub(crate) fn setup_runtime_harness() -> Result<(), String> {
    let repo_root = repo_root()?;
    let fixture_dir = repo_root.join("tests/fixtures/video");
    let cache_dir = repo_root.join("tests/fixtures/.cache");
    let tool_dir = repo_root.join(format!(
        "target/vigil-test-tools/mediamtx/{MEDIAMTX_VERSION}"
    ));
    let model_file = repo_root.join("tests/fixtures/models/yolox-tiny-coco.pth");
    let person_clip = fixture_dir.join("one-by-one-person-detection.mp4");
    let empty_clip = fixture_dir.join("empty-scene-from-one-by-one-person-detection.mp4");

    let ffmpeg = require_tool("ffmpeg")?;
    let ffprobe = require_tool("ffprobe")?;
    require_tool("mosquitto")?;
    require_tool("mosquitto_pub")?;
    require_tool("mosquitto_sub")?;
    require_tool("curl")?;
    require_tool("tar")?;

    verify_sha(PERSON_SHA, &person_clip)?;
    verify_sha(EMPTY_SHA, &empty_clip)?;
    verify_sha(MODEL_SHA, &model_file)?;
    ffprobe_video(&ffprobe, &person_clip)?;
    ffprobe_video(&ffprobe, &empty_clip)?;

    fs::create_dir_all(&cache_dir).map_err(|error| format!("create cache dir: {error}"))?;
    fs::create_dir_all(&tool_dir).map_err(|error| format!("create mediamtx dir: {error}"))?;
    let mediamtx = mediamtx_asset()?;
    let mediamtx_archive = cache_dir.join(mediamtx.asset);
    download_if_missing(&mediamtx.url, &mediamtx_archive)?;
    verify_sha(mediamtx.sha, &mediamtx_archive)?;
    run_command(
        Command::new("tar")
            .arg("-xzf")
            .arg(&mediamtx_archive)
            .arg("-C")
            .arg(&tool_dir)
            .arg("mediamtx"),
        "extract mediamtx",
    )?;
    make_executable(&tool_dir.join("mediamtx"))?;

    println!("ffmpeg: {}", ffmpeg.display());
    println!("ffprobe: {}", ffprobe.display());
    println!("mediamtx: {}", tool_dir.join("mediamtx").display());
    println!("person fixture: {}", person_clip.display());
    println!("empty fixture: {}", empty_clip.display());
    println!("model fixture: {}", model_file.display());
    Ok(())
}

pub(crate) fn setup_harness() -> Result<(), String> {
    let repo_root = repo_root()?;
    let fixture_dir = repo_root.join("tests/fixtures/video");
    let cache_dir = repo_root.join("tests/fixtures/.cache");
    let tool_dir = repo_root.join(format!(
        "target/vigil-test-tools/mediamtx/{MEDIAMTX_VERSION}"
    ));
    let zig_tool_dir = repo_root.join("target/vigil-test-tools/zig");
    let model_dir = repo_root.join("tests/fixtures/models");

    let person_clip = fixture_dir.join("one-by-one-person-detection.mp4");
    let empty_clip = fixture_dir.join("empty-scene-from-one-by-one-person-detection.mp4");
    let model_file = model_dir.join("yolox-tiny-coco.pth");

    let ffmpeg = require_tool("ffmpeg")?;
    let ffprobe = require_tool("ffprobe")?;
    require_tool("mosquitto")?;
    require_tool("mosquitto_pub")?;
    require_tool("mosquitto_sub")?;
    require_tool("curl")?;
    require_tool("tar")?;
    require_tool("rustc")?;

    verify_sha(PERSON_SHA, &person_clip)?;
    verify_sha(EMPTY_SHA, &empty_clip)?;
    verify_sha(MODEL_SHA, &model_file)?;
    ffprobe_video(&ffprobe, &person_clip)?;
    ffprobe_video(&ffprobe, &empty_clip)?;

    fs::create_dir_all(&cache_dir).map_err(|error| format!("create cache dir: {error}"))?;
    fs::create_dir_all(&tool_dir).map_err(|error| format!("create mediamtx dir: {error}"))?;
    fs::create_dir_all(&zig_tool_dir).map_err(|error| format!("create zig dir: {error}"))?;

    let mediamtx = mediamtx_asset()?;
    let mediamtx_archive = cache_dir.join(mediamtx.asset);
    download_if_missing(&mediamtx.url, &mediamtx_archive)?;
    verify_sha(mediamtx.sha, &mediamtx_archive)?;
    run_command(
        Command::new("tar")
            .arg("-xzf")
            .arg(&mediamtx_archive)
            .arg("-C")
            .arg(&tool_dir)
            .arg("mediamtx"),
        "extract mediamtx",
    )?;
    make_executable(&tool_dir.join("mediamtx"))?;

    let zig = zig_asset()?;
    let zig_archive = cache_dir.join(zig.asset);
    let zig_dir = zig_tool_dir.join(zig.asset.trim_end_matches(".tar.xz"));
    download_if_missing(&zig.url, &zig_archive)?;
    verify_sha(zig.sha, &zig_archive)?;
    if !zig_dir.join("zig").is_file() {
        run_command(
            Command::new("tar")
                .arg("-xf")
                .arg(&zig_archive)
                .arg("-C")
                .arg(&zig_tool_dir),
            "extract zig",
        )?;
    }
    let zig_bin = zig_dir.join("zig");
    write_zig_wrapper(
        &zig_tool_dir.join("zig-cc-x86_64-linux-musl"),
        &zig_bin,
        "x86_64-linux-musl",
        "cc",
    )?;
    write_zig_wrapper(
        &zig_tool_dir.join("zig-cxx-x86_64-linux-musl"),
        &zig_bin,
        "x86_64-linux-musl",
        "c++",
    )?;
    write_zig_wrapper(
        &zig_tool_dir.join("zig-cc-aarch64-linux-musl"),
        &zig_bin,
        "aarch64-linux-musl",
        "cc",
    )?;
    write_zig_wrapper(
        &zig_tool_dir.join("zig-cxx-aarch64-linux-musl"),
        &zig_bin,
        "aarch64-linux-musl",
        "c++",
    )?;

    let x86_libcxx_dir = zig_archive_dir(&zig_bin, &zig_tool_dir, "x86_64-linux-musl", "libc++.a")?;
    let x86_libcxxabi_dir =
        zig_archive_dir(&zig_bin, &zig_tool_dir, "x86_64-linux-musl", "libc++abi.a")?;
    let aarch64_libcxx_dir =
        zig_archive_dir(&zig_bin, &zig_tool_dir, "aarch64-linux-musl", "libc++.a")?;
    let aarch64_libcxxabi_dir =
        zig_archive_dir(&zig_bin, &zig_tool_dir, "aarch64-linux-musl", "libc++abi.a")?;
    write_rust_lld_wrapper(
        &zig_tool_dir.join("rust-lld-x86_64-linux-musl"),
        &x86_libcxx_dir,
        &x86_libcxxabi_dir,
    )?;
    write_rust_lld_wrapper(
        &zig_tool_dir.join("rust-lld-aarch64-linux-musl"),
        &aarch64_libcxx_dir,
        &aarch64_libcxxabi_dir,
    )?;

    println!("ffmpeg: {}", ffmpeg.display());
    println!("ffprobe: {}", ffprobe.display());
    println!("mediamtx: {}", tool_dir.join("mediamtx").display());
    println!("zig: {}", zig_bin.display());
    println!(
        "zig cc x86_64 musl: {}",
        zig_tool_dir.join("zig-cc-x86_64-linux-musl").display()
    );
    println!(
        "zig c++ x86_64 musl: {}",
        zig_tool_dir.join("zig-cxx-x86_64-linux-musl").display()
    );
    println!(
        "rust-lld x86_64 musl: {}",
        zig_tool_dir.join("rust-lld-x86_64-linux-musl").display()
    );
    println!(
        "zig cc aarch64 musl: {}",
        zig_tool_dir.join("zig-cc-aarch64-linux-musl").display()
    );
    println!(
        "zig c++ aarch64 musl: {}",
        zig_tool_dir.join("zig-cxx-aarch64-linux-musl").display()
    );
    println!(
        "rust-lld aarch64 musl: {}",
        zig_tool_dir.join("rust-lld-aarch64-linux-musl").display()
    );
    println!("person fixture: {}", person_clip.display());
    println!("empty fixture: {}", empty_clip.display());
    println!("model fixture: {}", model_file.display());
    Ok(())
}

fn repo_root() -> Result<PathBuf, String> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "xtask has no workspace parent".to_string())
}

struct Asset {
    asset: &'static str,
    sha: &'static str,
    url: String,
}

fn mediamtx_asset() -> Result<Asset, String> {
    let (asset, sha) = match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => (
            "mediamtx_v1.19.1_linux_amd64.tar.gz",
            "035ee04f91b1c7a0c02e13b2139ca2456e43b6bd6a80e3100e8c228556e07807",
        ),
        ("linux", "aarch64") => (
            "mediamtx_v1.19.1_linux_arm64.tar.gz",
            "97a277cf24153e168008c18da53fe84e8d364456e2d7b457dc0457666c32867b",
        ),
        ("linux", "arm") | ("linux", "armv7") => (
            "mediamtx_v1.19.1_linux_armv7.tar.gz",
            "052654f2268ad0604f2bb277e417cf3c122d7399f814e6d1ca2dbcf180ed7fe9",
        ),
        _ => {
            return Err(format!(
                "Unsupported platform for pinned mediamtx download: {}-{}",
                env::consts::OS,
                env::consts::ARCH
            ));
        }
    };
    Ok(Asset {
        asset,
        sha,
        url: format!(
            "https://github.com/bluenviron/mediamtx/releases/download/{MEDIAMTX_VERSION}/{asset}"
        ),
    })
}

fn zig_asset() -> Result<Asset, String> {
    let (asset, sha) = match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => (
            "zig-linux-x86_64-0.13.0.tar.xz",
            "d45312e61ebcc48032b77bc4cf7fd6915c11fa16e4aad116b66c9468211230ea",
        ),
        ("linux", "aarch64") => (
            "zig-linux-aarch64-0.13.0.tar.xz",
            "041ac42323837eb5624068acd8b00cd5777dac4cf91179e8dad7a7e90dd0c556",
        ),
        _ => {
            return Err(format!(
                "Unsupported platform for pinned Zig download: {}-{}",
                env::consts::OS,
                env::consts::ARCH
            ));
        }
    };
    Ok(Asset {
        asset,
        sha,
        url: format!("https://ziglang.org/download/{ZIG_VERSION}/{asset}"),
    })
}

fn verify_sha(expected: &str, path: &Path) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let digest = format!("{:x}", Sha256::digest(&bytes));
    if digest == expected {
        println!("{}: OK", path.display());
        Ok(())
    } else {
        Err(format!(
            "sha256 mismatch for {}: expected {expected}, got {digest}",
            path.display()
        ))
    }
}

fn ffprobe_video(ffprobe: &Path, path: &Path) -> Result<(), String> {
    run_command(
        Command::new(ffprobe)
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-select_streams")
            .arg("v:0")
            .arg("-count_frames")
            .arg("-show_entries")
            .arg("stream=nb_read_frames,duration")
            .arg("-of")
            .arg("default=noprint_wrappers=1")
            .arg(path),
        "probe video fixture",
    )
}

fn download_if_missing(url: &str, path: &Path) -> Result<(), String> {
    if path.is_file() {
        return Ok(());
    }
    run_command(
        Command::new("curl")
            .arg("-L")
            .arg("--fail")
            .arg("--output")
            .arg(path)
            .arg(url),
        "download pinned harness asset",
    )
}

fn write_zig_wrapper(
    wrapper: &Path,
    zig_bin: &Path,
    target_triple: &str,
    compiler: &str,
) -> Result<(), String> {
    let content = format!(
        "#!/usr/bin/env bash\nset -euo pipefail\nzig_bin={}\ntarget_triple={}\ncompiler={}\nargs=()\nfor arg in \"$@\"; do\n  case \"${{arg}}\" in\n    --target=*) ;;\n    *) args+=(\"${{arg}}\") ;;\n  esac\ndone\nexec \"${{zig_bin}}\" \"${{compiler}}\" -target \"${{target_triple}}\" \"${{args[@]}}\"\n",
        shell_quote(zig_bin.display()),
        shell_quote(target_triple),
        shell_quote(compiler)
    );
    fs::write(wrapper, content).map_err(|error| format!("write {}: {error}", wrapper.display()))?;
    make_executable(wrapper)
}

fn zig_archive_dir(
    zig_bin: &Path,
    zig_tool_dir: &Path,
    target_triple: &str,
    archive_name: &str,
) -> Result<String, String> {
    let probe_cpp = zig_tool_dir.join(format!("probe-{target_triple}.cpp"));
    let probe_obj = zig_tool_dir.join(format!("probe-{target_triple}.o"));
    fs::write(
        &probe_cpp,
        "extern \"C\" int zig_probe(void) { return 0; }\n",
    )
    .map_err(|error| format!("write {}: {error}", probe_cpp.display()))?;
    run_command(
        Command::new(zig_bin)
            .arg("c++")
            .arg("-target")
            .arg(target_triple)
            .arg("-c")
            .arg(&probe_cpp)
            .arg("-o")
            .arg(&probe_obj),
        "compile zig archive probe",
    )?;
    let output = Command::new(zig_bin)
        .arg("cc")
        .arg("-target")
        .arg(target_triple)
        .arg("-###")
        .arg(&probe_obj)
        .arg("-lc++")
        .arg("-o")
        .arg(probe_obj.with_extension("out"))
        .output()
        .map_err(|error| format!("run zig archive probe: {error}"))?;
    let trace = String::from_utf8_lossy(&output.stderr);
    for token in trace.split(|character: char| character == '"' || character.is_whitespace()) {
        if token.ends_with(archive_name) {
            let path = Path::new(token);
            if let Some(parent) = path.parent() {
                return Ok(parent.display().to_string());
            }
        }
    }
    Err(format!(
        "Unable to locate Zig {archive_name} for {target_triple}"
    ))
}

fn write_rust_lld_wrapper(
    wrapper: &Path,
    libcxx_dir: &str,
    libcxxabi_dir: &str,
) -> Result<(), String> {
    let rust_lld = rust_lld_path()?;
    let content = format!(
        "#!/usr/bin/env bash\nset -euo pipefail\nrust_lld={}\nlibcxx_dir={}\nlibcxxabi_dir={}\nargs=(\"-L\" \"${{libcxx_dir}}\" \"-L\" \"${{libcxxabi_dir}}\")\nskip_next=false\nfor arg in \"$@\"; do\n  if [[ \"${{skip_next}}\" == true ]]; then\n    skip_next=false\n    continue\n  fi\n  if [[ \"${{arg}}\" == \"-flavor\" ]]; then\n    skip_next=true\n    continue\n  fi\n  args+=(\"${{arg}}\")\n  if [[ \"${{arg}}\" == \"-lc++\" ]]; then\n    args+=(\"-lc++abi\")\n  fi\ndone\nexec \"${{rust_lld}}\" -flavor gnu \"${{args[@]}}\"\n",
        shell_quote(rust_lld.display()),
        shell_quote(libcxx_dir),
        shell_quote(libcxxabi_dir)
    );
    fs::write(wrapper, content).map_err(|error| format!("write {}: {error}", wrapper.display()))?;
    make_executable(wrapper)
}

fn rust_lld_path() -> Result<PathBuf, String> {
    let sysroot = command_stdout(Command::new("rustc").arg("--print").arg("sysroot"))?;
    let verbose = command_stdout(Command::new("rustc").arg("-vV"))?;
    let host = verbose
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .ok_or_else(|| "rustc -vV did not report a host triple".to_string())?;
    let rust_lld = PathBuf::from(sysroot.trim())
        .join("lib/rustlib")
        .join(host)
        .join("bin/rust-lld");
    if rust_lld.is_file() {
        Ok(rust_lld)
    } else {
        Err(format!(
            "Unable to locate rust-lld at {}",
            rust_lld.display()
        ))
    }
}

fn require_tool(name: &str) -> Result<PathBuf, String> {
    find_on_path(name).ok_or_else(|| {
        format!("{name} is required. Install {name} or put it on PATH before running setup.")
    })
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    env::split_paths(&paths)
        .map(|path| path.join(name))
        .find(|path| path.is_file())
}

fn command_stdout(command: &mut Command) -> Result<String, String> {
    let output = command
        .output()
        .map_err(|error| format!("run command: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}

fn run_command(command: &mut Command, label: &str) -> Result<(), String> {
    let output = command
        .output()
        .map_err(|error| format!("{label}: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)
        .map_err(|error| format!("stat {}: {error}", path.display()))?
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
        .map_err(|error| format!("chmod {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn shell_quote(value: impl std::fmt::Display) -> String {
    let value = value.to_string();
    format!("'{}'", value.replace('\'', "'\\''"))
}
