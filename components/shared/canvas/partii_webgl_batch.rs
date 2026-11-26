/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii (perf, wglbatch): WebGL command batching. Every WebGLCommand used to be its own channel message: on
//! llvmpipe a same-state draw cost ~0.68 us to issue and ~1.1 us in all, ~30x Chromium, and the per-message
//! work (crossbeam send/recv, the boxed command's alloc/free, the WebGL thread's dispatch) was the largest share
//! (perf job e6c9497c). Here a thread keeps the commands that need no reply in a thread-local batch and sends
//! them as one `WebGLMsg::WebGLCommandBatch`.
//!
//! Order is kept by flushing the batch before *anything else* this thread sends to the WebGL thread: a command
//! that waits for a reply (getters, ReadPixels behind toDataURL / toBlob / drawImage / texImage from a WebGL
//! canvas / transferToImageBitmap / captureStream and the clips readback, Finish, fences), Flush, and every
//! other WebGLMsg (SwapBuffers on present and OffscreenCanvas commit, resize, image keys, context removal),
//! because `WebGLChan::send` flushes first. A command for another context flushes too (context switch), so a
//! batch only ever holds one context's commands. It also flushes at `MAX_COMMANDS` commands, at `MAX_BYTES` of
//! payload, when the oldest command is older than the age bound (checked on each push), and when the script
//! thread is about to wait for its next event (`flush_thread`).
//!
//! Payloads are owned by the command when it is batched (uniform arrays are Vecs, texture and buffer bytes are
//! shared memory copied from the page's view at the call), so a page that changes a typed array right after
//! the call can't change what is uploaded. BufferData / BufferSubData send their bytes on their own channel at
//! the call: the WebGL thread reads them when it reaches the command, wherever that is in the batch.
//!
//! In-process only (a crossbeam WebGL channel): in multiprocess mode nothing is batched. `PARTII_WEBGL_BATCH`
//! = the command bound (default 0 = off while the A/B runs), `PARTII_WEBGL_BATCH_US` = the age bound (1000).
//! The wglq queue bound still counts commands, one per batched command, when the batch is sent.

use std::cell::RefCell;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use crate::webgl::{WebGLChan, WebGLCommand, WebGLCommandBacktrace, WebGLContextId, WebGLMsg};

/// Payload bytes after which a batch is sent.
pub const MAX_BYTES: usize = 64 * 1024;

struct Config {
    max_commands: usize,
    max_age: Duration,
}

static CONFIG: LazyLock<Config> = LazyLock::new(|| Config {
    max_commands: std::env::var("PARTII_WEBGL_BATCH")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0),
    max_age: Duration::from_micros(
        std::env::var("PARTII_WEBGL_BATCH_US")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(1000),
    ),
});

/// The command bound (0 = batching off).
#[inline]
pub fn max_commands() -> usize {
    CONFIG.max_commands
}

struct Batch {
    chan: Option<WebGLChan>,
    ctx: Option<WebGLContextId>,
    commands: Vec<(WebGLContextId, WebGLCommand, WebGLCommandBacktrace)>,
    bytes: usize,
    since: Option<Instant>,
}

thread_local! {
    static BATCH: RefCell<Batch> = const { RefCell::new(Batch {
        chan: None, ctx: None, commands: Vec::new(), bytes: 0, since: None,
    }) };
}

