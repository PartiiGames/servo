/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name};
use js::context::JSContext;
use js::rust::HandleObject;
use stylo_dom::ElementState;

use crate::dom::bindings::codegen::Bindings::SVGFEDistantLightElementBinding::SVGFEDistantLightElementMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::document::Document;
use crate::dom::node::Node;
use crate::dom::svg::svganimatednumber::SVGAnimatedNumber;
use crate::dom::svg::svgelement::SVGElement;
use crate::dom::svg::svgreflectedattribute::AttributePart;

/// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGFEDistantLightElement>
#[dom_struct]
pub(crate) struct SVGFEDistantLightElement {
    svgelement: SVGElement,
    azimuth: MutNullableDom<SVGAnimatedNumber>,
    elevation: MutNullableDom<SVGAnimatedNumber>,
}

impl SVGFEDistantLightElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGFEDistantLightElement {
        SVGFEDistantLightElement {
            svgelement: SVGElement::new_inherited_with_state(
                ElementState::empty(),
                local_name,
                prefix,
                document,
            ),
            azimuth: Default::default(),
            elevation: Default::default(),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGFEDistantLightElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGFEDistantLightElement::new_inherited(
                local_name, prefix, document,
            )),
            document,
            proto,
        )
    }
}

impl SVGFEDistantLightElementMethods<crate::DomTypeHolder> for SVGFEDistantLightElement {
    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfedistantlightelement-azimuth>
    fn Azimuth(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.azimuth.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("azimuth"),
                AttributePart::Whole,
                0.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfedistantlightelement-elevation>
    fn Elevation(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.elevation.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("elevation"),
                AttributePart::Whole,
                0.,
            )
        })
    }
}
