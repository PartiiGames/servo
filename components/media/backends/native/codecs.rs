/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The decoders: symphonia's codecs as built here, plus Opus through libopus (BSD-3-Clause,
//! via symphonia-adapter-libopus and opusic-sys) with the `opus` feature. symphonia 0.5 demuxes
//! Opus in Ogg and WebM but has no Opus decoder.

use std::sync::LazyLock;

use symphonia::core::codecs::CodecRegistry;

pub fn codecs() -> &'static CodecRegistry {
    static CODECS: LazyLock<CodecRegistry> = LazyLock::new(|| {
        let mut registry = CodecRegistry::new();
        symphonia::default::register_enabled_codecs(&mut registry);
        #[cfg(feature = "opus")]
        registry.register_all::<symphonia_adapter_libopus::OpusDecoder>();
        registry
    });
    &CODECS
}
