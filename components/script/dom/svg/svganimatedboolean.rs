/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::LocalName;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object};

use crate::dom::bindings::codegen::Bindings::SVGAnimatedBooleanBinding::SVGAnimatedBooleanMethods;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::element::Element;
use crate::dom::node::NodeTraits;
use crate::dom::svg::svgreflectedattribute::{attribute_value, set_attribute_value};

/// <https://svgwg.org/svg2-draft/types.html#InterfaceSVGAnimatedBoolean>
///
/// Reflects an attribute whose valid values are the keywords "true" and "false".
#[dom_struct]
pub(crate) struct SVGAnimatedBoolean {
    reflector_: Reflector,
    element: Dom<Element>,
    /// The reflected content attribute.
    #[no_trace]
    attribute: LocalName,
    /// The value used when the attribute is missing or invalid.
    initial: bool,
}

impl SVGAnimatedBoolean {
    fn new_inherited(element: &Element, attribute: LocalName, initial: bool) -> SVGAnimatedBoolean {
        SVGAnimatedBoolean {
            reflector_: Reflector::new(),
            element: Dom::from_ref(element),
            attribute,
            initial,
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        element: &Element,
        attribute: LocalName,
        initial: bool,
    ) -> DomRoot<SVGAnimatedBoolean> {
        reflect_dom_object(
            cx,
            Box::new(SVGAnimatedBoolean::new_inherited(
                element, attribute, initial,
            )),
            &*element.owner_window(),
        )
    }
}

impl SVGAnimatedBooleanMethods<crate::DomTypeHolder> for SVGAnimatedBoolean {
    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedBoolean__baseVal>
    fn BaseVal(&self) -> bool {
        match attribute_value(&self.element, &self.attribute).as_deref() {
            Some("true") => true,
            Some("false") => false,
            _ => self.initial,
        }
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedBoolean__baseVal>
    fn SetBaseVal(&self, cx: &mut JSContext, value: bool) {
        set_attribute_value(cx, &self.element, &self.attribute, value.to_string());
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedBoolean__animVal>
    fn AnimVal(&self) -> bool {
        // Without SMIL animation the animated value is the base value.
        self.BaseVal()
    }
}
