/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii (stream webgl-deletes): glDeleteTextures made while the WebGL thread is idle.
//!
//! GPU slot run-1123-0144 (Apple M5): with the compile pauses gone, hexii's and rocket-to-heaven's last runtime pauses
//! over 16.7 ms are their restarts. Phaser deletes every texture it made (72-93 DeleteTexture back to back) and
//! Apple's GL takes 0.2-0.3 ms each (up to 2.6 ms), 13-26 ms in one frame, all inside the driver's
//! glDeleteTextures. Script has already let go of the texture (it answers isTexture itself and never names a deleted
//! one again), and GL doesn't hand a name out again before it is deleted, so the GL call can wait. It can't when GL
//! would change what the content sees: a texture bound to a texture unit (GL unbinds it, so later draws sample
//! nothing) or attached to a bound framebuffer (GL detaches it). Those are deleted at once. A texture ever bound to a
//! target other than TEXTURE_2D is deleted at once too: its unit bindings aren't followed. The waiting deletes run in
//! the deferred-work idle budget (webgl_thread drain_deferred_programs), all of them before a readPixels, and they
//! are dropped with their context (destroying the context frees them). `PARTII_WEBGL_DEFER_DELETES=0` turns this off.

use rustc_hash::{FxHashMap, FxHashSet};

/// `PARTII_WEBGL_DEFER_DELETES=0` turns deferred texture deletes off (A/B switch).
pub(crate) fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PARTII_WEBGL_DEFER_DELETES").map_or(true, |v| v != "0"))
}

/// More waiting than this and the oldest are deleted at once (their GPU memory is held until then).
pub(crate) const MAX_WAITING: usize = 1024;

/// What GL state says about one texture, as the content made it (GLState's tracking).
pub(crate) struct TextureUse<'a> {
    /// The TEXTURE_2D binding of each texture unit.
    pub(crate) unit_textures: &'a [u32],
    /// Textures ever bound to another target (their bindings aren't followed).
    pub(crate) other_target_textures: &'a FxHashSet<u32>,
    /// Textures ever attached to each framebuffer.
    pub(crate) framebuffer_textures: &'a FxHashMap<u32, Vec<u32>>,
    /// The framebuffers bound to DRAW_FRAMEBUFFER and READ_FRAMEBUFFER (0: the canvas's own).
    pub(crate) bound_framebuffers: (u32, u32),
}

impl TextureUse<'_> {
    /// Whether deleting `name` later leaves everything the content can see as deleting it now would.
    pub(crate) fn delete_may_wait(&self, name: u32) -> bool {
        let attached_to_bound = |framebuffer: u32| {
            framebuffer != 0
                && self
                    .framebuffer_textures
                    .get(&framebuffer)
                    .is_some_and(|attached| attached.contains(&name))
        };
        !self.unit_textures.contains(&name)
            && !self.other_target_textures.contains(&name)
            && !attached_to_bound(self.bound_framebuffers.0)
            && !attached_to_bound(self.bound_framebuffers.1)
    }
}

/// The texture deletes waiting in one context, oldest first.
#[derive(Default)]
pub(crate) struct DeferredDeletes {
    textures: Vec<u32>,
}

impl std::fmt::Debug for DeferredDeletes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DeferredDeletes({} waiting)", self.textures.len())
    }
}

impl DeferredDeletes {
    pub(crate) fn is_empty(&self) -> bool {
        self.textures.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.textures.len()
    }

    /// A delete that waits; with MAX_WAITING already waiting, the oldest is deleted now.
    pub(crate) fn push(&mut self, name: u32, delete: &mut impl FnMut(u32)) {
        if self.textures.len() >= MAX_WAITING {
            delete(self.textures.remove(0));
        }
        self.textures.push(name);
    }

    /// Deletes the oldest waiting while `more()` says so (checked before each); whether any are left.
    pub(crate) fn run(
        &mut self,
        mut more: impl FnMut() -> bool,
        delete: &mut impl FnMut(u32),
    ) -> bool {
        let mut done = 0;
        while done < self.textures.len() && more() {
            delete(self.textures[done]);
            done += 1;
        }
        self.textures.drain(..done);
        !self.textures.is_empty()
    }

