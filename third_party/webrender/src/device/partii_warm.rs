/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Partii (gaps-6): draw-state warm-up for software GL.
//!
//! A software GL (Mesa's llvmpipe, softpipe, swrast) compiles a shader *variant* the first time a
//! program is drawn with a given state: render target format, blend and depth state, and the
//! format, filtering and swizzle of every sampled texture. That compile runs inside the draw
//! call on WebRender's thread: 80-170 ms per variant with a cold Mesa cache and still several ms
//! with a warm one, so the first frame of every new screen (hexii's round start and GAME OVER
//! overlay) stalls for 100 ms to over a second.
//!
//! The recorder notes each (program, draw state) WebRender draws with and appends the new ones to
//! a small text file next to the Mesa shader cache. On the next start the renderer replays them:
//! one degenerate instance per entry into small scratch targets of the same formats, before the
//! first frame, so the variants exist when the page needs them. GPUs compile at link time, so this
//! is on only for software GL. `PARTII_WR_WARMUP=0` turns it off, `=1` forces it on any GL.
//! `PARTII_WR_WARMUP_FILE` names the file, `PARTII_WR_WARMUP_MS` caps the replay (default 4000).

use super::*;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::io::Write as _;

const MAX_ENTRIES: usize = 4000;
const STATE_ENUMS: [gl::GLenum; 10] = [
    gl::BLEND,
    gl::BLEND_SRC_RGB,
    gl::BLEND_DST_RGB,
    gl::BLEND_SRC_ALPHA,
    gl::BLEND_DST_ALPHA,
    gl::BLEND_EQUATION_RGB,
    gl::BLEND_EQUATION_ALPHA,
    gl::DEPTH_TEST,
    gl::DEPTH_FUNC,
    gl::DEPTH_WRITEMASK,
];

fn is_software_gl(renderer: &str) -> bool {
    let r = renderer.to_ascii_lowercase();
    r.contains("llvmpipe") || r.contains("softpipe") || r.contains("swrast")
}

/// Whether warm-up runs for this GL (`PARTII_WR_WARMUP`: unset = software GL only).
pub(crate) fn enabled_for(renderer: &str) -> bool {
    match std::env::var("PARTII_WR_WARMUP").as_deref() {
        Ok("0") => false,
        Ok("1") => true,
        _ => is_software_gl(renderer),
    }
}

fn file_path() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    if let Some(p) = std::env::var_os("PARTII_WR_WARMUP_FILE") {
        return Some(PathBuf::from(p));
    }
    // Next to Mesa's shader cache: both hold compiled state for this GL, and live and go together.
    if let Some(d) = std::env::var_os("MESA_SHADER_CACHE_DIR") {
        return Some(PathBuf::from(d).join("partii-wr-warmup.txt"));
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("partii").join("wr-warmup.txt"))
}

/// What a texture unit held at a recorded draw.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct UnitState {
    unit: usize,
    target: gl::GLenum,
    /// `None` for an external texture (the format isn't known here: RGBA8 is used).
    format: Option<ImageFormat>,
    pot_w: bool,
    pot_h: bool,
    min_filter: gl::GLint,
    mag_filter: gl::GLint,
    swizzle: [gl::GLint; 4],
}

/// One recorded draw state: the program, the target, the raw blend/depth/colour-mask state and the
/// bound textures.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct WarmEntry {
    pub(crate) program: String,
    /// `None` = the default framebuffer.
    target: Option<(ImageFormat, bool)>,
    state: [gl::GLint; 10],
    color_mask: [bool; 4],
    units: Vec<UnitState>,
}

fn fmt_name(f: ImageFormat) -> &'static str {
    match f {
        ImageFormat::R8 => "R8",
        ImageFormat::R16 => "R16",
        ImageFormat::BGRA8 => "BGRA8",
        ImageFormat::RGBAF32 => "RGBAF32",
        ImageFormat::RG8 => "RG8",
        ImageFormat::RG16 => "RG16",
        ImageFormat::RGBAI32 => "RGBAI32",
        ImageFormat::RGBA8 => "RGBA8",
    }
}

