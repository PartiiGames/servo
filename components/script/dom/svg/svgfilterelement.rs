/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name};
use js::context::JSContext;
use js::rust::HandleObject;
use stylo_dom::ElementState;

use crate::dom::bindings::codegen::Bindings::SVGFilterElementBinding::SVGFilterElementMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::document::Document;
use crate::dom::node::Node;
use crate::dom::svg::svganimatedenumeration::{SVGAnimatedEnumeration, SVGEnumeration};
use crate::dom::svg::svganimatedlength::SVGAnimatedLength;
use crate::dom::svg::svgelement::SVGElement;
use crate::dom::svg::svglength::{SVGLengthUnit, SVGLengthValue};

/// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGFilterElement>
#[dom_struct]
pub(crate) struct SVGFilterElement {
    svgelement: SVGElement,
    filter_units: MutNullableDom<SVGAnimatedEnumeration>,
    primitive_units: MutNullableDom<SVGAnimatedEnumeration>,
    x: MutNullableDom<SVGAnimatedLength>,
    y: MutNullableDom<SVGAnimatedLength>,
    width: MutNullableDom<SVGAnimatedLength>,
    height: MutNullableDom<SVGAnimatedLength>,
}

impl SVGFilterElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGFilterElement {
        SVGFilterElement {
            svgelement: SVGElement::new_inherited_with_state(
                ElementState::empty(),
                local_name,
                prefix,
                document,
            ),
            filter_units: Default::default(),
            primitive_units: Default::default(),
            x: Default::default(),
            y: Default::default(),
            width: Default::default(),
            height: Default::default(),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGFilterElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGFilterElement::new_inherited(
                local_name, prefix, document,
            )),
            document,
            proto,
        )
    }
}

impl SVGFilterElementMethods<crate::DomTypeHolder> for SVGFilterElement {
    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfilterelement-filterunits>
    fn FilterUnits(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedEnumeration> {
        self.filter_units.or_init(|| {
            SVGAnimatedEnumeration::new(
                cx,
                self.upcast(),
                local_name!("filterUnits"),
                SVGEnumeration::UnitTypes,
                2,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfilterelement-primitiveunits>
    fn PrimitiveUnits(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedEnumeration> {
        self.primitive_units.or_init(|| {
            SVGAnimatedEnumeration::new(
                cx,
                self.upcast(),
                local_name!("primitiveUnits"),
                SVGEnumeration::UnitTypes,
                1,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfilterelement-x>
    fn X(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedLength> {
        self.x.or_init(|| {
            SVGAnimatedLength::new(
                cx,
                self.upcast(),
                local_name!("x"),
                SVGLengthValue::new(-10., SVGLengthUnit::Percentage),
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfilterelement-y>
    fn Y(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedLength> {
        self.y.or_init(|| {
            SVGAnimatedLength::new(
                cx,
                self.upcast(),
                local_name!("y"),
                SVGLengthValue::new(-10., SVGLengthUnit::Percentage),
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfilterelement-width>
    fn Width(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedLength> {
        self.width.or_init(|| {
            SVGAnimatedLength::new(
                cx,
                self.upcast(),
                local_name!("width"),
                SVGLengthValue::new(120., SVGLengthUnit::Percentage),
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgfilterelement-height>
    fn Height(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedLength> {
        self.height.or_init(|| {
            SVGAnimatedLength::new(
                cx,
                self.upcast(),
                local_name!("height"),
                SVGLengthValue::new(120., SVGLengthUnit::Percentage),
            )
        })
    }
}