    /// Deletes all of them now (before a command that could see them: readPixels).
    pub(crate) fn flush(&mut self, delete: &mut impl FnMut(u32)) {
        for name in self.textures.drain(..) {
            delete(name);
        }
    }
}

/// Whether a command makes every waiting delete run first. A readPixels reads what draws made, which sampled only
/// textures bound to units (never one waiting), so it can't see them; flushing first keeps it certain.
pub(crate) fn flushes(command: &servo_canvas_traits::webgl::WebGLCommand) -> bool {
    use servo_canvas_traits::webgl::WebGLCommand::*;
    matches!(command, ReadPixels(..) | ReadPixelsPP(..) | ReadPixelsCapture(..))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uses<'a>(
        units: &'a [u32],
        other: &'a FxHashSet<u32>,
        fbs: &'a FxHashMap<u32, Vec<u32>>,
        bound: (u32, u32),
    ) -> TextureUse<'a> {
        TextureUse {
            unit_textures: units,
            other_target_textures: other,
            framebuffer_textures: fbs,
            bound_framebuffers: bound,
        }
    }

    #[test]
    fn bound_attached_or_other_target_textures_are_deleted_now() {
        let other: FxHashSet<u32> = [9].into_iter().collect();
        let mut fbs = FxHashMap::default();
        fbs.insert(20, vec![5]);
        fbs.insert(21, vec![6]);
        let u = uses(&[3, 0, 4], &other, &fbs, (20, 0));
        // Bound to unit 0 / unit 2.
        assert!(!u.delete_may_wait(3));
        assert!(!u.delete_may_wait(4));
        // Attached to the bound draw framebuffer.
        assert!(!u.delete_may_wait(5));
        // Bound as a cube map / 3D texture once.
        assert!(!u.delete_may_wait(9));
        // Attached to a framebuffer that isn't bound: GL keeps it as the attachment either way.
        assert!(u.delete_may_wait(6));
        // Unused.
        assert!(u.delete_may_wait(7));
        // Bound for reading.
        let u = uses(&[], &other, &fbs, (0, 21));
        assert!(!u.delete_may_wait(6));
    }

    #[test]
    fn deletes_run_oldest_first_within_the_budget_and_flush() {
        let mut d = DeferredDeletes::default();
        let mut log = vec![];
        for name in 1..=5 {
            d.push(name, &mut |n| log.push(n));
        }
        assert!(log.is_empty());
        let mut budget = 2;
        let left = d.run(
            || {
                budget -= 1;
                budget >= 0
            },
            &mut |n| log.push(n),
        );
        assert!(left);
        assert_eq!(log, vec![1, 2]);
        d.flush(&mut |n| log.push(n));
        assert_eq!(log, vec![1, 2, 3, 4, 5]);
        assert!(d.is_empty());
        assert!(!d.run(|| true, &mut |n| log.push(n)));
    }

    #[test]
    fn too_many_waiting_deletes_the_oldest_now() {
        let mut d = DeferredDeletes::default();
        let mut log = vec![];
        for name in 0..(MAX_WAITING as u32 + 2) {
            d.push(name, &mut |n| log.push(n));
        }
        assert_eq!(log, vec![0, 1]);
        assert_eq!(d.len(), MAX_WAITING);
    }

    /// Every command that reads pixels flushes (a new one in the enum fails here until it is added to `flushes`).
    #[test]
    fn every_read_pixels_command_flushes() {
        let src = include_str!("../shared/canvas/webgl.rs");
        let start = src.find("pub enum WebGLCommand {").unwrap();
        let body = &src[start..];
        let body = &body[..body.find("\n}\n").unwrap()];
        let reads: Vec<&str> = body
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("ReadPixels"))
            .map(|l| l.split(|c: char| !c.is_alphanumeric()).next().unwrap())
            .collect();
        assert!(
            reads.contains(&"ReadPixels") && reads.contains(&"ReadPixelsPP"),
            "{reads:?}"
        );
        for name in &reads {
            assert!(
                include_str!("partii_deferred_deletes.rs").contains(&format!("{name}(..)")),
                "{name} must flush"
            );
        }
    }
}
