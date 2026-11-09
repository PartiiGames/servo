/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name};
use js::context::JSContext;
use js::rust::HandleObject;
use stylo_dom::ElementState;

use crate::dom::bindings::codegen::Bindings::SVGFEMorphologyElementBinding::SVGFEMorphologyElementMethods;
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

/// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGFEMorphologyElement>
#[dom_struct]
pub(crate) struct SVGFEMorphologyElement {
    svgelement: SVGElement,
    standard_attributes: SVGFilterPrimitiveStandardAttributes,
    in1: MutNullableDom<SVGAnimatedString>,
    operator: MutNullableDom<SVGAnimatedEnumeration>,
    radius_x: MutNullableDom<SVGAnimatedNumber>,
    radius_y: MutNullableDom<SVGAnimatedNumber>,
}

impl SVGFEMorphologyElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGFEMorphologyElement {
        SVGFEMorphologyElement {
            svgelement: SVGElement::new_inherited_with_state(
                ElementState::empty(),
                local_name,
                prefix,
                document,
            ),
            standard_attributes: Default::default(),
            in1: Default::default(),
            operator: Default::default(),
            radius_x: Default::default(),
            radius_y: Default::default(),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGFEMorphologyElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGFEMorphologyElement::new_inherited(
                local_name, prefix, document,
            )),
            document,
            proto,
        )
    }
}

impl SVGFEMorphologyElementMethods<crate::DomTypeHolder> for SVGFEMorphologyElement {
    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfemorphologyelement-in1>
    fn In1(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedString> {
        self.in1
            .or_init(|| SVGAnimatedString::new(cx, self.upcast(), local_name!("in")))
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfemorphologyelement-operator>
    fn Operator(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedEnumeration> {
        self.operator.or_init(|| {
            SVGAnimatedEnumeration::new(
                cx,
                self.upcast(),
                local_name!("operator"),
                SVGEnumeration::MorphologyOperator,
                1,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfemorphologyelement-radiusx>
    fn RadiusX(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.radius_x.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("radius"),
                AttributePart::First,
                0.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfemorphologyelement-radiusy>
    fn RadiusY(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.radius_y.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("radius"),
                AttributePart::Second,
                0.,
            )
        })
    }

    filter_primitive_standard_attributes!();
}
