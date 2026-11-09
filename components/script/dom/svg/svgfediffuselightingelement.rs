/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name};
use js::context::JSContext;
use js::rust::HandleObject;
use stylo_dom::ElementState;

use crate::dom::bindings::codegen::Bindings::SVGFEDiffuseLightingElementBinding::SVGFEDiffuseLightingElementMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::document::Document;
use crate::dom::node::Node;
use crate::dom::svg::svganimatednumber::SVGAnimatedNumber;
use crate::dom::svg::svganimatedstring::SVGAnimatedString;
use crate::dom::svg::svgelement::SVGElement;
use crate::dom::svg::svgfilterprimitivestandardattributes::{
    SVGFilterPrimitiveStandardAttributes, filter_primitive_standard_attributes,
};
use crate::dom::svg::svgreflectedattribute::AttributePart;

/// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGFEDiffuseLightingElement>
#[dom_struct]
pub(crate) struct SVGFEDiffuseLightingElement {
    svgelement: SVGElement,
    standard_attributes: SVGFilterPrimitiveStandardAttributes,
    in1: MutNullableDom<SVGAnimatedString>,
    surface_scale: MutNullableDom<SVGAnimatedNumber>,
    diffuse_constant: MutNullableDom<SVGAnimatedNumber>,
    kernel_unit_length_x: MutNullableDom<SVGAnimatedNumber>,
    kernel_unit_length_y: MutNullableDom<SVGAnimatedNumber>,
}

impl SVGFEDiffuseLightingElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGFEDiffuseLightingElement {
        SVGFEDiffuseLightingElement {
            svgelement: SVGElement::new_inherited_with_state(
                ElementState::empty(),
                local_name,
                prefix,
                document,
            ),
            standard_attributes: Default::default(),
            in1: Default::default(),
            surface_scale: Default::default(),
            diffuse_constant: Default::default(),
            kernel_unit_length_x: Default::default(),
            kernel_unit_length_y: Default::default(),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGFEDiffuseLightingElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGFEDiffuseLightingElement::new_inherited(
                local_name, prefix, document,
            )),
            document,
            proto,
        )
    }
}

impl SVGFEDiffuseLightingElementMethods<crate::DomTypeHolder> for SVGFEDiffuseLightingElement {
    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfediffuselightingelement-in1>
    fn In1(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedString> {
        self.in1
            .or_init(|| SVGAnimatedString::new(cx, self.upcast(), local_name!("in")))
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfediffuselightingelement-surfacescale>
    fn SurfaceScale(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.surface_scale.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("surfaceScale"),
                AttributePart::Whole,
                1.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfediffuselightingelement-diffuseconstant>
    fn DiffuseConstant(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.diffuse_constant.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("diffuseConstant"),
                AttributePart::Whole,
                1.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfediffuselightingelement-kernelunitlengthx>
    fn KernelUnitLengthX(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.kernel_unit_length_x.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("kernelUnitLength"),
                AttributePart::First,
                0.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfediffuselightingelement-kernelunitlengthy>
    fn KernelUnitLengthY(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.kernel_unit_length_y.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("kernelUnitLength"),
                AttributePart::Second,
                0.,
            )
        })
    }

    filter_primitive_standard_attributes!();
}
