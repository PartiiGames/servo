/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::LocalName;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object};

use crate::dom::bindings::codegen::Bindings::SVGAnimatedLengthBinding::SVGAnimatedLengthMethods;
use crate::dom::bindings::root::{Dom, DomRoot, MutNullableDom};
use crate::dom::element::Element;
use crate::dom::node::NodeTraits;
use crate::dom::svg::svglength::{SVGLength, SVGLengthValue};
use crate::dom::svg::svgreflectedattribute::{attribute_value, set_attribute_value};

/// <https://svgwg.org/svg2-draft/types.html#InterfaceSVGAnimatedLength>
#[dom_struct]
pub(crate) struct SVGAnimatedLength {
    reflector_: Reflector,
    element: Dom<Element>,
    /// The reflected content attribute.
    #[no_trace]
    attribute: LocalName,
    /// The value used when the attribute is missing or invalid.
    initial: SVGLengthValue,
    base_val: MutNullableDom<SVGLength>,
    anim_val: MutNullableDom<SVGLength>,
}

impl SVGAnimatedLength {
    fn new_inherited(
        element: &Element,
        attribute: LocalName,
        initial: SVGLengthValue,
    ) -> SVGAnimatedLength {
        SVGAnimatedLength {
            reflector_: Reflector::new(),
            element: Dom::from_ref(element),
            attribute,
            initial,
            base_val: Default::default(),
            anim_val: Default::default(),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        element: &Element,
        attribute: LocalName,
        initial: SVGLengthValue,
    ) -> DomRoot<SVGAnimatedLength> {
        reflect_dom_object(
            cx,
            Box::new(SVGAnimatedLength::new_inherited(
                element, attribute, initial,
            )),
            &*element.owner_window(),
        )
    }

    pub(crate) fn base_value(&self) -> SVGLengthValue {
        attribute_value(&self.element, &self.attribute)
            .and_then(|value| SVGLengthValue::parse(&value))
            .unwrap_or(self.initial)
    }

    pub(crate) fn set_base_value(&self, cx: &mut JSContext, value: SVGLengthValue) {
        set_attribute_value(cx, &self.element, &self.attribute, value.to_string());
    }
}

impl SVGAnimatedLengthMethods<crate::DomTypeHolder> for SVGAnimatedLength {
    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedLength__baseVal>
    fn BaseVal(&self, cx: &mut JSContext) -> DomRoot<SVGLength> {
        self.base_val
            .or_init(|| SVGLength::new(cx, &self.element.owner_window(), self, false))
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedLength__animVal>
    fn AnimVal(&self, cx: &mut JSContext) -> DomRoot<SVGLength> {
        // Without SMIL animation the animated value is the base value, but the animVal
        // object is read only.
        self.anim_val
            .or_init(|| SVGLength::new(cx, &self.element.owner_window(), self, true))
    }
}
