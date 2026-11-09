/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Instant;
use std::{env, fmt};

use phf_shared::{self, FmtConst};
use serde_json::{self, Value};

fn main() {
    let start = Instant::now();

    let style_out_dir = PathBuf::from(env::var_os("DEP_SERVO_STYLE_CRATE_OUT_DIR").unwrap());
    let css_properties_json = style_out_dir.join("css-properties.json");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());

    println!("cargo:out_dir={}", out_dir.display());

    build_incremental_gc_shim();

    println!("cargo::rerun-if-changed=webidls");
    println!("cargo::rerun-if-changed=codegen");
    println!("cargo::rerun-if-changed={}", css_properties_json.display());
    println!("cargo::rerun-if-changed=third_party/WebIDL/parser/WebIDL.py");
    println!("cargo::rerun-if-changed=third_party/ply");

    let status = find_python()
        .arg("codegen/run.py")
        .arg(&css_properties_json)
        .arg(&out_dir)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .status()
        .unwrap();
    if !status.success() {
        std::process::exit(1)
    }

    println!("Binding generation completed in {:?}", start.elapsed());

    let json = out_dir.join("InterfaceObjectMapData.json");
    let json: Value = serde_json::from_reader(File::open(json).unwrap()).unwrap();
    let mut map = phf_codegen::Map::new();
    for (key, value) in json.as_object().unwrap() {
        let parts = value.as_array().unwrap();
        map.entry(
            Bytes(key),
            format!(
                "Interface {{ define: {}, enabled: {} }}",
                parts[0].as_str().unwrap(),
                parts[1].as_str().unwrap()
            ),
        );
    }
    let phf = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("InterfaceObjectMapPhf.rs");
    let mut phf = File::create(phf).unwrap();
    writeln!(
        &mut phf,
        "pub(crate) static MAP: phf::Map<&'static [u8], Interface> = {};",
        map.build(),
    )
    .unwrap();
}

#[derive(Eq, Hash, PartialEq)]
struct Bytes<'a>(&'a str);

impl FmtConst for Bytes<'_> {
    fn fmt_const(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "b\"{}\"", self.0)
    }
}

impl phf_shared::PhfHash for Bytes<'_> {
    fn phf_hash<H: std::hash::Hasher>(&self, hasher: &mut H) {
        self.0.as_bytes().phf_hash(hasher)
    }
}

/// Tests if a python command works by running it with --version.
/// Returns Ok(()) if it works, Err with message if not.
fn try_python_command(program: &str, args: &[&str]) -> Result<(), String> {
    let mut command = Command::new(program);
    command.args(args);
    command.arg("--version");
    command.stdin(Stdio::null());

    let command_result = command.output();

    if let Ok(output) = command_result {
        return if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "`{} {:?}` failed with {}",
                program,
                args,
                String::from_utf8_lossy(&output.stderr)
            ))
        };
    }
    Err(format!(
        "`{} {:?}` failed to run (is it installed?)",
        program, args
    ))
}

/// Tries to find a suitable python, which in Servo is always `uv run python`.
///
/// To be accommodating to different environments, which may manage python differently, we fallback
/// to `python3` and `python` in that order.
fn find_python() -> Command {
    // Test uv first - if it works, create a FRESH command to return
    let uv_result = try_python_command("uv", &["run", "--frozen", "python"])
        .inspect_err(|e| println!("cargo:warning={e}"));
    if uv_result.is_ok() {
        let mut cmd = Command::new("uv");
        cmd.args(["run", "--frozen", "python"]);
        return cmd;
    }

    println!(
        "cargo:warning=`uv` not found - Falling back to the default python! \
        If the build fails, please install uv and make sure it is in your PATH or make sure \
        to provision a python environment >= python 3.11."
    );

    let python3_result = try_python_command("python3", &[]);
    if python3_result.is_ok() {
        return Command::new("python3");
    }

    let python_result = try_python_command("python", &[]);
    if python_result.is_ok() {
        return Command::new("python");
    }

    // We first try `python` before printing an error for `python3`, since python3 is often missing
    // provided via python on Windows (but not necessarily on linux).
    println!("cargo:warning={}", python3_result.unwrap_err());
    println!("cargo:warning={}", python_result.unwrap_err());

    panic!("No suitable python found! Tried: `uv run python`, `python3`, `python`.");
}

