/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii (stream webgl-compile): shader compiles and program links the WebGL thread makes later than asked.
//!
//! Every Phaser boot (each new round in hexii, each death in Rocket to Heaven: a new context each time) compiles
//! 62 shaders and links 31 programs in one frame: ~21-25 ms of the WebGL thread's time on the Apple M5 (GL 2.1),
//! criterion 2(a)'s `webgl_compile` pauses. Script already doesn't wait for them after the first boot (its compile
//! and link caches, KHR_parallel_shader_compile semantics), but the WebGL thread still made every glCompileShader and
//! glLinkProgram right when it got them. Most of the 31 programs (post-FX, lights) are never drawn with.
//!
//! Now the GL compile of a shader waits until a link needs it, and a link script won't read (`LinkProgram`'s bool)
//! waits until the program is needed: glUseProgram of it, any command that asks GL about it, a link script waits for
//! that uses its shaders, or the thread's idle time between frames (a few ms a frame, see `WebGLThread::drain`).
//!
//! The ops wait in a queue in the order they came. An op that touches (names) an object with ops waiting joins the
//! queue behind them; others run at once. Making an object "needed" runs every queued op in the closure of the
//! objects it touches (ops sharing an object with one already in it, to a fixpoint), in queue order: ops outside it
//! share no object with ops inside it, so running those first can't change what either does. A link touches the
//! program and the shaders attached to it then (`attached`), so it pulls in their compiles. Shader and program
//! names come from one GL namespace. A deletion that waits keeps the GL name alive, so GL can't hand it out again
//! while ops still name it. `PARTII_WEBGL_DEFER_PROGRAMS=0` turns this off.

use std::collections::VecDeque;

use rustc_hash::FxHashMap;
use servo_canvas_traits::webgl::{WebGLProgramId, WebGLShaderId};

/// `PARTII_WEBGL_DEFER_PROGRAMS=0` turns deferred compiles and links off (A/B switch).
pub(crate) fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PARTII_WEBGL_DEFER_PROGRAMS").map_or(true, |v| v != "0"))
}

/// One GL call on a shader or program object.
#[derive(Debug, PartialEq)]
pub(crate) enum ProgramOp {
    /// glShaderSource + glCompileShader of the translated source.
    Compile(WebGLShaderId, String),
    Attach(WebGLProgramId, WebGLShaderId),
    Detach(WebGLProgramId, WebGLShaderId),
    BindAttribLocation(WebGLProgramId, u32, String),
    /// glLinkProgram alone: nobody reads the answer of a link that waited.
    Link(WebGLProgramId),
    DeleteShader(WebGLShaderId),
    DeleteProgram(WebGLProgramId),
}

#[derive(Default)]
pub(crate) struct DeferredPrograms {
    /// The ops waiting, oldest first, each with the object names it touches.
    ops: VecDeque<(Vec<u32>, ProgramOp)>,
    /// The shaders attached to each program, as of the last op given (waiting or not).
    attached: FxHashMap<u32, Vec<u32>>,
    /// The program glUseProgram last made current (0: none); a link of it can't wait.
    current_program: u32,
}

impl std::fmt::Debug for DeferredPrograms {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DeferredPrograms({} waiting)", self.ops.len())
    }
}

