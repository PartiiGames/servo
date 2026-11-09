/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name};
use js::context::JSContext;
use js::rust::HandleObject;
use stylo_dom::ElementState;

use crate::dom::bindings::codegen::Bindings::SVGFEPointLightElementBinding::SVGFEPointLightElementMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::document::Document;
use crate::dom::node::Node;
use crate::dom::svg::svganimatednumber::SVGAnimatedNumber;
use crate::dom::svg::svgelement::SVGElement;
use crate::dom::svg::svgreflectedattribute::AttributePart;

/// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGFEPointLightElement>
#[dom_struct]
pub(crate) struct SVGFEPointLightElement {
    svgelement: SVGElement,
    x: MutNullableDom<SVGAnimatedNumber>,
    y: MutNullableDom<SVGAnimatedNumber>,
    z: MutNullableDom<SVGAnimatedNumber>,
}

impl SVGFEPointLightElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGFEPointLightElement {
        SVGFEPointLightElement {
            svgelement: SVGElement::new_inherited_with_state(
                ElementState::empty(),
                local_name,
                prefix,
                document,
            ),
            x: Default::default(),
            y: Default::default(),
            z: Default::default(),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGFEPointLightElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGFEPointLightElement::new_inherited(
                local_name, prefix, document,
            )),
            document,
            proto,
        )
    }
}

impl SVGFEPointLightElementMethods<crate::DomTypeHolder> for SVGFEPointLightElement {
    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfepointlightelement-x>
    fn X(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.x.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("x"),
                AttributePart::Whole,
                0.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfepointlightelement-y>
    fn Y(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.y.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("y"),
                AttributePart::Whole,
                0.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfepointlightelement-z>
    fn Z(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.z.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("z"),
                AttributePart::Whole,
                0.,
            )
        })
    }
}
