//! Embeds the application icon into the executable.
//!
//! Windows has no linker flag for icons: they have to arrive as a compiled
//! resource. This runs the Windows SDK's `rc.exe` directly rather than pulling
//! in a wrapper crate, and degrades to a warning when no resource compiler is
//! installed so the build never breaks just because of an icon.
//!
//! Regenerate the artwork with `cargo run --example make_icon`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // The icon only exists to decorate the Windows binary.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    println!("cargo:rerun-if-changed=assets/icon.rc");
    println!("cargo:rerun-if-changed=assets/icon.ico");

    if let Err(message) = compile_icon() {
        // Not an error: a build without the SDK still produces a working binary.
        println!("cargo:warning=未嵌入应用图标：{message}");
    }
}

fn compile_icon() -> Result<(), String> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").map_err(|e| e.to_string())?);
    let rc_file = manifest.join("assets").join("icon.rc");
    if !rc_file.exists() {
        return Err(format!("{} 不存在", rc_file.display()));
    }

    let rc = find_resource_compiler().ok_or_else(|| {
        "找不到 rc.exe（需要 Windows SDK，或把它的目录加入 PATH）".to_string()
    })?;

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").map_err(|e| e.to_string())?);
    let res_file = out_dir.join("screenrec_icon.res");

    let output = Command::new(&rc)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_file)
        .arg(&rc_file)
        .output()
        .map_err(|err| format!("无法运行 {}: {err}", rc.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!("rc.exe 失败：{stdout}{stderr}"));
    }
    if !res_file.exists() {
        return Err("rc.exe 没有生成 .res 文件".to_string());
    }

    // MSVC's link.exe accepts a .res file as an input.
    println!("cargo:rustc-link-arg={}", res_file.display());
    Ok(())
}

/// Locates `rc.exe`: PATH first, then the highest Windows SDK version.
fn find_resource_compiler() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PATH") {
        for directory in std::env::split_paths(&path) {
            let candidate = directory.join("rc.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    // Prefer the newest SDK, and the 64-bit host tools within it.
    for kits in [
        r"C:\Program Files (x86)\Windows Kits\10\bin",
        r"C:\Program Files\Windows Kits\10\bin",
    ] {
        let kits = Path::new(kits);
        let mut versions: Vec<PathBuf> = match std::fs::read_dir(kits) {
            Ok(entries) => entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.is_dir()
                        && path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.starts_with("10."))
                })
                .collect(),
            Err(_) => continue,
        };
        // Version directories sort correctly as strings ("10.0.22621.0").
        versions.sort();
        for version in versions.into_iter().rev() {
            for host in ["x64", "x86", "arm64"] {
                let candidate = version.join(host).join("rc.exe");
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}
