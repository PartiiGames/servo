/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name};
use js::context::JSContext;
use js::rust::HandleObject;
use stylo_dom::ElementState;

use crate::dom::bindings::codegen::Bindings::SVGFESpotLightElementBinding::SVGFESpotLightElementMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::document::Document;
use crate::dom::node::Node;
use crate::dom::svg::svganimatednumber::SVGAnimatedNumber;
use crate::dom::svg::svgelement::SVGElement;
use crate::dom::svg::svgreflectedattribute::AttributePart;

/// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGFESpotLightElement>
#[dom_struct]
pub(crate) struct SVGFESpotLightElement {
    svgelement: SVGElement,
    x: MutNullableDom<SVGAnimatedNumber>,
    y: MutNullableDom<SVGAnimatedNumber>,
    z: MutNullableDom<SVGAnimatedNumber>,
    points_at_x: MutNullableDom<SVGAnimatedNumber>,
    points_at_y: MutNullableDom<SVGAnimatedNumber>,
    points_at_z: MutNullableDom<SVGAnimatedNumber>,
    specular_exponent: MutNullableDom<SVGAnimatedNumber>,
    limiting_cone_angle: MutNullableDom<SVGAnimatedNumber>,
}

impl SVGFESpotLightElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGFESpotLightElement {
        SVGFESpotLightElement {
            svgelement: SVGElement::new_inherited_with_state(
                ElementState::empty(),
                local_name,
                prefix,
                document,
            ),
            x: Default::default(),
            y: Default::default(),
            z: Default::default(),
            points_at_x: Default::default(),
            points_at_y: Default::default(),
            points_at_z: Default::default(),
            specular_exponent: Default::default(),
            limiting_cone_angle: Default::default(),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGFESpotLightElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGFESpotLightElement::new_inherited(
                local_name, prefix, document,
            )),
            document,
            proto,
        )
    }
}

impl SVGFESpotLightElementMethods<crate::DomTypeHolder> for SVGFESpotLightElement {
    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfespotlightelement-x>
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

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfespotlightelement-y>
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

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfespotlightelement-z>
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

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfespotlightelement-pointsatx>
    fn PointsAtX(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.points_at_x.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("pointsAtX"),
                AttributePart::Whole,
                0.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfespotlightelement-pointsaty>
    fn PointsAtY(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.points_at_y.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("pointsAtY"),
                AttributePart::Whole,
                0.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfespotlightelement-pointsatz>
    fn PointsAtZ(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.points_at_z.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("pointsAtZ"),
                AttributePart::Whole,
                0.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfespotlightelement-specularexponent>
    fn SpecularExponent(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.specular_exponent.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("specularExponent"),
                AttributePart::Whole,
                1.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfespotlightelement-limitingconeangle>
    fn LimitingConeAngle(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.limiting_cone_angle.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("limitingConeAngle"),
                AttributePart::Whole,
                0.,
            )
        })
    }
}
