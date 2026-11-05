/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use servo_canvas_traits::webgl::WebGLError::{InvalidOperation, InvalidValue};
use servo_canvas_traits::webgl::{WebGLCommand, WebGLResult};

use super::{WebGLExtension, WebGLExtensionSpec, WebGLExtensions};
use crate::dom::bindings::buffer_source::typed_array_to_vec;
use crate::dom::bindings::codegen::Bindings::ANGLEInstancedArraysBinding::ANGLEInstancedArraysConstants;
use crate::dom::bindings::codegen::Bindings::WEBGLMultiDrawBinding::WEBGLMultiDrawMethods;
use crate::dom::bindings::codegen::UnionTypes::Int32ArrayOrLongSequence;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::webgl::webglrenderingcontext::WebGLRenderingContext;

/// WEBGL_multi_draw: each list entry is drawn as its own drawArrays/drawElements(Instanced)
/// (with every WebGL 2 draw check on WebGL 2 contexts), stopping at the first draw that fails.
/// On WebGL 1 it implies ANGLE_instanced_arrays.
#[dom_struct]
pub(crate) struct WEBGLMultiDraw {
    reflector_: Reflector,
    ctx: Dom<WebGLRenderingContext>,
}

impl WEBGLMultiDraw {
    fn new_inherited(ctx: &WebGLRenderingContext) -> Self {
        Self {
            reflector_: Reflector::new(),
            ctx: Dom::from_ref(ctx),
        }
    }
}

impl WebGLExtension for WEBGLMultiDraw {
    type Extension = Self;
    fn new(cx: &mut JSContext, ctx: &WebGLRenderingContext) -> DomRoot<Self> {
        reflect_dom_object(
            cx,
            Box::new(WEBGLMultiDraw::new_inherited(ctx)),
            &*ctx.global(),
        )
    }

    fn spec() -> WebGLExtensionSpec {
        WebGLExtensionSpec::All
    }

    // Emulated with one draw per entry, so any WebGL 2 context has it.
    fn is_supported(_ext: &WebGLExtensions) -> bool {
        true
    }

    fn enable(ext: &WebGLExtensions) {
        ext.enable_get_vertex_attrib_name(
            ANGLEInstancedArraysConstants::VERTEX_ATTRIB_ARRAY_DIVISOR_ANGLE,
        );
    }

    fn name() -> &'static str {
        "WEBGL_multi_draw"
    }
}

pub(crate) fn to_vec(list: Int32ArrayOrLongSequence) -> Vec<i32> {
    match list {
        Int32ArrayOrLongSequence::Int32Array(v) => typed_array_to_vec(&*v),
        Int32ArrayOrLongSequence::LongSequence(v) => v,
    }
}

/// The `drawcount` entries of `list` from `offset`: INVALID_VALUE for a negative drawcount,
/// INVALID_OPERATION when the list is too short (WEBGL_multi_draw).
/// The `drawcount` entries of `list` from `offset`: INVALID_VALUE for a negative drawcount,
/// INVALID_OPERATION when the list is too short.
pub(crate) fn entries<T>(list: &[T], offset: u64, drawcount: i32) -> WebGLResult<&[T]> {
    let count = usize::try_from(drawcount).map_err(|_| InvalidValue)?;
    let start = usize::try_from(offset).map_err(|_| InvalidOperation)?;
    let end = start.checked_add(count).ok_or(InvalidOperation)?;
    list.get(start..end).ok_or(InvalidOperation)
}

/// Draws entries `0..drawcount` in order, stopping at the first error. A vertex shader's gl_DrawID
/// is the uniform `angle_DrawID` (the translator's emulateGLDrawID): it is set to each entry's
/// index and back to 0 afterwards, so plain draws see 0.
pub(crate) fn draw_entries(
    ctx: &WebGLRenderingContext,
    cx: &mut JSContext,
    drawcount: usize,
    mut draw: impl FnMut(&mut JSContext, usize) -> WebGLResult<()>,
) -> WebGLResult<()> {
    let draw_id = ctx
        .current_program()
        .and_then(|program| program.emulated_builtins().draw_id);
    let mut result = Ok(());
    for index in 0..drawcount {
        if let Some(location) = draw_id &&
            index > 0
        {
            ctx.send_command(WebGLCommand::Uniform1i(location, index as i32));
        }
        result = draw(cx, index);
        if result.is_err() {
            break;
        }
    }
    if let Some(location) = draw_id &&
        drawcount > 1
    {
        ctx.send_command(WebGLCommand::Uniform1i(location, 0));
    }
    result
}

impl WEBGLMultiDraw {
    fn draw_arrays(
        &self,
        cx: &mut JSContext,
        mode: u32,
        first: i32,
        count: i32,
        instances: i32,
    ) -> WebGLResult<()> {
        match self.ctx.webgl2() {
            Some(webgl2) => webgl2.draw_arrays_checked(cx, mode, first, count, instances),
            None => self
                .ctx
                .draw_arrays_instanced(cx, mode, first, count, instances),
        }
    }