fn fmt_parse(s: &str) -> Option<ImageFormat> {
    Some(match s {
        "R8" => ImageFormat::R8,
        "R16" => ImageFormat::R16,
        "BGRA8" => ImageFormat::BGRA8,
        "RGBAF32" => ImageFormat::RGBAF32,
        "RG8" => ImageFormat::RG8,
        "RG16" => ImageFormat::RG16,
        "RGBAI32" => ImageFormat::RGBAI32,
        "RGBA8" => ImageFormat::RGBA8,
        _ => return None,
    })
}

impl WarmEntry {
    /// `prog|tgt|s0,s1,..|cmask|unit:target:fmt:pw:ph:min:mag:r,g,b,a;...`
    fn to_line(&self) -> String {
        let tgt = match self.target {
            None => "default".to_owned(),
            Some((f, d)) => format!("{}{}", fmt_name(f), if d { "+d" } else { "" }),
        };
        let st: Vec<String> = self.state.iter().map(|v| v.to_string()).collect();
        let cm: String = self.color_mask.iter().map(|b| if *b { '1' } else { '0' }).collect();
        let units: Vec<String> = self
            .units
            .iter()
            .map(|u| {
                format!(
                    "{}:{}:{}:{}:{}:{}:{}:{},{},{},{}",
                    u.unit,
                    u.target,
                    u.format.map_or("ext", fmt_name),
                    u.pot_w as u8,
                    u.pot_h as u8,
                    u.min_filter,
                    u.mag_filter,
                    u.swizzle[0],
                    u.swizzle[1],
                    u.swizzle[2],
                    u.swizzle[3],
                )
            })
            .collect();
        format!("{}|{}|{}|{}|{}", self.program, tgt, st.join(","), cm, units.join(";"))
    }

    fn from_line(line: &str) -> Option<Self> {
        let mut parts = line.split('|');
        let program = parts.next()?.to_owned();
        let tgt = parts.next()?;
        let target = if tgt == "default" {
            None
        } else {
            let (f, d) = match tgt.strip_suffix("+d") {
                Some(f) => (f, true),
                None => (tgt, false),
            };
            Some((fmt_parse(f)?, d))
        };
        let mut state = [0; 10];
        let st: Vec<gl::GLint> = parts.next()?.split(',').filter_map(|v| v.parse().ok()).collect();
        if st.len() != state.len() {
            return None;
        }
        state.copy_from_slice(&st);
        let cm = parts.next()?.as_bytes();
        if cm.len() != 4 {
            return None;
        }
        let color_mask = [cm[0] == b'1', cm[1] == b'1', cm[2] == b'1', cm[3] == b'1'];
        let mut units = Vec::new();
        for u in parts.next()?.split(';').filter(|u| !u.is_empty()) {
            let f: Vec<&str> = u.split(':').collect();
            if f.len() != 8 {
                return None;
            }
            let sw: Vec<gl::GLint> = f[7].split(',').filter_map(|v| v.parse().ok()).collect();
            if sw.len() != 4 {
                return None;
            }
            let unit: usize = f[0].parse().ok()?;
            if unit >= 16 {
                return None;
            }
            units.push(UnitState {
                unit,
                target: f[1].parse().ok()?,
                format: if f[2] == "ext" { None } else { Some(fmt_parse(f[2])?) },
                pot_w: f[3] == "1",
                pot_h: f[4] == "1",
                min_filter: f[5].parse().ok()?,
                mag_filter: f[6].parse().ok()?,
                swizzle: [sw[0], sw[1], sw[2], sw[3]],
            });
        }
        Some(WarmEntry { program, target, state, color_mask, units })
    }
}