/// Whether a command can wait in a batch: it asks for no reply and isn't Flush. A command not listed here
/// (a new one included) is sent on its own, after the batch.
#[inline]
pub fn is_batchable(command: &WebGLCommand) -> bool {
    use WebGLCommand::*;
    matches!(
        command,
        ActiveTexture(..) |
            BlendColor(..) |
            BlendEquation(..) |
            BlendEquationSeparate(..) |
            BlendFunc(..) |
            BlendFuncSeparate(..) |
            AttachShader(..) |
            DetachShader(..) |
            BindAttribLocation(..) |
            BufferData(..) |
            BufferSubData(..) |
            CopyBufferSubData(..) |
            Clear(..) |
            ClearColor(..) |
            ClearDepth(..) |
            ClearStencil(..) |
            ColorMask(..) |
            EnableIndexed(..) |
            DisableIndexed(..) |
            BlendEquationIndexed(..) |
            BlendEquationSeparateIndexed(..) |
            BlendFuncIndexed(..) |
            BlendFuncSeparateIndexed(..) |
            ColorMaskIndexed(..) |
            CullFace(..) |
            FrontFace(..) |
            DepthFunc(..) |
            DepthMask(..) |
            DepthRange(..) |
            Enable(..) |
            Disable(..) |
            CopyTexImage2D { .. } |
            CopyTexSubImage2D(..) |
            DeleteBuffer(..) |
            DeleteFramebuffer(..) |
            DeleteRenderbuffer(..) |
            DeleteTexture(..) |
            DeleteProgram(..) |
            DeleteShader(..) |
            BindBuffer(..) |
            BindFramebuffer(..) |
            BindRenderbuffer(..) |
            BindTexture(..) |
            BlitFrameBuffer(..) |
            DisableVertexAttribArray(..) |
            EnableVertexAttribArray(..) |
            FramebufferRenderbuffer(..) |
            FramebufferTexture2D(..) |
            DeleteTransformFeedback(..) |
            BindTransformFeedback(..) |
            BeginTransformFeedback(..) |
            EndTransformFeedback(..) |
            PauseTransformFeedback(..) |
            ResumeTransformFeedback(..) |
            TransformFeedbackVaryings(..) |
            PolygonOffset(..) |
            RenderbufferStorage(..) |
            RenderbufferStorageMultisample(..) |
            ReadPixelsPP(..) |
            SampleCoverage(..) |
            Scissor(..) |
            StencilFunc(..) |
            StencilFuncSeparate(..) |
            StencilMask(..) |
            StencilMaskSeparate(..) |
            StencilOp(..) |
            StencilOpSeparate(..) |
            WaitSync(..) |
            DeleteSync(..) |
            Hint(..) |
            LineWidth(..) |
            PixelStorei(..) |
            Uniform1f(..) |
            Uniform1fv(..) |
            Uniform1i(..) |
            Uniform1ui(..) |
            Uniform1iv(..) |
            Uniform1uiv(..) |
            Uniform2f(..) |
            Uniform2fv(..) |
            Uniform2i(..) |
            Uniform2ui(..) |
            Uniform2iv(..) |
            Uniform2uiv(..) |
            Uniform3f(..) |
            Uniform3fv(..) |
            Uniform3i(..) |
            Uniform3ui(..) |
            Uniform3iv(..) |
            Uniform3uiv(..) |
            Uniform4f(..) |
            Uniform4fv(..) |
            Uniform4i(..) |
            Uniform4ui(..) |
            Uniform4iv(..) |
            Uniform4uiv(..) |
            UniformMatrix2fv(..) |
            UniformMatrix3fv(..) |
            UniformMatrix4fv(..) |
            UniformMatrix3x2fv(..) |
            UniformMatrix4x2fv(..) |
            UniformMatrix2x3fv(..) |
            UniformMatrix4x3fv(..) |
            UniformMatrix2x4fv(..) |
            UniformMatrix3x4fv(..) |
            UseProgram(..) |
            ValidateProgram(..) |
            VertexAttrib(..) |
            VertexAttribI(..) |
            VertexAttribU(..) |
            VertexAttribPointer(..) |
            VertexAttribIPointer(..) |
            VertexAttribPointer2f(..) |
            SetViewport(..) |
            TexImage3D { .. } |
            TexImage3DPBO { .. } |
            TexSubImage3D { .. } |
            TexSubImage3DPBO { .. } |
            TexSubImage2DPBO { .. } |
            CopyTexSubImage3D(..) |
            TexImage2D { .. } |
            TexImage2DPBO { .. } |
            TexSubImage2D { .. } |
            CompressedTexImage2D { .. } |
            CompressedTexSubImage2D { .. } |
            CompressedTexImage3D { .. } |
            CompressedTexSubImage3D { .. } |
            GenerateMipmap(..) |
            DeleteVertexArray(..) |
            BindVertexArray(..) |
            TexParameteri(..) |
            TexParameterf(..) |
            TexStorage2D(..) |
            TexStorage3D(..) |
            DrawArrays { .. } |
            DrawArraysInstanced { .. } |
            DrawElements { .. } |
            DrawElementsInstanced { .. } |
            DrawArraysInstancedBaseInstance { .. } |
            DrawElementsInstancedBaseVertexBaseInstance { .. } |
            VertexAttribDivisor { .. } |
            UniformBlockBinding(..) |
            InitializeFramebuffer { .. } |
            BindIncompleteTexture(..) |
            BeginQuery(..) |
            DeleteQuery(..) |
            EndQuery(..) |
            DeleteSampler(..) |
            BindSampler(..) |
            SetSamplerParameterFloat(..) |
            SetSamplerParameterInt(..) |
            BindBufferBase(..) |
            BindBufferRange(..) |
            ClearBufferfv(..) |
            ClearBufferiv(..) |
            ClearBufferuiv(..) |
            ClearBufferfi(..) |
            InvalidateFramebuffer(..) |
            InvalidateSubFramebuffer(..) |
            FramebufferTextureLayer(..) |
            ReadBuffer(..) |
            DrawBuffers(..)
    )
}

