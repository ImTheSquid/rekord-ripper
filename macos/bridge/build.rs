//! Generates the Swift side of the bridge straight into the SwiftPM package:
//! the C headers into the `RekordRipperFFI` module, the Swift glue into
//! `RekordRipperBridge`, which has to import that module explicitly.

use std::fs;
use std::path::{Path, PathBuf};

const BRIDGES: &[&str] = &["src/lib.rs"];

fn main() {
    for path in BRIDGES {
        println!("cargo:rerun-if-changed={path}");
    }
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("swift-bridge");
    let crate_name = env!("CARGO_PKG_NAME");
    swift_bridge_build::parse_bridges(BRIDGES.iter().map(|p| manifest.join(p)))
        .write_all_concatenated(&out, crate_name);

    let sources = manifest.join("../app/Sources");
    let ffi = sources.join("RekordRipperFFI");
    let glue = sources.join("RekordRipperBridge");
    fs::create_dir_all(&glue).unwrap();
    copy(&out.join("SwiftBridgeCore.h"), &ffi.join("SwiftBridgeCore.h"), "");
    copy(&out.join(crate_name).join(format!("{crate_name}.h")), &ffi.join("Bridge.h"), "");
    let import = "import RekordRipperFFI\n\n";
    copy(&out.join("SwiftBridgeCore.swift"), &glue.join("SwiftBridgeCore.swift"), import);
    copy(&out.join(crate_name).join(format!("{crate_name}.swift")), &glue.join("Bridge.swift"), import);
}

/// Writes only on change, so SwiftPM does not rebuild after every cargo build.
fn copy(from: &Path, to: &Path, prefix: &str) {
    let body = format!("{prefix}{}", fs::read_to_string(from).unwrap());
    if fs::read_to_string(to).ok().as_deref() != Some(body.as_str()) {
        fs::write(to, body).unwrap();
    }
}
