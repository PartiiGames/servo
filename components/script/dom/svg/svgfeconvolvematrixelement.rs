/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name};
use js::context::JSContext;
use js::rust::HandleObject;
use stylo_dom::ElementState;

use crate::dom::bindings::codegen::Bindings::SVGFEConvolveMatrixElementBinding::SVGFEConvolveMatrixElementMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::document::Document;
use crate::dom::node::Node;
use crate::dom::svg::svganimatedboolean::SVGAnimatedBoolean;
use crate::dom::svg::svganimatedenumeration::{SVGAnimatedEnumeration, SVGEnumeration};
use crate::dom::svg::svganimatedinteger::SVGAnimatedInteger;
use crate::dom::svg::svganimatednumber::SVGAnimatedNumber;
use crate::dom::svg::svganimatedstring::SVGAnimatedString;
use crate::dom::svg::svgelement::SVGElement;
use crate::dom::svg::svgfilterprimitivestandardattributes::{
    SVGFilterPrimitiveStandardAttributes, filter_primitive_standard_attributes,
};
use crate::dom::svg::svgreflectedattribute::AttributePart;

/// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGFEConvolveMatrixElement>
#[dom_struct]
pub(crate) struct SVGFEConvolveMatrixElement {
    svgelement: SVGElement,
    standard_attributes: SVGFilterPrimitiveStandardAttributes,
    in1: MutNullableDom<SVGAnimatedString>,
    order_x: MutNullableDom<SVGAnimatedInteger>,
    order_y: MutNullableDom<SVGAnimatedInteger>,
    divisor: MutNullableDom<SVGAnimatedNumber>,
    bias: MutNullableDom<SVGAnimatedNumber>,
    target_x: MutNullableDom<SVGAnimatedInteger>,
    target_y: MutNullableDom<SVGAnimatedInteger>,
    edge_mode: MutNullableDom<SVGAnimatedEnumeration>,
    kernel_unit_length_x: MutNullableDom<SVGAnimatedNumber>,
    kernel_unit_length_y: MutNullableDom<SVGAnimatedNumber>,
    preserve_alpha: MutNullableDom<SVGAnimatedBoolean>,
}

impl SVGFEConvolveMatrixElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGFEConvolveMatrixElement {
        SVGFEConvolveMatrixElement {
            svgelement: SVGElement::new_inherited_with_state(
                ElementState::empty(),
                local_name,
                prefix,
                document,
            ),
            standard_attributes: Default::default(),
            in1: Default::default(),
            order_x: Default::default(),
            order_y: Default::default(),
            divisor: Default::default(),
            bias: Default::default(),
            target_x: Default::default(),
            target_y: Default::default(),
            edge_mode: Default::default(),
            kernel_unit_length_x: Default::default(),
            kernel_unit_length_y: Default::default(),
            preserve_alpha: Default::default(),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGFEConvolveMatrixElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGFEConvolveMatrixElement::new_inherited(
                local_name, prefix, document,
            )),
            document,
            proto,
        )
    }
}

impl SVGFEConvolveMatrixElementMethods<crate::DomTypeHolder> for SVGFEConvolveMatrixElement {
    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfeconvolvematrixelement-in1>
    fn In1(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedString> {
        self.in1
            .or_init(|| SVGAnimatedString::new(cx, self.upcast(), local_name!("in")))
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfeconvolvematrixelement-orderx>
    fn OrderX(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedInteger> {
        self.order_x.or_init(|| {
            SVGAnimatedInteger::new(
                cx,
                self.upcast(),
                local_name!("order"),
                AttributePart::First,
                3,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfeconvolvematrixelement-ordery>
    fn OrderY(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedInteger> {
        self.order_y.or_init(|| {
            SVGAnimatedInteger::new(
                cx,
                self.upcast(),
                local_name!("order"),
                AttributePart::Second,
                3,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfeconvolvematrixelement-divisor>
    fn Divisor(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.divisor.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("divisor"),
                AttributePart::Whole,
                1.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfeconvolvematrixelement-bias>
    fn Bias(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.bias.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("bias"),
                AttributePart::Whole,
                0.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfeconvolvematrixelement-targetx>
    fn TargetX(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedInteger> {
        self.target_x.or_init(|| {
            SVGAnimatedInteger::new(
                cx,
                self.upcast(),
                local_name!("targetX"),
                AttributePart::Whole,
                0,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfeconvolvematrixelement-targety>
    fn TargetY(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedInteger> {
        self.target_y.or_init(|| {
            SVGAnimatedInteger::new(
                cx,
                self.upcast(),
                local_name!("targetY"),
                AttributePart::Whole,
                0,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfeconvolvematrixelement-edgemode>
    fn EdgeMode(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedEnumeration> {
        self.edge_mode.or_init(|| {
            SVGAnimatedEnumeration::new(
                cx,
                self.upcast(),
                local_name!("edgeMode"),
                SVGEnumeration::EdgeMode,
                1,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfeconvolvematrixelement-kernelunitlengthx>
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

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfeconvolvematrixelement-kernelunitlengthy>
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

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfeconvolvematrixelement-preservealpha>
    fn PreserveAlpha(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedBoolean> {
        self.preserve_alpha.or_init(|| {
            SVGAnimatedBoolean::new(cx, self.upcast(), local_name!("preserveAlpha"), false)
        })
    }

    filter_primitive_standard_attributes!();
}
