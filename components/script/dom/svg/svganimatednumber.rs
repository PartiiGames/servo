/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::LocalName;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object};

use crate::dom::bindings::codegen::Bindings::SVGAnimatedNumberBinding::SVGAnimatedNumberMethods;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::element::Element;
use crate::dom::node::NodeTraits;
use crate::dom::svg::svgreflectedattribute::{
    AttributePart, attribute_value, parse_number, set_attribute_value,
};

/// <https://svgwg.org/svg2-draft/types.html#InterfaceSVGAnimatedNumber>
#[dom_struct]
pub(crate) struct SVGAnimatedNumber {
    reflector_: Reflector,
    element: Dom<Element>,
    /// The reflected content attribute.
    #[no_trace]
    attribute: LocalName,
    /// Which number of the attribute this object reflects.
    part: AttributePart,
    /// The value used when the attribute is missing or invalid.
    initial: f32,
}

impl SVGAnimatedNumber {
    fn new_inherited(
        element: &Element,
        attribute: LocalName,
        part: AttributePart,
        initial: f32,
    ) -> SVGAnimatedNumber {
        SVGAnimatedNumber {
            reflector_: Reflector::new(),
            element: Dom::from_ref(element),
            attribute,
            part,
            initial,
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        element: &Element,
        attribute: LocalName,
        part: AttributePart,
        initial: f32,
    ) -> DomRoot<SVGAnimatedNumber> {
        reflect_dom_object(
            cx,
            Box::new(SVGAnimatedNumber::new_inherited(
                element, attribute, part, initial,
            )),
            &*element.owner_window(),
        )
    }

    pub(crate) fn base_value(&self) -> f32 {
        attribute_value(&self.element, &self.attribute)
            .and_then(|value| self.part.read(&value, parse_number))
            .unwrap_or(self.initial)
    }

    pub(crate) fn set_base_value(&self, cx: &mut JSContext, value: f32) {
        let current = attribute_value(&self.element, &self.attribute);
        let new_value = self
            .part
            .write(current.as_deref(), self.initial, value, parse_number);
        set_attribute_value(cx, &self.element, &self.attribute, new_value);
    }
}

impl SVGAnimatedNumberMethods<crate::DomTypeHolder> for SVGAnimatedNumber {
    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedNumber__baseVal>
    fn BaseVal(&self) -> Finite<f32> {
        Finite::wrap(self.base_value())
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedNumber__baseVal>
    fn SetBaseVal(&self, cx: &mut JSContext, value: Finite<f32>) {
        self.set_base_value(cx, *value);
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedNumber__animVal>
    fn AnimVal(&self) -> Finite<f32> {
        // Without SMIL animation the animated value is the base value.
        self.BaseVal()
    }
}
