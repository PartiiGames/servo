/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name};
use js::context::JSContext;
use js::rust::HandleObject;
use stylo_dom::ElementState;

use crate::dom::bindings::codegen::Bindings::SVGFETurbulenceElementBinding::SVGFETurbulenceElementMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::document::Document;
use crate::dom::node::Node;
use crate::dom::svg::svganimatedenumeration::{SVGAnimatedEnumeration, SVGEnumeration};
use crate::dom::svg::svganimatedinteger::SVGAnimatedInteger;
use crate::dom::svg::svganimatednumber::SVGAnimatedNumber;
use crate::dom::svg::svgelement::SVGElement;
use crate::dom::svg::svgfilterprimitivestandardattributes::{
    SVGFilterPrimitiveStandardAttributes, filter_primitive_standard_attributes,
};
use crate::dom::svg::svgreflectedattribute::AttributePart;

/// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGFETurbulenceElement>
#[dom_struct]
pub(crate) struct SVGFETurbulenceElement {
    svgelement: SVGElement,
    standard_attributes: SVGFilterPrimitiveStandardAttributes,
    base_frequency_x: MutNullableDom<SVGAnimatedNumber>,
    base_frequency_y: MutNullableDom<SVGAnimatedNumber>,
    num_octaves: MutNullableDom<SVGAnimatedInteger>,
    seed: MutNullableDom<SVGAnimatedNumber>,
    stitch_tiles: MutNullableDom<SVGAnimatedEnumeration>,
    type_: MutNullableDom<SVGAnimatedEnumeration>,
}

impl SVGFETurbulenceElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGFETurbulenceElement {
        SVGFETurbulenceElement {
            svgelement: SVGElement::new_inherited_with_state(
                ElementState::empty(),
                local_name,
                prefix,
                document,
            ),
            standard_attributes: Default::default(),
            base_frequency_x: Default::default(),
            base_frequency_y: Default::default(),
            num_octaves: Default::default(),
            seed: Default::default(),
            stitch_tiles: Default::default(),
            type_: Default::default(),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGFETurbulenceElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGFETurbulenceElement::new_inherited(
                local_name, prefix, document,
            )),
            document,
            proto,
        )
    }
}

impl SVGFETurbulenceElementMethods<crate::DomTypeHolder> for SVGFETurbulenceElement {
    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfeturbulenceelement-basefrequencyx>
    fn BaseFrequencyX(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.base_frequency_x.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("baseFrequency"),
                AttributePart::First,
                0.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfeturbulenceelement-basefrequencyy>
    fn BaseFrequencyY(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.base_frequency_y.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("baseFrequency"),
                AttributePart::Second,
                0.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfeturbulenceelement-numoctaves>
    fn NumOctaves(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedInteger> {
        self.num_octaves.or_init(|| {
            SVGAnimatedInteger::new(
                cx,
                self.upcast(),
                local_name!("numOctaves"),
                AttributePart::Whole,
                1,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfeturbulenceelement-seed>
    fn Seed(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.seed.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("seed"),
                AttributePart::Whole,
                0.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfeturbulenceelement-stitchtiles>
    fn StitchTiles(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedEnumeration> {
        self.stitch_tiles.or_init(|| {
            SVGAnimatedEnumeration::new(
                cx,
                self.upcast(),
                local_name!("stitchTiles"),
                SVGEnumeration::StitchType,
                2,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfeturbulenceelement-type>
    fn Type(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedEnumeration> {
        self.type_.or_init(|| {
            SVGAnimatedEnumeration::new(
                cx,
                self.upcast(),
                local_name!("type"),
                SVGEnumeration::TurbulenceType,
                2,
            )
        })
    }

    filter_primitive_standard_attributes!();
}
