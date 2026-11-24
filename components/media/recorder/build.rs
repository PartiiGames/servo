/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Compiles shim.c against the vendored libvpx headers (partii-libvpx-sys exports them as
//! DEP_VPX_INCLUDE and links libvpx.a).

fn main() {
    println!("cargo:rerun-if-changed=shim.c");
    let include = std::env::var("DEP_VPX_INCLUDE").expect("partii-libvpx-sys sets DEP_VPX_INCLUDE");
    cc::Build::new()
        .file("shim.c")
        .include(include)
        .opt_level(2)
        .warnings(false)
        .compile("partii_vp8_shim");
}
