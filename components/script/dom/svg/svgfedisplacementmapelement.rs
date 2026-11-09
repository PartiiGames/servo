/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name};
use js::context::JSContext;
use js::rust::HandleObject;
use stylo_dom::ElementState;

use crate::dom::bindings::codegen::Bindings::SVGFEDisplacementMapElementBinding::SVGFEDisplacementMapElementMethods;
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

/// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGFEDisplacementMapElement>
#[dom_struct]
pub(crate) struct SVGFEDisplacementMapElement {
    svgelement: SVGElement,
    standard_attributes: SVGFilterPrimitiveStandardAttributes,
    in1: MutNullableDom<SVGAnimatedString>,
    in2: MutNullableDom<SVGAnimatedString>,
    scale: MutNullableDom<SVGAnimatedNumber>,
    x_channel_selector: MutNullableDom<SVGAnimatedEnumeration>,
    y_channel_selector: MutNullableDom<SVGAnimatedEnumeration>,
}

impl SVGFEDisplacementMapElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGFEDisplacementMapElement {
        SVGFEDisplacementMapElement {
            svgelement: SVGElement::new_inherited_with_state(
                ElementState::empty(),
                local_name,
                prefix,
                document,
            ),
            standard_attributes: Default::default(),
            in1: Default::default(),
            in2: Default::default(),
            scale: Default::default(),
            x_channel_selector: Default::default(),
            y_channel_selector: Default::default(),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGFEDisplacementMapElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGFEDisplacementMapElement::new_inherited(
                local_name, prefix, document,
            )),
            document,
            proto,
        )
    }
}

impl SVGFEDisplacementMapElementMethods<crate::DomTypeHolder> for SVGFEDisplacementMapElement {
    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfedisplacementmapelement-in1>
    fn In1(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedString> {
        self.in1
            .or_init(|| SVGAnimatedString::new(cx, self.upcast(), local_name!("in")))
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfedisplacementmapelement-in2>
    fn In2(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedString> {
        self.in2
            .or_init(|| SVGAnimatedString::new(cx, self.upcast(), local_name!("in2")))
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfedisplacementmapelement-scale>
    fn Scale(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.scale.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("scale"),
                AttributePart::Whole,
                0.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfedisplacementmapelement-xchannelselector>
    fn XChannelSelector(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedEnumeration> {
        self.x_channel_selector.or_init(|| {
            SVGAnimatedEnumeration::new(
                cx,
                self.upcast(),
                local_name!("xChannelSelector"),
                SVGEnumeration::ChannelSelector,
                4,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfedisplacementmapelement-ychannelselector>
    fn YChannelSelector(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedEnumeration> {
        self.y_channel_selector.or_init(|| {
            SVGAnimatedEnumeration::new(
                cx,
                self.upcast(),
                local_name!("yChannelSelector"),
                SVGEnumeration::ChannelSelector,
                4,
            )
        })
    }

    filter_primitive_standard_attributes!();
}
