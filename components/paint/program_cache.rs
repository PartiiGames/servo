/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii (app stream, 2026-11-24): WebRender's linked programs kept on disk.
//!
//! WebRender asks for a program binary before it compiles and links a program
//! (`ProgramCacheObserver::try_load_shader_from_disk`) and hands over the ones it linked at the
//! end of a frame (`save_shaders_to_disk`). Servo passed no cache, so every launch compiled and
//! linked every program it drew again. Here each binary is a file named by WebRender's source
//! digest, in a folder per GL driver (vendor, renderer and version: a binary only loads on the
//! driver that made it), under the app's cache folder:
//!
//! - `PARTII_WR_PROGRAM_CACHE_DIR`, when the embedder sets it;
//! - else `$XDG_CACHE_HOME/partii/wr-programs` (Linux; `~/.cache` without it), or
//!   `~/Library/Caches/partii/wr-programs` (macOS), or `%LOCALAPPDATA%\partii\wr-programs`.
//!
//! `PARTII_WR_PROGRAM_CACHE=0` turns it off. Files are written on a thread of their own, so the
//! frame that linked them doesn't wait for the disk; a binary the driver refuses is deleted.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use log::warn;
use webrender::{ProgramBinary, ProgramCache, ProgramCacheObserver, ProgramSourceDigest};

/// Bigger files are not WebRender programs: never read.
const MAX_BINARY: u64 = 32 << 20;

/// The cache for this GL driver, or None when it is off or there's no folder for it.
pub(crate) fn new(vendor: &str, renderer: &str, version: &str) -> Option<Rc<ProgramCache>> {
    if std::env::var("PARTII_WR_PROGRAM_CACHE").as_deref() == Ok("0") {
        return None;
    }
    let base = base_dir()?;
    let mut h = DefaultHasher::new();
    (vendor, renderer, version).hash(&mut h);
    let dir = base.join(format!("{:016x}", h.finish()));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!("WebRender program cache: no folder at {}: {e}", dir.display());
        return None;
    }
    // What this folder is for, for whoever looks at it.
    let _ = std::fs::write(dir.join("driver.txt"), format!("{vendor}\n{renderer}\n{version}\n"));
    Some(ProgramCache::new(Some(Box::new(DiskCache { dir }))))
}

fn base_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("PARTII_WR_PROGRAM_CACHE_DIR") {
        return Some(d.into());
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cache = if cfg!(target_os = "macos") {
        home.map(|h| h.join("Library/Caches"))
    } else if cfg!(target_os = "windows") {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| home.map(|h| h.join(".cache")))
    };
    cache.map(|c| c.join("partii").join("wr-programs"))
}

struct DiskCache {
    dir: PathBuf,
}

impl DiskCache {
    fn file(dir: &Path, digest: &ProgramSourceDigest) -> PathBuf {
        dir.join(format!("{digest}.bin"))
    }
}

impl ProgramCacheObserver for DiskCache {
    fn save_shaders_to_disk(&self, entries: Vec<Arc<ProgramBinary>>) {
        if entries.is_empty() {
            return;
        }
        let dir = self.dir.clone();
        let spawned = std::thread::Builder::new()
            .name("WRProgramCache".to_owned())
            .spawn(move || {
                for binary in entries {
                    let path = DiskCache::file(&dir, binary.source_digest());
                    if path.exists() {
                        continue;
                    }
                    let Ok(bytes) = postcard::to_stdvec(&*binary) else {
                        continue;
                    };
                    // Whole files only: a reader never sees a half-written one.
                    let tmp = path.with_extension("tmp");
                    if std::fs::write(&tmp, bytes).is_ok() {
                        let _ = std::fs::rename(&tmp, &path);
                    }
                }
            });
        if let Err(e) = spawned {
            warn!("WebRender program cache: no writer thread: {e}");
        }
    }

    fn set_startup_shaders(&self, entries: Vec<Arc<ProgramBinary>>) {
        // The programs the first frames linked (WebRender's tenth frame): kept as a list for
        // whoever wants to load them early; loading is per program on demand today.
        let list: String = entries
            .iter()
            .map(|b| format!("{}\n", b.source_digest()))
            .collect();
        let path = self.dir.join("startup.txt");
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, list).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    fn try_load_shader_from_disk(&self, digest: &ProgramSourceDigest, program_cache: &Rc<ProgramCache>) {
        let path = DiskCache::file(&self.dir, digest);
        let Ok(meta) = std::fs::metadata(&path) else {
            return;
        };
        if meta.len() > MAX_BINARY {
            return;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            return;
        };
        match postcard::from_bytes::<ProgramBinary>(&bytes) {
            Ok(binary) if binary.source_digest() == digest => {
                program_cache.load_program_binary(Arc::new(binary));
            },
            _ => {
                let _ = std::fs::remove_file(&path);
            },
        }
    }

    fn notify_program_binary_failed(&self, program_binary: &Arc<ProgramBinary>) {
        let _ = std::fs::remove_file(DiskCache::file(&self.dir, program_binary.source_digest()));
    }
}