/// Partii: compiles incremental_gc_shim.cpp (a JS::CallbackTracer for the incremental GC's black
/// remark) against the SpiderMonkey headers of the mozjs_sys build we link. mozjs_sys doesn't
/// export its build directory, so it is found among the sibling build directories (this crate
/// depends on mozjs_sys directly, so its build script has run, and its output is one of them).
///
/// The headers must come from the same SpiderMonkey build as the library, by construction:
/// - Which build: every sibling `mozjs_sys-*/out/build` with bindings (`jsapi.rs`) counts. Exactly one,
///   or several with byte-identical bindings; otherwise (two configurations in one target dir) it is
///   ambiguous and the shim is left out.
/// - Its headers belong to its library when either
///   - it was unpacked from a Partii mozjs archive (tools/gc/mozjs-archive.sh in partii-runtime: the
///     library, bindings and headers of one from-source build, with `partii-mozjs.txt` naming the
///     bindings' length and FNV-1a hash), and the bindings there still match that note; or
///   - it is a from-source build (`MOZJS_FROM_SOURCE=1`) that never unpacked servo/mozjs's prebuilt
///     archive (no `libmozjs-*.tar.gz` next to it; that archive ships no headers and would leave
///     older ones beside a different library).
/// - `PARTII_MOZJS_HEADERS=<dir>` overrides all of it (a headers bundle; the caller vouches for it).
/// Without such headers (servo/mozjs's prebuilt archive) the shim is left out: `partii_gc_shim`
/// stays unset, the black remark is a no-op and incremental sweeping stays off, so such builds keep
/// stage-1 behaviour instead of failing. The runtime still compares `sizeof(JSTracer)` as a last check.
fn build_incremental_gc_shim() {
    println!("cargo::rerun-if-changed=incremental_gc_shim.cpp");
    println!("cargo::rustc-check-cfg=cfg(partii_gc_shim)");
    if env::var("CARGO_CFG_WINDOWS").is_ok() {
        return;
    }
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let build_root = out_dir
        .parent()
        .and_then(|p| p.parent())
        .unwrap()
        .to_path_buf();
    let has_headers = |b: &std::path::Path| {
        b.join("dist/include/jsapi.h").exists() && b.join("js/src/js-confdefs.h").exists()
    };
    println!("cargo::rerun-if-env-changed=PARTII_MOZJS_HEADERS");
    let chosen = if let Some(dir) = env::var_os("PARTII_MOZJS_HEADERS").map(PathBuf::from) {
        if has_headers(&dir) {
            Ok(dir)
        } else {
            Err(format!(
                "PARTII_MOZJS_HEADERS={} has no dist/include/jsapi.h or js/src/js-confdefs.h",
                dir.display()
            ))
        }
    } else {
        choose_mozjs_build(&build_root, has_headers)
    };
    let build_dir = match chosen {
        Ok(dir) => {
            println!("cargo::rerun-if-changed={}", dir.join("jsapi.rs").display());
            println!("cargo::rerun-if-changed={}", dir.join("js/src/js-confdefs.h").display());
            dir
        },
        Err(why) => {
            println!(
                "cargo::warning=incremental_gc_shim: {why}; built without the black remark \
                 (MOZJS_FROM_SOURCE=1, or MOZJS_ARCHIVE=<a Partii mozjs archive>, builds it)"
            );
            return;
        },
    };
    println!(
        "cargo::warning=incremental_gc_shim: SpiderMonkey headers from {}",
        build_dir.display()
    );
    let config =
        std::fs::read_to_string(build_dir.join("dist/include/js-config.h")).unwrap_or_default();
    let debug = config.contains("#define JS_DEBUG 1");
    let mut cc = cc::Build::new();
    cc.cpp(true)
        .std("gnu++20")
        .pic(true)
        .flag_if_supported("-fno-rtti")
        .flag_if_supported("-fno-sized-deallocation")
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-invalid-offsetof")
        .flag("-include")
        .flag(build_dir.join("js/src/js-confdefs.h").display().to_string())
        .include(build_dir.join("dist/include"))
        .define("STATIC_JS_API", None)
        .file("incremental_gc_shim.cpp");
    if debug {
        cc.define("JS_GC_ZEAL", None)
            .define("DEBUG", None)
            .define("JS_DEBUG", None);
    }
    cc.compile("partii_incremental_gc_shim");
    println!("cargo::rustc-cfg=partii_gc_shim");
}

/// FNV-1a, 64 bit (tools/gc/mozjs-archive.sh writes the same for jsapi.rs into partii-mozjs.txt).
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// The mozjs_sys build we link and whose headers belong to it, or why there is none (see
/// `build_incremental_gc_shim`).
fn choose_mozjs_build(
    build_root: &std::path::Path,
    has_headers: impl Fn(&std::path::Path) -> bool,
) -> Result<PathBuf, String> {
    let builds: Vec<(PathBuf, Vec<u8>)> = std::fs::read_dir(build_root)
        .map_err(|e| format!("can't list {}: {e}", build_root.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("mozjs_sys-"))
        .map(|e| e.path().join("out").join("build"))
        .filter_map(|b| Some((b.clone(), std::fs::read(b.join("jsapi.rs")).ok()?)))
        .collect();
    let Some((_, first)) = builds.first() else {
        return Err(format!("no mozjs_sys build under {}", build_root.display()));
    };
    if builds.iter().any(|(_, bindings)| bindings != first) {
        return Err(format!(
            "{} mozjs_sys builds with different bindings under {} (which one is linked is unknown)",
            builds.len(),
            build_root.display()
        ));
    }
    let hash = format!("fnv1a64 {:016x} bytes {}", fnv1a64(first), first.len());
    let mut why = Vec::new();
    for (b, _) in &builds {
        if !has_headers(b) {
            why.push(format!("{}: no headers (servo/mozjs prebuilt archive?)", b.display()));
            continue;
        }
        if let Ok(note) = std::fs::read_to_string(b.join("partii-mozjs.txt")) {
            if note.lines().any(|l| l.trim() == format!("jsapi.rs {hash}")) {
                println!("cargo::warning=incremental_gc_shim: Partii mozjs archive in {} ({})", b.display(),
                    note.lines().next().unwrap_or("").trim());
                return Ok(b.clone());
            }
        }
        let out = b.parent().unwrap();
        let unpacked_prebuilt = std::fs::read_dir(out)
            .map(|d| {
                d.filter_map(|e| e.ok()).any(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    n.starts_with("libmozjs-") && n.ends_with(".tar.gz")
                })
            })
            .unwrap_or(true);
        if !unpacked_prebuilt && b.join("js/src/build").is_dir() {
            return Ok(b.clone());
        }
        why.push(format!(
            "{}: headers not proven to match the library (servo/mozjs archive unpacked over a source build?)",
            b.display()
        ));
    }
    Err(why.join("; "))
}
