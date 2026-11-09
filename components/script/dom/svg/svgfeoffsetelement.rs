/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name};
use js::context::JSContext;
use js::rust::HandleObject;
use stylo_dom::ElementState;

use crate::dom::bindings::codegen::Bindings::SVGFEOffsetElementBinding::SVGFEOffsetElementMethods;
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

/// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGFEOffsetElement>
#[dom_struct]
pub(crate) struct SVGFEOffsetElement {
    svgelement: SVGElement,
    standard_attributes: SVGFilterPrimitiveStandardAttributes,
    in1: MutNullableDom<SVGAnimatedString>,
    dx: MutNullableDom<SVGAnimatedNumber>,
    dy: MutNullableDom<SVGAnimatedNumber>,
}

impl SVGFEOffsetElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGFEOffsetElement {
        SVGFEOffsetElement {
            svgelement: SVGElement::new_inherited_with_state(
                ElementState::empty(),
                local_name,
                prefix,
                document,
            ),
            standard_attributes: Default::default(),
            in1: Default::default(),
            dx: Default::default(),
            dy: Default::default(),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGFEOffsetElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGFEOffsetElement::new_inherited(
                local_name, prefix, document,
            )),
            document,
            proto,
        )
    }
}

impl SVGFEOffsetElementMethods<crate::DomTypeHolder> for SVGFEOffsetElement {
    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfeoffsetelement-in1>
    fn In1(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedString> {
        self.in1
            .or_init(|| SVGAnimatedString::new(cx, self.upcast(), local_name!("in")))
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfeoffsetelement-dx>
    fn Dx(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.dx.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("dx"),
                AttributePart::Whole,
                0.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfeoffsetelement-dy>
    fn Dy(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.dy.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("dy"),
                AttributePart::Whole,
                0.,
            )
        })
    }

    filter_primitive_standard_attributes!();
}
