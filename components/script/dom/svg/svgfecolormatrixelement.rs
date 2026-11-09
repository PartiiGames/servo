/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name};
use js::context::JSContext;
use js::rust::HandleObject;
use stylo_dom::ElementState;

use crate::dom::bindings::codegen::Bindings::SVGFEColorMatrixElementBinding::SVGFEColorMatrixElementMethods;
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

/// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGFEColorMatrixElement>
#[dom_struct]
pub(crate) struct SVGFEColorMatrixElement {
    svgelement: SVGElement,
    standard_attributes: SVGFilterPrimitiveStandardAttributes,
    in1: MutNullableDom<SVGAnimatedString>,
    type_: MutNullableDom<SVGAnimatedEnumeration>,
}

impl SVGFEColorMatrixElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGFEColorMatrixElement {
        SVGFEColorMatrixElement {
            svgelement: SVGElement::new_inherited_with_state(
                ElementState::empty(),
                local_name,
                prefix,
                document,
            ),
            standard_attributes: Default::default(),
            in1: Default::default(),
            type_: Default::default(),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGFEColorMatrixElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGFEColorMatrixElement::new_inherited(
                local_name, prefix, document,
            )),
            document,
            proto,
        )
    }
}

impl SVGFEColorMatrixElementMethods<crate::DomTypeHolder> for SVGFEColorMatrixElement {
    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfecolormatrixelement-in1>
    fn In1(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedString> {
        self.in1
            .or_init(|| SVGAnimatedString::new(cx, self.upcast(), local_name!("in")))
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfecolormatrixelement-type>
    fn Type(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedEnumeration> {
        self.type_.or_init(|| {
            SVGAnimatedEnumeration::new(
                cx,
                self.upcast(),
                local_name!("type"),
                SVGEnumeration::ColorMatrixType,
                1,
            )
        })
    }

    filter_primitive_standard_attributes!();
}
