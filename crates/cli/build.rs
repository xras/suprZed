#![allow(clippy::disallowed_methods, reason = "build scripts are exempt")]
use std::process::Command;

#[path = "../zed/build_version.rs"]
mod build_version;

fn main() {
    let source_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    println!("cargo:rerun-if-changed=../../SUPRZED_VERSION");
    println!("cargo:rerun-if-changed=../zed/build_version.rs");
    for variable in [
        "SUPRZED_BUILD_VERSION",
        "SUPRZED_BUILD_ORIGIN",
        "SUPRZED_BUILD_ATTEMPT",
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

    if std::env::var("ZED_UPDATE_EXPLANATION").is_ok() {
        println!(r#"cargo:rustc-cfg=feature="no-bundled-uninstall""#);
    }

    if cfg!(target_os = "macos") {
        println!("cargo:rustc-env=MACOSX_DEPLOYMENT_TARGET=10.15.7");
    }

    // Populate git sha environment variable if git is available
    if let Some(output) = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
    {
        let git_sha = String::from_utf8_lossy(&output.stdout);
        let git_sha = git_sha.trim();

        println!("cargo:rustc-env=ZED_COMMIT_SHA={git_sha}");
    }
    if let Some(build_identifier) = option_env!("GITHUB_RUN_NUMBER") {
        println!("cargo:rustc-env=ZED_BUILD_ID={build_identifier}");
    }

    #[cfg(all(windows, feature = "cli"))]
    {
        println!("cargo:rerun-if-env-changed=RELEASE_CHANNEL");
        println!("cargo:rerun-if-env-changed=GITHUB_RUN_NUMBER");

        windows_resources::compile(false).expect("failed to compile Windows resources");
    }
}
