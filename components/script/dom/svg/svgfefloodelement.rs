/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix};
use js::context::JSContext;
use js::rust::HandleObject;
use stylo_dom::ElementState;

use crate::dom::bindings::codegen::Bindings::SVGFEFloodElementBinding::SVGFEFloodElementMethods;
use crate::dom::bindings::root::DomRoot;
use crate::dom::document::Document;
use crate::dom::node::Node;
use crate::dom::svg::svgelement::SVGElement;
use crate::dom::svg::svgfilterprimitivestandardattributes::{
    SVGFilterPrimitiveStandardAttributes, filter_primitive_standard_attributes,
};

/// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGFEFloodElement>
#[dom_struct]
pub(crate) struct SVGFEFloodElement {
    svgelement: SVGElement,
    standard_attributes: SVGFilterPrimitiveStandardAttributes,
}

impl SVGFEFloodElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGFEFloodElement {
        SVGFEFloodElement {
            svgelement: SVGElement::new_inherited_with_state(
                ElementState::empty(),
                local_name,
                prefix,
                document,
            ),
            standard_attributes: Default::default(),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGFEFloodElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGFEFloodElement::new_inherited(
                local_name, prefix, document,
            )),
            document,
            proto,
        )
    }
}

impl SVGFEFloodElementMethods<crate::DomTypeHolder> for SVGFEFloodElement {
    filter_primitive_standard_attributes!();
}
