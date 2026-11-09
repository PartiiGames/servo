/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name};
use js::context::JSContext;
use js::rust::HandleObject;
use stylo_dom::ElementState;

use crate::dom::bindings::codegen::Bindings::SVGFEImageElementBinding::SVGFEImageElementMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::document::Document;
use crate::dom::node::Node;
use crate::dom::svg::svganimatedstring::SVGAnimatedString;
use crate::dom::svg::svgelement::SVGElement;
use crate::dom::svg::svgfilterprimitivestandardattributes::{
    SVGFilterPrimitiveStandardAttributes, filter_primitive_standard_attributes,
};

/// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGFEImageElement>
#[dom_struct]
pub(crate) struct SVGFEImageElement {
    svgelement: SVGElement,
    standard_attributes: SVGFilterPrimitiveStandardAttributes,
    href: MutNullableDom<SVGAnimatedString>,
    cross_origin: MutNullableDom<SVGAnimatedString>,
}

impl SVGFEImageElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGFEImageElement {
        SVGFEImageElement {
            svgelement: SVGElement::new_inherited_with_state(
                ElementState::empty(),
                local_name,
                prefix,
                document,
            ),
            standard_attributes: Default::default(),
            href: Default::default(),
            cross_origin: Default::default(),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGFEImageElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGFEImageElement::new_inherited(
                local_name, prefix, document,
            )),
            document,
            proto,
        )
    }
}

impl SVGFEImageElementMethods<crate::DomTypeHolder> for SVGFEImageElement {
    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfeimageelement-crossorigin>
    fn CrossOrigin(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedString> {
        self.cross_origin
            .or_init(|| SVGAnimatedString::new(cx, self.upcast(), local_name!("crossorigin")))
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGURIReference__href>
    fn Href(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedString> {
        self.href
            .or_init(|| SVGAnimatedString::new_href(cx, self.upcast()))
    }

    filter_primitive_standard_attributes!();
}
