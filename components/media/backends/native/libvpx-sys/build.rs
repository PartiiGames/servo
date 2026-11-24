/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Builds libvpx.a from libvpx/ with its own configure and make (needs sh, perl and make, as every
//! Linux and macOS build host has): the VP8/VP9 decoders and the VP8 encoder (realtime only, for
//! MediaRecorder: servo-media-recorder), static, PIC. The x86 assembly needs nasm or yasm;
//! when neither is on PATH (most farm nodes) libvpx is built for `generic-gnu`, its plain C code.
//! PARTII_VPX_TARGET overrides the configure target.

use std::env;
use std::path::PathBuf;
use std::process::Command;

/// libvpx configure flags. Part of the configure stamp: changing them reconfigures.
const CONFIGURE: &[&str] = &[
    "--enable-static",
    "--disable-shared",
    "--enable-pic",
    "--disable-examples",
    "--disable-tools",
    "--disable-docs",
    "--disable-unit-tests",
    "--disable-install-docs",
    "--disable-install-bins",
    "--disable-install-srcs",
    "--enable-realtime-only",
    "--disable-vp9-encoder",
    "--disable-webm-io",
    "--disable-libyuv",
    "--enable-vp9-highbitdepth",
    "--enable-multithread",
    "--disable-debug",
    "--size-limit=16384x16384",
];

fn has(tool: &str) -> bool {
    Command::new(tool)
        .arg("-v")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn main() {
    let src = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("libvpx");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let build = out.join("build");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=libvpx/vpx/vpx_decoder.h");
    println!("cargo:rerun-if-changed=libvpx/vpx/vpx_encoder.h");
    println!("cargo:rerun-if-env-changed=PARTII_VPX_TARGET");
    println!("cargo:rerun-if-env-changed=PARTII_REQUIRE_NASM");

    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    let host = env::var("HOST").unwrap();
    let target_triple = env::var("TARGET").unwrap();
    let native = host == target_triple;
    let target = env::var("PARTII_VPX_TARGET").unwrap_or_else(|_| {
        match (arch.as_str(), os.as_str()) {
            ("x86_64", "linux") if native && (has("nasm") || has("yasm")) => "x86_64-linux-gcc".into(),
            ("aarch64", "linux") if native => "arm64-linux-gcc".into(),
            // arm64-darwin-gcc is libvpx's iOS target (iPhoneOS SDK); macOS 11+ is arm64-darwin20-gcc.
            ("aarch64", "macos") if native => "arm64-darwin20-gcc".into(),
            _ => "generic-gnu".into(),
        }
    });

    // Partii (clips): generic-gnu on x86_64 is plain C, no SSE2/SSSE3/AVX2: the VP8 encoder runs ~4x
    // slower (18-21 ms a 640x360 frame on the farm, which has no nasm). Say so loudly: a build that
    // ships needs nasm (or yasm) on the builder.
    // PARTII_REQUIRE_NASM=1 (release and export jobs set it, with --needs sys:nasm): that fallback fails
    // the build instead, so a release can't ship the slow encoder silently.
    if target == "generic-gnu" && arch == "x86_64" {
        let msg = "libvpx: no nasm/yasm, building generic C (no SIMD): VP8 encode ~4x slower. Install nasm.";
        if env::var("PARTII_REQUIRE_NASM").is_ok_and(|v| v == "1") {
            panic!("{msg} (PARTII_REQUIRE_NASM=1)");
        }
        println!("cargo:warning={msg}");
    }
    let compiler = cc::Build::new().opt_level(2).pic(true).get_compiler();
    let ar = cc::Build::new().get_archiver();
    std::fs::create_dir_all(&build).unwrap();
    let stamp = build.join(".configured");
    let wanted = format!("{target} {:?} {}", compiler.path(), CONFIGURE.join(" "));
    if std::fs::read_to_string(&stamp).ok().as_deref() != Some(wanted.as_str()) {
        // A reconfigure over an old tree keeps its old object list (the VP8 encoder never got built when
        // it was turned on): start from an empty build directory.
        let _ = std::fs::remove_dir_all(&build);
        std::fs::create_dir_all(&build).unwrap();
        let status = Command::new("sh")
            .arg(src.join("configure"))
            .arg(format!("--target={target}"))
            .args(CONFIGURE)
            .env("CC", compiler.path())
            .env("CXX", compiler.path())
            .env("AR", ar.get_program())
            .env("CFLAGS", compiler.cflags_env())
            .current_dir(&build)
            .status()
            .expect("libvpx: can't run configure (needs sh)");
        assert!(status.success(), "libvpx: configure failed (target {target})");
        std::fs::write(&stamp, &wanted).unwrap();
    }
    let jobs = env::var("NUM_JOBS").unwrap_or_else(|_| "4".into());
    let status = Command::new("make")
        .args(["-j", &jobs, "libvpx.a"])
        .current_dir(&build)
        .status()
        .expect("libvpx: can't run make");
    assert!(status.success(), "libvpx: make failed");
    assert!(
        build.join("vp8/vp8_cx_iface.c.o").exists(),
        "libvpx: the VP8 encoder wasn't built (vp8/vp8_cx_iface.c.o missing in {})",
        build.display()
    );
    println!("cargo:rustc-link-search=native={}", build.display());
    println!("cargo:rustc-link-lib=static=vpx");
    println!("cargo:include={}", src.display());
}