    fn draw_elements(
        &self,
        cx: &mut JSContext,
        mode: u32,
        count: i32,
        type_: u32,
        offset: i32,
        instances: i32,
    ) -> WebGLResult<()> {
        let offset = offset as i64;
        match self.ctx.webgl2() {
            Some(webgl2) => webgl2.draw_elements_checked(cx, mode, count, type_, offset, instances),
            None => self
                .ctx
                .draw_elements_instanced(cx, mode, count, type_, offset, instances),
        }
    }

    fn draw(&self, cx: &mut JSContext, draws: impl FnOnce(&mut JSContext) -> WebGLResult<()>) {
        handle_potential_webgl_error!(self.ctx, draws(cx));
    }
}

impl WEBGLMultiDrawMethods<crate::DomTypeHolder> for WEBGLMultiDraw {
    /// <https://registry.khronos.org/webgl/extensions/WEBGL_multi_draw/>
    fn MultiDrawArraysWEBGL(
        &self,
        cx: &mut JSContext,
        mode: u32,
        firsts_list: Int32ArrayOrLongSequence,
        firsts_offset: u64,
        counts_list: Int32ArrayOrLongSequence,
        counts_offset: u64,
        drawcount: i32,
    ) {
        let (firsts, counts) = (to_vec(firsts_list), to_vec(counts_list));
        self.draw(cx, |cx| {
            let firsts = entries(&firsts, firsts_offset, drawcount)?;
            let counts = entries(&counts, counts_offset, drawcount)?;
            draw_entries(&self.ctx, cx, firsts.len(), |cx, i| {
                self.draw_arrays(cx, mode, firsts[i], counts[i], 1)
            })
        })
    }

    /// <https://registry.khronos.org/webgl/extensions/WEBGL_multi_draw/>
    fn MultiDrawElementsWEBGL(
        &self,
        cx: &mut JSContext,
        mode: u32,
        counts_list: Int32ArrayOrLongSequence,
        counts_offset: u64,
        type_: u32,
        offsets_list: Int32ArrayOrLongSequence,
        offsets_offset: u64,
        drawcount: i32,
    ) {
        let (counts, offsets) = (to_vec(counts_list), to_vec(offsets_list));
        self.draw(cx, |cx| {
            let counts = entries(&counts, counts_offset, drawcount)?;
            let offsets = entries(&offsets, offsets_offset, drawcount)?;
            draw_entries(&self.ctx, cx, counts.len(), |cx, i| {
                self.draw_elements(cx, mode, counts[i], type_, offsets[i], 1)
            })
        })
    }

    /// <https://registry.khronos.org/webgl/extensions/WEBGL_multi_draw/>
    fn MultiDrawArraysInstancedWEBGL(
        &self,
        cx: &mut JSContext,
        mode: u32,
        firsts_list: Int32ArrayOrLongSequence,
        firsts_offset: u64,
        counts_list: Int32ArrayOrLongSequence,
        counts_offset: u64,
        instance_counts_list: Int32ArrayOrLongSequence,
        instance_counts_offset: u64,
        drawcount: i32,
    ) {
        let (firsts, counts, instances) = (
            to_vec(firsts_list),
            to_vec(counts_list),
            to_vec(instance_counts_list),
        );
        self.draw(cx, |cx| {
            let firsts = entries(&firsts, firsts_offset, drawcount)?;
            let counts = entries(&counts, counts_offset, drawcount)?;
            let instances = entries(&instances, instance_counts_offset, drawcount)?;
            draw_entries(&self.ctx, cx, firsts.len(), |cx, i| {
                self.draw_arrays(cx, mode, firsts[i], counts[i], instances[i])
            })
        })
    }

    /// <https://registry.khronos.org/webgl/extensions/WEBGL_multi_draw/>
    fn MultiDrawElementsInstancedWEBGL(
        &self,
        cx: &mut JSContext,
        mode: u32,
        counts_list: Int32ArrayOrLongSequence,
        counts_offset: u64,
        type_: u32,
        offsets_list: Int32ArrayOrLongSequence,
        offsets_offset: u64,
        instance_counts_list: Int32ArrayOrLongSequence,
        instance_counts_offset: u64,
        drawcount: i32,
    ) {
        let (counts, offsets, instances) = (
            to_vec(counts_list),
            to_vec(offsets_list),
            to_vec(instance_counts_list),
        );
        self.draw(cx, |cx| {
            let counts = entries(&counts, counts_offset, drawcount)?;
            let offsets = entries(&offsets, offsets_offset, drawcount)?;
            let instances = entries(&instances, instance_counts_offset, drawcount)?;
            draw_entries(&self.ctx, cx, counts.len(), |cx, i| {
                self.draw_elements(cx, mode, counts[i], type_, offsets[i], instances[i])
            })
        })
    }
}
