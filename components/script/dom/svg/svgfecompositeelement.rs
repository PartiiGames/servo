/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name};
use js::context::JSContext;
use js::rust::HandleObject;
use stylo_dom::ElementState;

use crate::dom::bindings::codegen::Bindings::SVGFECompositeElementBinding::SVGFECompositeElementMethods;
use crate::dom::bindings::inheritance::Castable;
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
use crate::dom::svg::svgreflectedattribute::AttributePart;

/// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGFECompositeElement>
#[dom_struct]
pub(crate) struct SVGFECompositeElement {
    svgelement: SVGElement,
    standard_attributes: SVGFilterPrimitiveStandardAttributes,
    in1: MutNullableDom<SVGAnimatedString>,
    in2: MutNullableDom<SVGAnimatedString>,
    operator: MutNullableDom<SVGAnimatedEnumeration>,
    k1: MutNullableDom<SVGAnimatedNumber>,
    k2: MutNullableDom<SVGAnimatedNumber>,
    k3: MutNullableDom<SVGAnimatedNumber>,
    k4: MutNullableDom<SVGAnimatedNumber>,
}

impl SVGFECompositeElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGFECompositeElement {
        SVGFECompositeElement {
            svgelement: SVGElement::new_inherited_with_state(
                ElementState::empty(),
                local_name,
                prefix,
                document,
            ),
            standard_attributes: Default::default(),
            in1: Default::default(),
            in2: Default::default(),
            operator: Default::default(),
            k1: Default::default(),
            k2: Default::default(),
            k3: Default::default(),
            k4: Default::default(),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGFECompositeElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGFECompositeElement::new_inherited(
                local_name, prefix, document,
            )),
            document,
            proto,
        )
    }
}

impl SVGFECompositeElementMethods<crate::DomTypeHolder> for SVGFECompositeElement {
    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfecompositeelement-in1>
    fn In1(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedString> {
        self.in1
            .or_init(|| SVGAnimatedString::new(cx, self.upcast(), local_name!("in")))
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfecompositeelement-in2>
    fn In2(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedString> {
        self.in2
            .or_init(|| SVGAnimatedString::new(cx, self.upcast(), local_name!("in2")))
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfecompositeelement-operator>
    fn Operator(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedEnumeration> {
        self.operator.or_init(|| {
            SVGAnimatedEnumeration::new(
                cx,
                self.upcast(),
                local_name!("operator"),
                SVGEnumeration::CompositeOperator,
                1,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfecompositeelement-k1>
    fn K1(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.k1.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("k1"),
                AttributePart::Whole,
                0.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfecompositeelement-k2>
    fn K2(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.k2.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("k2"),
                AttributePart::Whole,
                0.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfecompositeelement-k3>
    fn K3(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.k3.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("k3"),
                AttributePart::Whole,
                0.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfecompositeelement-k4>
    fn K4(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.k4.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("k4"),
                AttributePart::Whole,
                0.,
            )
        })
    }

    filter_primitive_standard_attributes!();
}