/// Per-device recorder state (only made when warm-up is on for this GL).
pub(crate) struct Recorder {
    path: Option<std::path::PathBuf>,
    /// Texture id -> (format, width, height) for WebRender's own textures.
    textures: FastHashMap<gl::GLuint, (ImageFormat, i32, i32)>,
    /// The GL target last bound on each unit.
    unit_targets: [gl::GLenum; 16],
    /// Hashes of the cheap per-draw key already looked at.
    seen_keys: HashSet<u64>,
    /// Entries in the file or recorded in this process.
    known: HashSet<String>,
    /// Entries read at start, for the replay.
    loaded: Vec<WarmEntry>,
}

impl Recorder {
    pub(crate) fn new() -> Box<Self> {
        let path = file_path();
        let mut known = HashSet::new();
        let mut loaded = Vec::new();
        if let Some(text) = path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()) {
            for line in text.lines() {
                if known.len() >= MAX_ENTRIES {
                    break;
                }
                if known.contains(line) {
                    continue;
                }
                if let Some(e) = WarmEntry::from_line(line) {
                    known.insert(line.to_owned());
                    loaded.push(e);
                }
            }
        }
        Box::new(Recorder {
            path,
            textures: FastHashMap::default(),
            unit_targets: [gl::TEXTURE_2D; 16],
            seen_keys: HashSet::new(),
            known,
            loaded,
        })
    }

    fn append(&mut self, line: String) {
        if self.known.len() >= MAX_ENTRIES || !self.known.insert(line.clone()) {
            return;
        }
        let Some(path) = self.path.as_ref() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{line}");
        }
    }
}

fn is_pot(v: i32) -> bool {
    v > 0 && (v & (v - 1)) == 0
}

impl Device {
    pub(crate) fn partii_warm_note_texture(&mut self, id: gl::GLuint, format: ImageFormat, w: i32, h: i32) {
        if let Some(r) = self.partii_warm.as_mut() {
            r.textures.insert(id, (format, w, h));
        }
    }

    pub(crate) fn partii_warm_forget_texture(&mut self, id: gl::GLuint) {
        if let Some(r) = self.partii_warm.as_mut() {
            r.textures.remove(&id);
        }
    }

    pub(crate) fn partii_warm_note_unit(&mut self, unit: usize, target: gl::GLenum) {
        if let Some(r) = self.partii_warm.as_mut() {
            r.unit_targets[unit] = target;
        }
    }

    fn partii_raw_state(&self) -> ([gl::GLint; 10], [bool; 4]) {
        let mut state = [0; 10];
        for (i, e) in STATE_ENUMS.iter().enumerate() {
            let mut v = [0];
            unsafe { self.gl.get_integer_v(*e, &mut v) };
            state[i] = v[0];
        }
        // Blend factors and equations only matter with blending on; depth func only with the test on.
        if state[0] == 0 {
            for s in &mut state[1..7] {
                *s = 0;
            }
        }
        if state[7] == 0 {
            state[8] = 0;
        }
        let mut cm = [0u8; 4];
        unsafe { self.gl.get_boolean_v(gl::COLOR_WRITEMASK, &mut cm) };
        (state, [cm[0] != 0, cm[1] != 0, cm[2] != 0, cm[3] != 0])
    }