/// The payload a command carries (bytes), for MAX_BYTES.
fn payload_len(command: &WebGLCommand) -> usize {
    use WebGLCommand::*;
    match command {
        TexImage2D { data, .. } | TexSubImage2D { data, .. } | TexImage3D { data, .. } | TexSubImage3D { data, .. } => {
            data.len()
        },
        CompressedTexImage2D { data, .. } |
        CompressedTexSubImage2D { data, .. } |
        CompressedTexImage3D { data, .. } |
        CompressedTexSubImage3D { data, .. } => data.len(),
        Uniform1fv(_, v) | Uniform2fv(_, v) | Uniform3fv(_, v) | Uniform4fv(_, v) | UniformMatrix2fv(_, v) |
        UniformMatrix3fv(_, v) | UniformMatrix4fv(_, v) | UniformMatrix3x2fv(_, v) | UniformMatrix4x2fv(_, v) |
        UniformMatrix2x3fv(_, v) | UniformMatrix4x3fv(_, v) | UniformMatrix2x4fv(_, v) | UniformMatrix3x4fv(_, v) => {
            v.len() * 4
        },
        Uniform1iv(_, v) | Uniform2iv(_, v) | Uniform3iv(_, v) | Uniform4iv(_, v) => v.len() * 4,
        Uniform1uiv(_, v) | Uniform2uiv(_, v) | Uniform3uiv(_, v) | Uniform4uiv(_, v) => v.len() * 4,
        _ => 0,
    }
}

/// Adds a batchable command for `ctx`, sent over `chan` (flushing first on a context switch, and after it when
/// a bound is reached). The caller has checked `max_commands() > 0`, `is_batchable` and that `chan` is
/// in-process.
pub fn push(chan: &WebGLChan, ctx: WebGLContextId, command: WebGLCommand, backtrace: WebGLCommandBacktrace) {
    let bytes = payload_len(&command);
    BATCH.with(|b| {
        let mut b = b.borrow_mut();
        if b.ctx.is_some_and(|c| c != ctx) {
            send_batch(&mut b);
        }
        if b.commands.is_empty() {
            b.chan = Some(chan.clone());
            b.ctx = Some(ctx);
            b.since = Some(Instant::now());
            if b.commands.capacity() == 0 {
                b.commands.reserve(CONFIG.max_commands.min(4096));
            }
        }
        b.commands.push((ctx, command, backtrace));
        b.bytes += bytes;
        if b.commands.len() >= CONFIG.max_commands ||
            b.bytes >= MAX_BYTES ||
            b.since.is_some_and(|t| t.elapsed() >= CONFIG.max_age)
        {
            send_batch(&mut b);
        }
    });
}

/// Sends this thread's batch now (no-op when empty). Called before anything else goes to the WebGL thread and
/// when the script thread is about to wait.
#[inline]
pub fn flush_thread() {
    if CONFIG.max_commands == 0 {
        return;
    }
    // try_with: a flush from a thread-local destructor at thread exit finds nothing to do.
    let _ = BATCH.try_with(|b| {
        if let Ok(mut b) = b.try_borrow_mut() {
            if !b.commands.is_empty() {
                send_batch(&mut b);
            }
        }
    });
}

fn send_batch(b: &mut Batch) {
    let commands = std::mem::take(&mut b.commands);
    b.bytes = 0;
    b.since = None;
    b.ctx = None;
    let Some(chan) = b.chan.take() else {
        return;
    };
    if commands.is_empty() {
        return;
    }
    let n = commands.len();
    // The wglq bound counts commands: wait for room for this batch before sending it, like a single command.
    let gauge = &crate::partii_webgl_queue::GAUGE;
    gauge.before_send(crate::partii_webgl_queue::GIVE_UP);
    for _ in 0..n {
        gauge.add();
    }
    if chan.send_unflushed(WebGLMsg::WebGLCommandBatch(commands)).is_err() {
        for _ in 0..n {
            gauge.unadd();
        }
    }
    b.commands.reserve(CONFIG.max_commands.min(4096));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_and_flush_are_not_batchable() {
        let (s, _r) = crate::webgl::webgl_channel::<()>().unwrap();
        assert!(!is_batchable(&WebGLCommand::Finish(s)));
        assert!(!is_batchable(&WebGLCommand::Flush));
        assert!(is_batchable(&WebGLCommand::Uniform1f(0, 1.0)));
        assert!(is_batchable(&WebGLCommand::DrawArrays { mode: 4, first: 0, count: 3 }));
    }
}
