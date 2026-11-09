/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::LocalName;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object};

use crate::dom::bindings::codegen::Bindings::SVGAnimatedIntegerBinding::SVGAnimatedIntegerMethods;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::element::Element;
use crate::dom::node::NodeTraits;
use crate::dom::svg::svgreflectedattribute::{
    AttributePart, attribute_value, parse_integer, set_attribute_value,
};

/// <https://svgwg.org/svg2-draft/types.html#InterfaceSVGAnimatedInteger>
#[dom_struct]
pub(crate) struct SVGAnimatedInteger {
    reflector_: Reflector,
    element: Dom<Element>,
    /// The reflected content attribute.
    #[no_trace]
    attribute: LocalName,
    /// Which integer of the attribute this object reflects.
    part: AttributePart,
    /// The value used when the attribute is missing or invalid.
    initial: i32,
}

impl SVGAnimatedInteger {
    fn new_inherited(
        element: &Element,
        attribute: LocalName,
        part: AttributePart,
        initial: i32,
    ) -> SVGAnimatedInteger {
        SVGAnimatedInteger {
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
        initial: i32,
    ) -> DomRoot<SVGAnimatedInteger> {
        reflect_dom_object(
            cx,
            Box::new(SVGAnimatedInteger::new_inherited(
                element, attribute, part, initial,
            )),
            &*element.owner_window(),
        )
    }
}

impl SVGAnimatedIntegerMethods<crate::DomTypeHolder> for SVGAnimatedInteger {
    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedInteger__baseVal>
    fn BaseVal(&self) -> i32 {
        attribute_value(&self.element, &self.attribute)
            .and_then(|value| self.part.read(&value, parse_integer))
            .unwrap_or(self.initial)
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedInteger__baseVal>
    fn SetBaseVal(&self, cx: &mut JSContext, value: i32) {
        let current = attribute_value(&self.element, &self.attribute);
        let new_value = self
            .part
            .write(current.as_deref(), self.initial, value, parse_integer);
        set_attribute_value(cx, &self.element, &self.attribute, new_value);
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedInteger__animVal>
    fn AnimVal(&self) -> i32 {
        // Without SMIL animation the animated value is the base value.
        self.BaseVal()
    }
}