    /// Called before each WebRender draw while recording.
    pub(crate) fn partii_warm_note_draw(&mut self) {
        if self.partii_warm.is_none() {
            return;
        }
        let (state, color_mask) = self.partii_raw_state();
        let mut h = DefaultHasher::new();
        (self.bound_program, self.bound_draw_fbo.0, self.bound_textures, state, color_mask).hash(&mut h);
        let key = h.finish();
        {
            let r = self.partii_warm.as_mut().unwrap();
            if r.seen_keys.len() > 200_000 {
                r.seen_keys.clear();
            }
            if !r.seen_keys.insert(key) {
                return;
            }
        }
        let program = self.bound_program_name.to_string_lossy().into_owned();
        if program.is_empty() {
            return;
        }
        let target = if self.bound_draw_fbo == self.default_draw_fbo {
            None
        } else {
            let ty = self.gl.get_framebuffer_attachment_parameter_iv(
                gl::DRAW_FRAMEBUFFER,
                gl::COLOR_ATTACHMENT0,
                gl::FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE,
            );
            let name = self.gl.get_framebuffer_attachment_parameter_iv(
                gl::DRAW_FRAMEBUFFER,
                gl::COLOR_ATTACHMENT0,
                gl::FRAMEBUFFER_ATTACHMENT_OBJECT_NAME,
            );
            let depth = self.gl.get_framebuffer_attachment_parameter_iv(
                gl::DRAW_FRAMEBUFFER,
                gl::DEPTH_ATTACHMENT,
                gl::FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE,
            ) != gl::NONE as gl::GLint;
            let r = self.partii_warm.as_ref().unwrap();
            let format = if ty == gl::TEXTURE as gl::GLint {
                r.textures.get(&(name as gl::GLuint)).map(|t| t.0)
            } else {
                None
            };
            // An FBO on a texture we didn't make (an external target): not replayable.
            Some((format.unwrap_or(ImageFormat::RGBA8), depth))
        };
        let mut units = Vec::new();
        for unit in 0 .. self.bound_textures.len() {
            let id = self.bound_textures[unit];
            if id == 0 {
                continue;
            }
            let target = self.partii_warm.as_ref().unwrap().unit_targets[unit];
            let info = self.partii_warm.as_ref().unwrap().textures.get(&id).copied();
            self.gl.active_texture(gl::TEXTURE0 + unit as gl::GLuint);
            let min_filter = self.gl.get_tex_parameter_iv(target, gl::TEXTURE_MIN_FILTER);
            let mag_filter = self.gl.get_tex_parameter_iv(target, gl::TEXTURE_MAG_FILTER);
            let swizzle = if self.capabilities.supports_texture_swizzle {
                [
                    self.gl.get_tex_parameter_iv(target, gl::TEXTURE_SWIZZLE_R),
                    self.gl.get_tex_parameter_iv(target, gl::TEXTURE_SWIZZLE_G),
                    self.gl.get_tex_parameter_iv(target, gl::TEXTURE_SWIZZLE_B),
                    self.gl.get_tex_parameter_iv(target, gl::TEXTURE_SWIZZLE_A),
                ]
            } else {
                [gl::RED as gl::GLint, gl::GREEN as gl::GLint, gl::BLUE as gl::GLint, gl::ALPHA as gl::GLint]
            };
            units.push(UnitState {
                unit,
                target,
                format: info.map(|i| i.0),
                // An external texture's size isn't known: canvases and video frames are rarely a power of two.
                pot_w: info.map_or(false, |i| is_pot(i.1)),
                pot_h: info.map_or(false, |i| is_pot(i.2)),
                min_filter,
                mag_filter,
                swizzle,
            });
        }
        self.gl.active_texture(gl::TEXTURE0);
        let line = WarmEntry { program, target, state, color_mask, units }.to_line();
        self.partii_warm.as_mut().unwrap().append(line);
    }

    /// The entries the file held at start (empty when warm-up is off).
    pub(crate) fn partii_warm_entries(&mut self) -> Vec<WarmEntry> {
        self.partii_warm.as_mut().map_or(Vec::new(), |r| std::mem::take(&mut r.loaded))
    }

