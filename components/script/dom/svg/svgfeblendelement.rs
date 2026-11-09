/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name};
use js::context::JSContext;
use js::rust::HandleObject;
use stylo_dom::ElementState;

use crate::dom::bindings::codegen::Bindings::SVGFEBlendElementBinding::SVGFEBlendElementMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::document::Document;
use crate::dom::node::Node;
use crate::dom::svg::svganimatedenumeration::{SVGAnimatedEnumeration, SVGEnumeration};
use crate::dom::svg::svganimatedstring::SVGAnimatedString;
use crate::dom::svg::svgelement::SVGElement;
use crate::dom::svg::svgfilterprimitivestandardattributes::{
    SVGFilterPrimitiveStandardAttributes, filter_primitive_standard_attributes,
};

/// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGFEBlendElement>
#[dom_struct]
pub(crate) struct SVGFEBlendElement {
    svgelement: SVGElement,
    standard_attributes: SVGFilterPrimitiveStandardAttributes,
    in1: MutNullableDom<SVGAnimatedString>,
    in2: MutNullableDom<SVGAnimatedString>,
    mode: MutNullableDom<SVGAnimatedEnumeration>,
}

impl SVGFEBlendElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGFEBlendElement {
        SVGFEBlendElement {
            svgelement: SVGElement::new_inherited_with_state(
                ElementState::empty(),
                local_name,
                prefix,
                document,
            ),
            standard_attributes: Default::default(),
            in1: Default::default(),
            in2: Default::default(),
            mode: Default::default(),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGFEBlendElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGFEBlendElement::new_inherited(
                local_name, prefix, document,
            )),
            document,
            proto,
        )
    }
}

impl SVGFEBlendElementMethods<crate::DomTypeHolder> for SVGFEBlendElement {
    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfeblendelement-in1>
    fn In1(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedString> {
        self.in1
            .or_init(|| SVGAnimatedString::new(cx, self.upcast(), local_name!("in")))
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfeblendelement-in2>
    fn In2(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedString> {
        self.in2
            .or_init(|| SVGAnimatedString::new(cx, self.upcast(), local_name!("in2")))
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfeblendelement-mode>
    fn Mode(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedEnumeration> {
        self.mode.or_init(|| {
            SVGAnimatedEnumeration::new(
                cx,
                self.upcast(),
                local_name!("mode"),
                SVGEnumeration::BlendMode,
                1,
            )
        })
    }

    filter_primitive_standard_attributes!();
}
