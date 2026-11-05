/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use servo_canvas_traits::webgl::WebGLError::{InvalidOperation, InvalidValue};
use servo_canvas_traits::webgl::WebGLResult;

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

fn to_vec(list: Int32ArrayOrLongSequence) -> Vec<i32> {
    match list {
        Int32ArrayOrLongSequence::Int32Array(v) => typed_array_to_vec(&*v),
        Int32ArrayOrLongSequence::LongSequence(v) => v,
    }
}

/// The `drawcount` entries of `list` from `offset`: INVALID_VALUE for a negative drawcount,
/// INVALID_OPERATION when the list is too short (WEBGL_multi_draw).
fn entries(list: &[i32], offset: u64, drawcount: i32) -> WebGLResult<&[i32]> {
    let count = usize::try_from(drawcount).map_err(|_| InvalidValue)?;
    let start = usize::try_from(offset).map_err(|_| InvalidOperation)?;
    let end = start.checked_add(count).ok_or(InvalidOperation)?;
    list.get(start..end).ok_or(InvalidOperation)
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
            for (&first, &count) in firsts.iter().zip(counts) {
                self.draw_arrays(cx, mode, first, count, 1)?;
            }
            Ok(())
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
            for (&count, &offset) in counts.iter().zip(offsets) {
                self.draw_elements(cx, mode, count, type_, offset, 1)?;
            }
            Ok(())
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
            for ((&first, &count), &instances) in firsts.iter().zip(counts).zip(instances) {
                self.draw_arrays(cx, mode, first, count, instances)?;
            }
            Ok(())
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
            for ((&count, &offset), &instances) in counts.iter().zip(offsets).zip(instances) {
                self.draw_elements(cx, mode, count, type_, offset, instances)?;
            }
            Ok(())
        })
    }
}
