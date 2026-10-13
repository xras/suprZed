#![allow(clippy::disallowed_methods, reason = "build scripts are exempt")]
use std::process::Command;

#[cfg(windows)]
mod build_conpty;
mod build_version;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    println!("cargo:rerun-if-changed=../../SUPRZED_VERSION");
    println!("cargo:rerun-if-changed=build_version.rs");
    for variable in [
        "SUPRZED_BUILD_VERSION",
        "SUPRZED_BUILD_ORIGIN",
        "SUPRZED_BUILD_ATTEMPT",
        "ZED_COMMIT_SHA",
    ] {
        println!("cargo:rerun-if-env-changed={variable}");
    }
    build_version::watch_git(&source_root);
    let fork = build_version::load(&source_root).expect("invalid suprZed build metadata");
    println!("cargo:rustc-env=SUPRZED_VERSION={}", fork.version);
    println!("cargo:rustc-env=SUPRZED_ORIGIN={}", fork.origin);
    println!("cargo:rustc-env=SUPRZED_COMMIT={}", fork.commit);
    println!(
        "cargo:rustc-env=SUPRZED_SOURCE_STATUS={}",
        fork.source_status
    );
    #[cfg(target_os = "linux")]
    {
        // Add rpaths for libraries that webrtc-sys dlopens at runtime.
        // This is mostly required for hosts with non-standard SO installation
        // locations such as NixOS.
        let dlopened_libs = ["libva", "libva-drm", "egl"];

        let mut rpath_dirs = std::collections::BTreeSet::new();
        for lib in &dlopened_libs {
            if let Some(libdir) = pkg_config::get_variable(lib, "libdir").ok() {
                rpath_dirs.insert(libdir);
            } else {
                eprintln!("zed build.rs: {lib} not found in pkg-config's path");
            }
        }

        for dir in &rpath_dirs {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
        }
    }

    if cfg!(target_os = "macos") {
        println!("cargo:rustc-env=MACOSX_DEPLOYMENT_TARGET=10.15.7");

        // Weakly link ReplayKit to ensure Zed can be used on macOS 10.15+.
        println!("cargo:rustc-link-arg=-Wl,-weak_framework,ReplayKit");

        // Seems to be required to enable Swift concurrency
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");

        // Register exported Objective-C selectors, protocols, etc
        println!("cargo:rustc-link-arg=-Wl,-ObjC");

        // weak link to support Catalina
        println!("cargo:rustc-link-arg=-Wl,-weak_framework,ScreenCaptureKit");
    }

    // Populate git sha environment variable if git is available
    println!(
        "cargo:rustc-env=TARGET={}",
        std::env::var("TARGET").unwrap()
    );

    let git_sha = match std::env::var("ZED_COMMIT_SHA").ok() {
        Some(git_sha) => {
            // In deterministic build environments such as Nix, we inject the commit sha into the build script.
            Some(git_sha)
        }
        None => {
            if let Some(output) = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .output()
                .ok()
                && output.status.success()
            {
                let git_sha = String::from_utf8_lossy(&output.stdout);
                Some(git_sha.trim().to_string())
            } else {
                None
            }
        }
    };

    if let Some(git_sha) = git_sha {
        println!("cargo:rustc-env=ZED_COMMIT_SHA={git_sha}");

        if let Some(build_identifier) = option_env!("GITHUB_RUN_NUMBER") {
            println!("cargo:rustc-env=ZED_BUILD_ID={build_identifier}");
        }

        if let Ok(build_profile) = std::env::var("PROFILE")
            && build_profile == "release"
        {
            // This is currently the best way to make `cargo build ...`'s build script
            // to print something to stdout without extra verbosity.
            println!("cargo::warning=Info: using '{git_sha}' hash for ZED_COMMIT_SHA env var");
        }
    }

    if cfg!(windows) {
        if cfg!(target_env = "msvc") {
            // todo(windows): This is to avoid stack overflow. Remove it when solved.
            println!("cargo:rustc-link-arg=/stack:{}", 8 * 1024 * 1024);
            println!("cargo:rustc-link-arg=/DELAYLOAD:windowsperformancerecordercontrol");
            println!("cargo:rustc-link-lib=delayimp");
        }

        #[cfg(windows)]
        build_conpty::prepare(
            std::path::Path::new(&std::env::var("OUT_DIR")?),
            &std::env::var("CARGO_CFG_TARGET_ARCH")?,
        )?;

        println!("cargo:rerun-if-env-changed=RELEASE_CHANNEL");
        println!("cargo:rerun-if-env-changed=GITHUB_RUN_NUMBER");

        #[cfg(windows)]
        {
            windows_resources::compile(false).expect("failed to compile Windows resources");
        }
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    prepare_app_icon_x11();

    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn icon_path() -> std::path::PathBuf {
    use std::str::FromStr;

    let release_channel = option_env!("RELEASE_CHANNEL").unwrap_or("dev");
    let channel = match release_channel {
        "stable" => "",
        "preview" => "-preview",
        "nightly" => "-nightly",
        "dev" => "-dev",
        _ => "-dev",
    };

    #[cfg(windows)]
    let icon = format!("resources/windows/app-icon{}.ico", channel);
    #[cfg(not(windows))]
    let icon = format!("resources/app-icon{}.png", channel);

    std::path::PathBuf::from_str(&icon).unwrap()
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn prepare_app_icon_x11() {
    use image::{ImageReader, imageops};
    use std::env;
    use std::path::Path;

    let out_dir = env::var("OUT_DIR").unwrap();

    let resized_image = ImageReader::open(icon_path())
        .unwrap()
        .decode()
        .unwrap()
        .resize(256, 256, imageops::FilterType::Lanczos3);

    // name should match include_bytes! call in src/zed.rs
    let icon_out_path = Path::new(&out_dir).join("app_icon.png");
    resized_image.save(&icon_out_path).expect("saving app icon");

    println!("cargo:rerun-if-env-changed=RELEASE_CHANNEL");
    println!("cargo:rerun-if-changed={}", icon_path().to_string_lossy());
}