    /// Draws one degenerate instance of the bound program with `e`'s state. The program is bound
    /// and `vao` is its vertex layout. Scratch textures are kept in `scratch` by their state.
    pub(crate) fn partii_warm_draw(
        &mut self,
        e: &WarmEntry,
        vao: &VAO,
        scratch: &mut WarmScratch,
    ) {
        // Target.
        match e.target {
            None => {
                let fbo = self.default_draw_fbo;
                self.bind_draw_target_impl(fbo);
                self.gl.viewport(0, 0, 4, 4);
            },
            Some((format, depth)) => {
                let fbo = scratch.target(self, format, depth);
                self.bind_draw_target_impl(fbo);
                self.gl.viewport(0, 0, 4, 4);
            },
        }
        // Textures.
        for u in &e.units {
            let id = scratch.texture(self, u);
            self.gl.active_texture(gl::TEXTURE0 + u.unit as gl::GLuint);
            self.gl.bind_texture(u.target, id);
            self.bound_textures[u.unit] = id;
        }
        self.gl.active_texture(gl::TEXTURE0);
        // Raw blend / depth / colour mask.
        let s = &e.state;
        if s[0] != 0 {
            self.gl.enable(gl::BLEND);
            self.gl.blend_func_separate(s[1] as _, s[2] as _, s[3] as _, s[4] as _);
            self.gl.blend_equation_separate(s[5] as _, s[6] as _);
        } else {
            self.gl.disable(gl::BLEND);
        }
        if s[7] != 0 {
            self.gl.enable(gl::DEPTH_TEST);
            self.gl.depth_func(s[8] as _);
        } else {
            self.gl.disable(gl::DEPTH_TEST);
        }
        self.gl.depth_mask(s[9] != 0);
        let m = e.color_mask;
        self.gl.color_mask(m[0], m[1], m[2], m[3]);
        // One zeroed instance: the vertex shader makes a degenerate quad, but the GL still builds
        // the draw's shader variants.
        self.bind_vao(vao);
        self.gl.bind_buffer(gl::ARRAY_BUFFER, vao.instance_vbo_id.0);
        let zeros = vec![0u8; vao.instance_stride.max(16)];
        self.gl.buffer_data_untyped(
            gl::ARRAY_BUFFER,
            zeros.len() as _,
            zeros.as_ptr() as *const _,
            gl::STREAM_DRAW,
        );
        if vao.instance_divisor == 0 {
            self.gl.draw_elements(gl::TRIANGLES, 6, gl::UNSIGNED_SHORT, 0);
        } else {
            self.gl.draw_elements_instanced(gl::TRIANGLES, 6, gl::UNSIGNED_SHORT, 0, 1);
        }
    }

    /// Back to WebRender's default state after a replay.
    pub(crate) fn partii_warm_finish(&mut self, scratch: WarmScratch) {
        self.gl.disable(gl::BLEND);
        self.gl.disable(gl::DEPTH_TEST);
        self.gl.depth_mask(true);
        self.gl.color_mask(true, true, true, true);
        for t in scratch.textures.values() {
            self.gl.delete_textures(&[*t]);
        }
        for (fbo, tex, depth) in scratch.targets.values() {
            self.gl.delete_framebuffers(&[*fbo]);
            self.gl.delete_textures(&[*tex]);
            if let Some(d) = depth {
                self.gl.delete_renderbuffers(&[*d]);
            }
        }
        self.gl.finish();
        self.reset_state();
    }
}

/// Scratch textures and render targets for a replay.
#[derive(Default)]
pub(crate) struct WarmScratch {
    textures: FastHashMap<(gl::GLenum, Option<ImageFormat>, bool, bool, gl::GLint, gl::GLint, [gl::GLint; 4]), gl::GLuint>,
    targets: FastHashMap<(ImageFormat, bool), (gl::GLuint, gl::GLuint, Option<gl::GLuint>)>,
}

impl WarmScratch {
    fn alloc(device: &mut Device, target: gl::GLenum, format: ImageFormat, w: i32, h: i32, levels: i32) -> gl::GLuint {
        let id = device.gl.gen_textures(1)[0];
        device.gl.bind_texture(target, id);
        let desc = device.gl_describe_format(format);
        let use_storage = match device.texture_storage_usage {
            TexStorageUsage::Always => true,
            TexStorageUsage::NonBGRA8 => format != ImageFormat::BGRA8,
            TexStorageUsage::Never => false,
        };
        if use_storage {
            device.gl.tex_storage_2d(target, levels, desc.internal, w, h);
        } else {
            for l in 0 .. levels {
                device.gl.tex_image_2d(
                    target,
                    l,
                    desc.internal as gl::GLint,
                    (w >> l).max(1),
                    (h >> l).max(1),
                    0,
                    desc.external,
                    desc.pixel_type,
                    None,
                );
            }
        }
        device.gl.tex_parameter_i(target, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as gl::GLint);
        device.gl.tex_parameter_i(target, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as gl::GLint);
        id
    }

