/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name};
use js::context::JSContext;
use js::rust::HandleObject;
use stylo_dom::ElementState;

use crate::dom::bindings::codegen::Bindings::SVGFEGaussianBlurElementBinding::SVGFEGaussianBlurElementMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::document::Document;
use crate::dom::node::Node;
use crate::dom::svg::svganimatedenumeration::{SVGAnimatedEnumeration, SVGEnumeration};
use crate::dom::svg::svganimatednumber::SVGAnimatedNumber;
use crate::dom::svg::svganimatedstring::SVGAnimatedString;
use crate::dom::svg::svgelement::SVGElement;
use crate::dom::svg::svgfilterprimitivestandardattributes::{
    SVGFilterPrimitiveStandardAttributes, filter_primitive_standard_attributes,
};
use crate::dom::svg::svgreflectedattribute::{AttributePart, set_attribute_value};

/// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGFEGaussianBlurElement>
#[dom_struct]
pub(crate) struct SVGFEGaussianBlurElement {
    svgelement: SVGElement,
    standard_attributes: SVGFilterPrimitiveStandardAttributes,
    in1: MutNullableDom<SVGAnimatedString>,
    std_deviation_x: MutNullableDom<SVGAnimatedNumber>,
    std_deviation_y: MutNullableDom<SVGAnimatedNumber>,
    edge_mode: MutNullableDom<SVGAnimatedEnumeration>,
}

impl SVGFEGaussianBlurElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGFEGaussianBlurElement {
        SVGFEGaussianBlurElement {
            svgelement: SVGElement::new_inherited_with_state(
                ElementState::empty(),
                local_name,
                prefix,
                document,
            ),
            standard_attributes: Default::default(),
            in1: Default::default(),
            std_deviation_x: Default::default(),
            std_deviation_y: Default::default(),
            edge_mode: Default::default(),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGFEGaussianBlurElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGFEGaussianBlurElement::new_inherited(
                local_name, prefix, document,
            )),
            document,
            proto,
        )
    }
}

impl SVGFEGaussianBlurElementMethods<crate::DomTypeHolder> for SVGFEGaussianBlurElement {
    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfegaussianblurelement-in1>
    fn In1(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedString> {
        self.in1
            .or_init(|| SVGAnimatedString::new(cx, self.upcast(), local_name!("in")))
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfegaussianblurelement-stddeviationx>
    fn StdDeviationX(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.std_deviation_x.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("stdDeviation"),
                AttributePart::First,
                0.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfegaussianblurelement-stddeviationy>
    fn StdDeviationY(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.std_deviation_y.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("stdDeviation"),
                AttributePart::Second,
                0.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfegaussianblurelement-edgemode>
    fn EdgeMode(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedEnumeration> {
        self.edge_mode.or_init(|| {
            SVGAnimatedEnumeration::new(
                cx,
                self.upcast(),
                local_name!("edgeMode"),
                SVGEnumeration::EdgeMode,
                3,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfegaussianblurelement-setstddeviation>
    fn SetStdDeviation(
        &self,
        cx: &mut JSContext,
        std_deviation_x: Finite<f32>,
        std_deviation_y: Finite<f32>,
    ) {
        set_attribute_value(
            cx,
            self.upcast(),
            &local_name!("stdDeviation"),
            format!("{} {}", *std_deviation_x, *std_deviation_y),
        );
    }

    filter_primitive_standard_attributes!();
}
