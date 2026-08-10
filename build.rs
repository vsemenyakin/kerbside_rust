//! Put the model next to the binary.
//!
//! The Python finds `vehicle.onnx` beside its own source file. A compiled
//! binary has no such thing, so the model has to be *deployed* -- and the two
//! places it can sensibly live are next to the executable or at a path given by
//! `KERBSIDE_MODEL_DIR`. This copies it to the first of those at build time, so
//! `cargo run` and a copied-out `target/release` directory both work without
//! anyone having to remember a step.
//!
//! Note what this does *not* do: it does not embed the weights in the binary.
//! Embedding is a real option and is on the list of things a port evaluation
//! should cost separately -- but it is a change to the threat model, not a
//! build detail, and doing it silently here would make the reverse-engineering
//! assessment read better than the shipped artefact deserves. The `.onnx` file
//! is as readable next to this binary as it is next to the Python. See the note
//! in PORTING.md: no language port protects the model weights.

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=model/vehicle.onnx");
    println!("cargo:rerun-if-env-changed=ORT_DYLIB_PATH");

    let out_dir = match std::env::var_os("OUT_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => return,
    };
    // OUT_DIR is target/<profile>/build/<pkg>-<hash>/out; the binary lands
    // three levels up. This is the same walk the `ort` crate uses to place its
    // dylibs, and it is stable across cargo versions in practice.
    let target_dir = match out_dir.ancestors().nth(3) {
        Some(dir) => dir.to_path_buf(),
        None => return,
    };

    copy_model(&target_dir);
    copy_onnx_runtime(&target_dir);
    copy_opencv_runtime(&target_dir);
}

/// Copy a file only when the destination is missing or a different size.
///
/// `opencv_world` is around sixty megabytes; copying it on every incremental
/// build would make `cargo build` feel broken.
fn copy_if_stale(source: &Path, destination: &Path) {
    let up_to_date = match (source.metadata(), destination.metadata()) {
        (Ok(from), Ok(to)) => from.len() == to.len(),
        _ => false,
    };
    if up_to_date {
        return;
    }
    if let Err(e) = std::fs::copy(source, destination) {
        println!(
            "cargo:warning=could not copy {} to {}: {e}",
            source.display(),
            destination.display()
        );
    }
}

/// Put OpenCV's runtime DLL next to the binary, on Windows only.
///
/// On Raspberry Pi OS OpenCV comes from `libopencv-dev` and lives on the
/// standard library search path, so there is nothing to copy and copying would
/// be actively wrong -- it would pin a second copy of the library that apt
/// could no longer update.
///
/// Windows has no such path. Without this the linker succeeds, the binary is
/// produced, and running it fails with exit code 53 and *no message at all*,
/// because the loader gives up before `main` is reached. That is the least
/// debuggable failure in the whole build, so it is worth sixty megabytes.
fn copy_opencv_runtime(target_dir: &Path) {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rerun-if-env-changed=OPENCV_LINK_PATHS");
    let Ok(link_paths) = std::env::var("OPENCV_LINK_PATHS") else {
        return;
    };

    for lib_dir in link_paths.split(',').map(|p| PathBuf::from(p.trim())) {
        // The prebuilt release puts the import library in x64/vc16/lib and the
        // DLL beside it in x64/vc16/bin.
        let Some(bin_dir) = lib_dir.parent().map(|p| p.join("bin")) else {
            continue;
        };
        let Ok(entries) = std::fs::read_dir(&bin_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            // The `d` suffix marks the debug build, which the release import
            // library does not match.
            if name.starts_with("opencv_world") && name.ends_with(".dll") && !name.ends_with("d.dll")
            {
                copy_if_stale(&entry.path(), &target_dir.join(&name));
                return;
            }
        }
    }
    println!(
        "cargo:warning=opencv_world*.dll was not found next to OPENCV_LINK_PATHS; \
         the binary will need it on PATH at run time"
    );
}

/// Put the ONNX Runtime shared library next to the binary too.
///
/// The runtime is loaded rather than linked (see BUILD.md), which means the
/// executable needs to find it at startup. Requiring an environment variable
/// for that is a trap: the build succeeds, the binary is produced, and it dies
/// at startup on anyone who runs it without having sourced a script first.
///
/// Copying it here makes `target/<profile>/` self-contained -- binary, model,
/// runtime -- which is also what gets copied onto a device.
fn copy_onnx_runtime(target_dir: &Path) {
    let names: &[&str] = match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("windows") => &["onnxruntime.dll", "onnxruntime_providers_shared.dll"],
        Ok("macos") => &["libonnxruntime.dylib"],
        _ => &["libonnxruntime.so"],
    };
    let primary = names[0];

    // Where the library might be, at build time. An explicit ORT_DYLIB_PATH
    // wins; otherwise look inside the Python project's virtualenv next door,
    // because pointing both arms at the same runtime is what makes a CSV
    // comparison attributable to the code rather than to the library.
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(path) = std::env::var("ORT_DYLIB_PATH") {
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent() {
            roots.push(parent.to_path_buf());
        }
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if let Some(workspace) = manifest.parent() {
        roots.push(
            workspace
                .join("kerbside/venv/Lib/site-packages/onnxruntime/capi"),
        );
        for python in ["python3.11", "python3.12", "python3.13"] {
            roots.push(
                workspace
                    .join("kerbside/venv/lib")
                    .join(python)
                    .join("site-packages/onnxruntime/capi"),
            );
        }
    }

    let Some(root) = roots.iter().find(|root| root.join(primary).is_file()) else {
        // Not fatal. A system-installed runtime on the library search path is a
        // perfectly good deployment, and so is setting ORT_DYLIB_PATH by hand.
        println!(
            "cargo:warning={primary} was not found at build time; set \
             ORT_DYLIB_PATH before running, or see BUILD.md"
        );
        return;
    };

    for name in names {
        let source = root.join(name);
        if !source.is_file() {
            continue;
        }
        copy_if_stale(&source, &target_dir.join(name));
    }
}

fn copy_model(target_dir: &Path) {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("model/vehicle.onnx");
    if !source.is_file() {
        // Not fatal: the model is only needed when `model.ENABLED` is true, and
        // a clear runtime error naming the search path is more useful than a
        // build failure for someone who only wants the scene generator.
        println!(
            "cargo:warning=model/vehicle.onnx is missing; build it with \
             `tools/export_model.py` in the Python repository and copy it here"
        );
        return;
    }

    copy_if_stale(&source, &target_dir.join("vehicle.onnx"));
}
