use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_DEV");
    if env::var_os("CARGO_FEATURE_DEV").is_some() {
        // Development serves the frontend through Vite. Do not watch frontend
        // sources here: changing them must not invalidate the Rust build.
        return;
    }

    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"));
    let client_index = manifest.join("dist/client/index.html");

    println!("cargo:rerun-if-changed=web");
    println!("cargo:rerun-if-changed=shared");
    println!("cargo:rerun-if-changed=package.json");
    println!("cargo:rerun-if-changed=package-lock.json");
    println!("cargo:rerun-if-changed=vite.config.ts");
    println!("cargo:rerun-if-changed=scripts/dev-options.ts");
    println!("cargo:rerun-if-env-changed=ATTYD_SKIP_WEB_BUILD");

    if env::var_os("ATTYD_SKIP_WEB_BUILD").is_some() && client_index.is_file() {
        return;
    }

    let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };
    let status = Command::new(npm)
        .args(["run", "build:client"])
        .current_dir(&manifest)
        .status()
        .expect("failed to start `npm run build:client`");
    if !status.success() {
        panic!("`npm run build:client` failed with {status}");
    }
    if !client_index.is_file() {
        panic!("frontend build did not create {}", client_index.display());
    }
}