impl DeferredPrograms {
    pub(crate) fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.ops.len()
    }

    fn attached_to(&self, program: u32) -> impl Iterator<Item = u32> + '_ {
        self.attached.get(&program).into_iter().flatten().copied()
    }

    fn waiting(&self, names: &[u32]) -> bool {
        self.ops
            .iter()
            .any(|(touches, _)| touches.iter().any(|n| names.contains(n)))
    }

    /// An op content asked for. `may_wait`: nobody waits for its outcome (always true of a GL compile: script
    /// learns compile status from the translator). A link script waits for runs now, after what it needs.
    pub(crate) fn submit(
        &mut self,
        op: ProgramOp,
        may_wait: bool,
        run: &mut impl FnMut(ProgramOp),
    ) {
        let touches: Vec<u32> = match op {
            ProgramOp::Compile(shader, _) | ProgramOp::DeleteShader(shader) => vec![shader.get()],
            ProgramOp::Attach(program, shader) => {
                let list = self.attached.entry(program.get()).or_default();
                if !list.contains(&shader.get()) {
                    list.push(shader.get());
                }
                vec![program.get(), shader.get()]
            },
            ProgramOp::Detach(program, shader) => {
                if let Some(list) = self.attached.get_mut(&program.get()) {
                    list.retain(|s| *s != shader.get());
                }
                vec![program.get(), shader.get()]
            },
            ProgramOp::BindAttribLocation(program, ..) => vec![program.get()],
            ProgramOp::Link(program) => std::iter::once(program.get())
                .chain(self.attached_to(program.get()))
                .collect(),
            ProgramOp::DeleteProgram(program) => {
                let touches = std::iter::once(program.get())
                    .chain(self.attached_to(program.get()))
                    .collect();
                self.attached.remove(&program.get());
                touches
            },
        };
        let link_now = match op {
            // Relinking the program in use changes what the next draw runs.
            ProgramOp::Link(program) => !may_wait || program.get() == self.current_program,
            _ => false,
        };
        if link_now {
            self.need(&touches, run);
            run(op);
        } else if may_wait || self.waiting(&touches) {
            self.ops.push_back((touches, op));
        } else {
            run(op);
        }
    }

    /// Before a command that uses or asks GL about these objects: runs every op they depend on.
    pub(crate) fn need(&mut self, names: &[u32], run: &mut impl FnMut(ProgramOp)) {
        if self.ops.is_empty() || !self.waiting(names) {
            return;
        }
        let mut closure: Vec<u32> = names.to_vec();
        loop {
            let before = closure.len();
            for (touches, _) in &self.ops {
                if touches.iter().any(|n| closure.contains(n)) {
                    for n in touches {
                        if !closure.contains(n) {
                            closure.push(*n);
                        }
                    }
                }
            }
            if closure.len() == before {
                break;
            }
        }
        for (touches, op) in std::mem::take(&mut self.ops) {
            if touches.iter().any(|n| closure.contains(n)) {
                run(op);
            } else {
                self.ops.push_back((touches, op));
            }
        }
    }

    /// Before a link that runs now (script waits for its answer, so it isn't given to `submit`): what it needs.
    pub(crate) fn link_now(&mut self, program: WebGLProgramId, run: &mut impl FnMut(ProgramOp)) {
        let touches: Vec<u32> = std::iter::once(program.get())
            .chain(self.attached_to(program.get()))
            .collect();
        self.need(&touches, run);
    }

    /// glUseProgram: the program's ops run first.
    pub(crate) fn use_program(
        &mut self,
        program: Option<WebGLProgramId>,
        run: &mut impl FnMut(ProgramOp),
    ) {
        let name = program.map_or(0, |p| p.get());
        if name != 0 {
            self.need(&[name], run);
        }
        self.current_program = name;
    }

    /// Runs the oldest waiting op (always safe: nothing waits before it). False when none was waiting.
    pub(crate) fn run_oldest(&mut self, run: &mut impl FnMut(ProgramOp)) -> bool {
        match self.ops.pop_front() {
            Some((_, op)) => {
                run(op);
                true
            },
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(n: u32) -> WebGLProgramId {
        WebGLProgramId::maybe_new(n).unwrap()
    }
    fn s(n: u32) -> WebGLShaderId {
        WebGLShaderId::maybe_new(n).unwrap()
    }

    /// Phaser's way per program: compile both, attach both, link (answered), delete both shaders.
    fn boot_program(
        d: &mut DeferredPrograms,
        log: &mut Vec<ProgramOp>,
        prog: u32,
        vs: u32,
        fs: u32,
        answered: bool,
    ) {
        let mut run = |op| log.push(op);
        d.submit(ProgramOp::Compile(s(vs), format!("vs{vs}")), true, &mut run);
        d.submit(ProgramOp::Compile(s(fs), format!("fs{fs}")), true, &mut run);
        d.submit(ProgramOp::Attach(p(prog), s(vs)), false, &mut run);
        d.submit(ProgramOp::Attach(p(prog), s(fs)), false, &mut run);
        d.submit(ProgramOp::Link(p(prog)), answered, &mut run);
        d.submit(ProgramOp::DeleteShader(s(vs)), false, &mut run);
        d.submit(ProgramOp::DeleteShader(s(fs)), false, &mut run);
    }

    fn program_ops(prog: u32, vs: u32, fs: u32) -> Vec<ProgramOp> {
        vec![
            ProgramOp::Compile(s(vs), format!("vs{vs}")),
            ProgramOp::Compile(s(fs), format!("fs{fs}")),
            ProgramOp::Attach(p(prog), s(vs)),
            ProgramOp::Attach(p(prog), s(fs)),
            ProgramOp::Link(p(prog)),
            ProgramOp::DeleteShader(s(vs)),
            ProgramOp::DeleteShader(s(fs)),
        ]
    }

    /// Every WebGLCommand that names a shader or program is one defer_program_work hands to DeferredPrograms, or
    /// one program_or_shader_named lists (so what it names is made first), or makes the object.
    #[test]
    fn every_command_naming_a_shader_or_program_is_covered() {
        let canvas = include_str!("../shared/canvas/webgl.rs");
        let thread = include_str!("webgl_thread.rs");
        let start = canvas.find("pub enum WebGLCommand {").unwrap();
        let body = &canvas[start..start + canvas[start..].find("\n}\n").unwrap()];
        let named_fn = &thread[thread.find("fn program_or_shader_named").unwrap()..];
        let named_fn = &named_fn[..named_fn.find("\n}\n").unwrap()];
        let defer_fn = &thread[thread.find("fn defer_program_work").unwrap()..];
        let defer_fn = &defer_fn[..defer_fn.find("\n    }\n").unwrap()];
        // Variants start at four spaces of indent; a variant's text runs to the next one.
        let mut variants: Vec<(String, String)> = vec![];
        for line in body.lines().skip(1) {
            let trimmed = line.trim_start();
            let indent = line.len() - trimmed.len();
            if indent == 4 && trimmed.starts_with(|c: char| c.is_ascii_uppercase()) {
                let name: String = trimmed
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect();
                variants.push((name, String::new()));
            }
            if let Some(last) = variants.last_mut() {
                last.1.push_str(line);
            }
        }
        let mut missing = vec![];
        for (name, text) in &variants {
            if !(text.contains("WebGLProgramId") || text.contains("WebGLShaderId")) {
                continue;
            }
            if name.starts_with("Create") {
                continue;
            }
            let pattern = format!("{name}(");
            if !named_fn.contains(&pattern)
                && !defer_fn.contains(&format!("WebGLCommand::{pattern}"))
            {
                missing.push(name.clone());
            }
        }
        assert!(variants.len() > 100, "parsed {} variants", variants.len());
        assert!(missing.is_empty(), "not covered: {missing:?}");
    }

    #[test]
    fn answered_boot_waits_and_use_runs_only_that_program() {
        let mut d = DeferredPrograms::default();
        let mut log = vec![];
        boot_program(&mut d, &mut log, 1, 2, 3, true);
        boot_program(&mut d, &mut log, 4, 5, 6, true);
        boot_program(&mut d, &mut log, 7, 8, 9, true);
        assert!(log.is_empty());
        assert_eq!(d.len(), 21);
        d.use_program(Some(p(4)), &mut |op| log.push(op));
        assert_eq!(log, program_ops(4, 5, 6));
        assert_eq!(d.len(), 14);
        // Draws with program 4 need nothing more; the rest drains oldest first.
        log.clear();
        while d.run_oldest(&mut |op| log.push(op)) {}
        let mut expect = program_ops(1, 2, 3);
        expect.extend(program_ops(7, 8, 9));
        assert_eq!(log, expect);
        assert!(d.is_empty());
    }

    #[test]
    fn unanswered_link_runs_now_with_its_compiles() {
        let mut d = DeferredPrograms::default();
        let mut log = vec![];
        boot_program(&mut d, &mut log, 1, 2, 3, true);
        boot_program(&mut d, &mut log, 4, 5, 6, false);
        // Program 4's compiles and attaches ran before its link; its shader deletes ran at once after.
        assert_eq!(log, program_ops(4, 5, 6));
        assert_eq!(d.len(), 7);
    }

    #[test]
    fn compile_after_direct_attach_is_pulled_in_by_the_link() {
        let mut d = DeferredPrograms::default();
        let mut log = vec![];
        let mut run = |op| log.push(op);
        // Attached before any compile: nothing waits, it runs at once.
        d.submit(ProgramOp::Attach(p(1), s(2)), false, &mut run);
        d.submit(ProgramOp::Compile(s(2), "a".into()), true, &mut run);
        d.submit(ProgramOp::Link(p(1)), false, &mut run);
        assert_eq!(
            log,
            vec![
                ProgramOp::Attach(p(1), s(2)),
                ProgramOp::Compile(s(2), "a".into()),
                ProgramOp::Link(p(1)),
            ]
        );
        assert!(d.is_empty());
    }

    #[test]
    fn relinking_the_current_program_runs_now() {
        let mut d = DeferredPrograms::default();
        let mut log = vec![];
        boot_program(&mut d, &mut log, 1, 2, 3, true);
        d.use_program(Some(p(1)), &mut |op| log.push(op));
        log.clear();
        let mut run = |op| log.push(op);
        d.submit(ProgramOp::Compile(s(10), "b".into()), true, &mut run);
        d.submit(ProgramOp::Attach(p(1), s(10)), false, &mut run);
        d.submit(ProgramOp::Link(p(1)), true, &mut run);
        assert_eq!(
            log,
            vec![
                ProgramOp::Compile(s(10), "b".into()),
                ProgramOp::Attach(p(1), s(10)),
                ProgramOp::Link(p(1)),
            ]
        );
    }

    #[test]
    fn a_shader_shared_by_two_programs_pulls_both_in_order() {
        let mut d = DeferredPrograms::default();
        let mut log = vec![];
        let mut run = |op| log.push(op);
        d.submit(ProgramOp::Compile(s(2), "v".into()), true, &mut run);
        d.submit(ProgramOp::Compile(s(3), "f".into()), true, &mut run);
        d.submit(ProgramOp::Compile(s(4), "g".into()), true, &mut run);
        d.submit(ProgramOp::Attach(p(1), s(2)), false, &mut run);
        d.submit(ProgramOp::Attach(p(1), s(3)), false, &mut run);
        d.submit(ProgramOp::Link(p(1)), true, &mut run);
        d.submit(ProgramOp::Attach(p(5), s(2)), false, &mut run);
        d.submit(ProgramOp::Attach(p(5), s(4)), false, &mut run);
        d.submit(ProgramOp::Link(p(5)), true, &mut run);
        // An unrelated program stays queued.
        d.submit(ProgramOp::Compile(s(9), "x".into()), true, &mut run);
        d.need(&[5], &mut run);
        assert_eq!(log.len(), 9);
        assert_eq!(log[5], ProgramOp::Link(p(1)));
        assert_eq!(log[8], ProgramOp::Link(p(5)));
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn delete_program_waits_behind_its_link_and_detach_updates_attachments() {
        let mut d = DeferredPrograms::default();
        let ran = std::cell::Cell::new(0);
        let mut run = |_| ran.set(ran.get() + 1);
        d.submit(ProgramOp::Attach(p(1), s(2)), false, &mut run);
        d.submit(ProgramOp::Detach(p(1), s(2)), false, &mut run);
        d.submit(ProgramOp::Compile(s(2), "a".into()), true, &mut run);
        // Detached: this link no longer touches shader 2, so it doesn't wait behind its compile.
        d.submit(ProgramOp::Link(p(1)), false, &mut run);
        assert_eq!(ran.get(), 3);
        assert_eq!(d.len(), 1);
        d.submit(ProgramOp::Link(p(1)), true, &mut run);
        d.submit(ProgramOp::DeleteProgram(p(1)), false, &mut run);
        assert_eq!(d.len(), 3);
        assert!(d.attached.get(&1).is_none());
    }
}
