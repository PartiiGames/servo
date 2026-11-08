/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use servo_canvas_traits::webgl::WebGLError::InvalidOperation;
use servo_canvas_traits::webgl::{WebGLResult, WebGLVersion};

use super::webgldrawinstancedbasevertexbaseinstance::base_vertex_base_instance_supported;
use super::webglmultidraw::{draw_entries, entries, to_vec};
use super::{WebGLExtension, WebGLExtensionSpec, WebGLExtensions};
use crate::dom::bindings::buffer_source::typed_array_to_vec;
use crate::dom::bindings::codegen::Bindings::WEBGLMultiDrawInstancedBaseVertexBaseInstanceBinding::WEBGLMultiDrawInstancedBaseVertexBaseInstanceMethods;
use crate::dom::bindings::codegen::UnionTypes::{
    Int32ArrayOrLongSequence, Uint32ArrayOrUnsignedLongSequence,
};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::webgl::webgl2renderingcontext::WebGL2RenderingContext;
use crate::dom::webgl::webglrenderingcontext::WebGLRenderingContext;

/// <https://registry.khronos.org/webgl/extensions/WEBGL_multi_draw_instanced_base_vertex_base_instance/>
#[dom_struct]
pub(crate) struct WEBGLMultiDrawInstancedBaseVertexBaseInstance {
    reflector_: Reflector,
    ctx: Dom<WebGLRenderingContext>,
}

impl WEBGLMultiDrawInstancedBaseVertexBaseInstance {
    fn new_inherited(ctx: &WebGLRenderingContext) -> Self {
        Self {
            reflector_: Reflector::new(),
            ctx: Dom::from_ref(ctx),
        }
    }

    /// Runs `draws` against the WebGL 2 context (the extension only exists there) and reports
    /// its error.
    fn draw(
        &self,
        cx: &mut JSContext,
        draws: impl FnOnce(&mut JSContext, &WebGL2RenderingContext) -> WebGLResult<()>,
    ) {
        let result = match self.ctx.webgl2() {
            Some(webgl2) => draws(cx, &webgl2),
            None => Err(InvalidOperation),
        };
        handle_potential_webgl_error!(self.ctx, result);
    }
}

fn to_vec_u32(list: Uint32ArrayOrUnsignedLongSequence) -> Vec<u32> {
    match list {
        Uint32ArrayOrUnsignedLongSequence::Uint32Array(v) => typed_array_to_vec(&*v),
        Uint32ArrayOrUnsignedLongSequence::UnsignedLongSequence(v) => v,
    }
}

impl WebGLExtension for WEBGLMultiDrawInstancedBaseVertexBaseInstance {
    type Extension = Self;
    fn new(cx: &mut JSContext, ctx: &WebGLRenderingContext) -> DomRoot<Self> {
        reflect_dom_object(cx, Box::new(Self::new_inherited(ctx)), &*ctx.global())
    }

    fn spec() -> WebGLExtensionSpec {
        WebGLExtensionSpec::Specific(WebGLVersion::WebGL2)
    }

    fn is_supported(ext: &WebGLExtensions) -> bool {
        base_vertex_base_instance_supported(ext)
    }

    fn enable(_ext: &WebGLExtensions) {}

    fn name() -> &'static str {
        "WEBGL_multi_draw_instanced_base_vertex_base_instance"
    }
}

impl WEBGLMultiDrawInstancedBaseVertexBaseInstanceMethods<crate::DomTypeHolder>
    for WEBGLMultiDrawInstancedBaseVertexBaseInstance
{
    #[allow(clippy::too_many_arguments)]
    fn MultiDrawArraysInstancedBaseInstanceWEBGL(
        &self,
        cx: &mut JSContext,
        mode: u32,
        firsts_list: Int32ArrayOrLongSequence,
        firsts_offset: u64,
        counts_list: Int32ArrayOrLongSequence,
        counts_offset: u64,
        instance_counts_list: Int32ArrayOrLongSequence,
        instance_counts_offset: u64,
        base_instances_list: Uint32ArrayOrUnsignedLongSequence,
        base_instances_offset: u64,
        drawcount: i32,
    ) {
        // Partii: a lost context's extensions do nothing (WebGL 1.0 5.15.2).
        if self.ctx.is_lost() {
            return;
        }
        let (firsts, counts, instances, base_instances) = (
            to_vec(firsts_list),
            to_vec(counts_list),
            to_vec(instance_counts_list),
            to_vec_u32(base_instances_list),
        );
        self.draw(cx, |cx, webgl2| {
            let firsts = entries(&firsts, firsts_offset, drawcount)?;
            let counts = entries(&counts, counts_offset, drawcount)?;
            let instances = entries(&instances, instance_counts_offset, drawcount)?;
            let base_instances = entries(&base_instances, base_instances_offset, drawcount)?;
            draw_entries(&self.ctx, cx, firsts.len(), |cx, i| {
                webgl2.draw_arrays_checked_base_instance(
                    cx,
                    mode,
                    firsts[i],
                    counts[i],
                    instances[i],
                    base_instances[i],
                )
            })
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn MultiDrawElementsInstancedBaseVertexBaseInstanceWEBGL(
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
        base_vertices_list: Int32ArrayOrLongSequence,
        base_vertices_offset: u64,
        base_instances_list: Uint32ArrayOrUnsignedLongSequence,
        base_instances_offset: u64,
        drawcount: i32,
    ) {
        // Partii: a lost context's extensions do nothing (WebGL 1.0 5.15.2).
        if self.ctx.is_lost() {
            return;
        }
        let (counts, offsets, instances, base_vertices, base_instances) = (
            to_vec(counts_list),
            to_vec(offsets_list),
            to_vec(instance_counts_list),
            to_vec(base_vertices_list),
            to_vec_u32(base_instances_list),
        );
        self.draw(cx, |cx, webgl2| {
            let counts = entries(&counts, counts_offset, drawcount)?;
            let offsets = entries(&offsets, offsets_offset, drawcount)?;
            let instances = entries(&instances, instance_counts_offset, drawcount)?;
            let base_vertices = entries(&base_vertices, base_vertices_offset, drawcount)?;
            let base_instances = entries(&base_instances, base_instances_offset, drawcount)?;
            draw_entries(&self.ctx, cx, counts.len(), |cx, i| {
                webgl2.draw_elements_checked_base_vertex_base_instance(
                    cx,
                    mode,
                    counts[i],
                    type_,
                    offsets[i] as i64,
                    instances[i],
                    base_vertices[i],
                    base_instances[i],
                )
            })
        })
    }
}