    fn texture(&mut self, device: &mut Device, u: &UnitState) -> gl::GLuint {
        let key = (u.target, u.format, u.pot_w, u.pot_h, u.min_filter, u.mag_filter, u.swizzle);
        if let Some(id) = self.textures.get(&key) {
            return *id;
        }
        // Only 2D and rectangle textures can be made here; an external OES target gets a 2D one.
        let target = if u.target == gl::TEXTURE_RECTANGLE { gl::TEXTURE_RECTANGLE } else { gl::TEXTURE_2D };
        let mips = u.min_filter == gl::LINEAR_MIPMAP_LINEAR as gl::GLint ||
            u.min_filter == gl::NEAREST_MIPMAP_NEAREST as gl::GLint ||
            u.min_filter == gl::LINEAR_MIPMAP_NEAREST as gl::GLint ||
            u.min_filter == gl::NEAREST_MIPMAP_LINEAR as gl::GLint;
        let (w, h) = (if u.pot_w { 4 } else { 6 }, if u.pot_h { 4 } else { 6 });
        let levels = if mips && target == gl::TEXTURE_2D { 3 } else { 1 };
        device.gl.active_texture(gl::TEXTURE0 + u.unit as gl::GLuint);
        let id = Self::alloc(device, target, u.format.unwrap_or(ImageFormat::RGBA8), w, h, levels);
        device.gl.tex_parameter_i(target, gl::TEXTURE_MIN_FILTER, u.min_filter);
        device.gl.tex_parameter_i(target, gl::TEXTURE_MAG_FILTER, u.mag_filter);
        if device.capabilities.supports_texture_swizzle {
            device.gl.tex_parameter_i(target, gl::TEXTURE_SWIZZLE_R, u.swizzle[0]);
            device.gl.tex_parameter_i(target, gl::TEXTURE_SWIZZLE_G, u.swizzle[1]);
            device.gl.tex_parameter_i(target, gl::TEXTURE_SWIZZLE_B, u.swizzle[2]);
            device.gl.tex_parameter_i(target, gl::TEXTURE_SWIZZLE_A, u.swizzle[3]);
        }
        device.gl.active_texture(gl::TEXTURE0);
        self.textures.insert(key, id);
        id
    }

    fn target(&mut self, device: &mut Device, format: ImageFormat, depth: bool) -> FBOId {
        if let Some((fbo, ..)) = self.targets.get(&(format, depth)) {
            return FBOId(*fbo);
        }
        let tex = Self::alloc(device, gl::TEXTURE_2D, format, 4, 4, 1);
        device.gl.tex_parameter_i(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::NEAREST as gl::GLint);
        device.gl.tex_parameter_i(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::NEAREST as gl::GLint);
        let fbo = device.gl.gen_framebuffers(1)[0];
        device.gl.bind_framebuffer(gl::DRAW_FRAMEBUFFER, fbo);
        device.gl.framebuffer_texture_2d(gl::DRAW_FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::TEXTURE_2D, tex, 0);
        let rb = if depth {
            let rb = device.gl.gen_renderbuffers(1)[0];
            device.gl.bind_renderbuffer(gl::RENDERBUFFER, rb);
            device.gl.renderbuffer_storage(gl::RENDERBUFFER, device.depth_format, 4, 4);
            device.gl.framebuffer_renderbuffer(gl::DRAW_FRAMEBUFFER, gl::DEPTH_ATTACHMENT, gl::RENDERBUFFER, rb);
            Some(rb)
        } else {
            None
        };
        // Force the next bind_draw_target_impl to rebind.
        device.bound_draw_fbo = FBOId(fbo);
        self.targets.insert((format, depth), (fbo, tex, rb));
        FBOId(fbo)
    }
}
