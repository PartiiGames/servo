/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii (stream frame-attr): spans of WebRender's phases for the embedder's per-frame trace
//! (Servo's `servo_base::frame_trace`, `PARTII_FRAME_TRACE`). WebRender doesn't know that crate:
//! the embedder sets two functions here before it creates the renderer, and every span calls them
//! (nothing when unset).

use std::sync::OnceLock;

/// A WebRender phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    /// The scene builder building a scene from a transaction.
    SceneBuild,
    /// The render backend handling a document update (resources, then maybe a frame build).
    Backend,
    /// The render backend building a frame.
    FrameBuild,
    /// The renderer uploading to the texture cache.
    TextureUpload,
    /// Compiling and linking a shader program.
    ShaderCompile,
}

/// What a span calls: `enter(phase)` at its start, `exit(phase)` at its end.
#[derive(Clone, Copy)]
pub struct Hooks {
    pub enter: fn(Phase),
    pub exit: fn(Phase),
}

static HOOKS: OnceLock<Hooks> = OnceLock::new();

/// Set once, before the first renderer is created.
pub fn set_hooks(hooks: Hooks) {
    let _ = HOOKS.set(hooks);
}

pub(crate) struct Span(Option<(Hooks, Phase)>);

impl Drop for Span {
    fn drop(&mut self) {
        if let Some((hooks, phase)) = self.0 {
            (hooks.exit)(phase);
        }
    }
}

#[inline]
pub(crate) fn span(phase: Phase) -> Span {
    match HOOKS.get() {
        Some(hooks) => {
            (hooks.enter)(phase);
            Span(Some((*hooks, phase)))
        }
        None => Span(None),
    }
}
