use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=./runtime/src");

    // Get the target directory
    let target_dir = env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".to_string());
    let release_dir = PathBuf::from(&target_dir).join("release");

    // Create release directory if it doesn't exist
    fs::create_dir_all(&release_dir).expect("Failed to create release directory");

    // Compile the runtime library using rustc
    let runtime_src = PathBuf::from("./runtime/src/lib.rs");

    let lib_name = if cfg!(target_os = "windows") {
        "luga_runtime.lib"
    } else {
        "libluga_runtime.a"
    };

    let lib_path = release_dir.join(lib_name);

    let mut rustc_cmd = Command::new("rustc");
    rustc_cmd
        .arg("--edition=2024")
        .arg("--crate-type=staticlib")
        .arg("--crate-name=luga_runtime")
        .arg("-O") // Optimization flag (equivalent to --release)
        .arg("-C")
        .arg("panic=abort") // Use panic=abort for no_std compatibility
        .arg("-o")
        .arg(&lib_path)
        .arg(&runtime_src);

    // Add target-specific flags if needed
    if let Ok(target) = env::var("TARGET") {
        rustc_cmd.arg("--target").arg(target);
    }

    // Add library search paths if needed
    if let Ok(deps_dir) = env::var("OUT_DIR") {
        let deps_path = PathBuf::from(deps_dir)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("deps");
        rustc_cmd
            .arg("-L")
            .arg(format!("dependency={}", deps_path.display()));
    }

    let output = rustc_cmd.output().expect("Failed to execute rustc");

    if !output.status.success() {
        panic!(
            "Runtime build failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Verify the library was created
    if !lib_path.exists() {
        panic!("Runtime library not found at: {}", lib_path.display());
    }

    // Copy to OUT_DIR
    let out_dir = env::var("OUT_DIR").unwrap();
    let dest_path = PathBuf::from(&out_dir).join(lib_name);
    fs::copy(&lib_path, &dest_path).expect("Failed to copy runtime library");

    println!("cargo:rustc-env=RUNTIME_LIB_PATH={}", dest_path.display());
    println!("cargo:rustc-env=RUNTIME_LIB_NAME={}", lib_name);
}
