/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, local_name, ns};
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object};

use crate::dom::bindings::codegen::Bindings::SVGAnimatedStringBinding::SVGAnimatedStringMethods;
use crate::dom::bindings::codegen::UnionTypes::StringOrTrustedScriptURL;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::element::Element;
use crate::dom::node::NodeTraits;
use crate::dom::svg::svgreflectedattribute::{attribute_value, set_attribute_value};

/// <https://svgwg.org/svg2-draft/types.html#InterfaceSVGAnimatedString>
#[dom_struct]
pub(crate) struct SVGAnimatedString {
    reflector_: Reflector,
    element: Dom<Element>,
    /// The reflected content attribute.
    #[no_trace]
    attribute: LocalName,
    /// Whether a missing attribute falls back to the deprecated `xlink:href`; true for
    /// `SVGURIReference.href`.
    xlink_fallback: bool,
}

impl SVGAnimatedString {
    fn new_inherited(
        element: &Element,
        attribute: LocalName,
        xlink_fallback: bool,
    ) -> SVGAnimatedString {
        SVGAnimatedString {
            reflector_: Reflector::new(),
            element: Dom::from_ref(element),
            attribute,
            xlink_fallback,
        }
    }

    /// Reflects the null-namespace attribute `attribute`; a missing attribute reads as "".
    pub(crate) fn new(
        cx: &mut JSContext,
        element: &Element,
        attribute: LocalName,
    ) -> DomRoot<SVGAnimatedString> {
        reflect_dom_object(
            cx,
            Box::new(SVGAnimatedString::new_inherited(element, attribute, false)),
            &*element.owner_window(),
        )
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGURIReference__href>
    ///
    /// Reflects `href`, or `xlink:href` when there is no `href` attribute.
    pub(crate) fn new_href(cx: &mut JSContext, element: &Element) -> DomRoot<SVGAnimatedString> {
        reflect_dom_object(
            cx,
            Box::new(SVGAnimatedString::new_inherited(
                element,
                local_name!("href"),
                true,
            )),
            &*element.owner_window(),
        )
    }

    fn value(&self) -> DOMString {
        let value = attribute_value(&self.element, &self.attribute).or_else(|| {
            if !self.xlink_fallback {
                return None;
            }
            self.element
                .get_attribute_string_value_with_namespace(&ns!(xlink), &self.attribute)
        });
        value.unwrap_or_default().into()
    }
}

impl SVGAnimatedStringMethods<crate::DomTypeHolder> for SVGAnimatedString {
    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedString__baseVal>
    fn BaseVal(&self) -> StringOrTrustedScriptURL {
        StringOrTrustedScriptURL::String(self.value())
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedString__baseVal>
    fn SetBaseVal(&self, cx: &mut JSContext, value: StringOrTrustedScriptURL) {
        // Trusted Types only guard the href of SVG script elements, which this object never
        // reflects, so a TrustedScriptURL is used for its string value.
        let value = match value {
            StringOrTrustedScriptURL::String(value) => value,
            StringOrTrustedScriptURL::TrustedScriptURL(url) => url.data().clone(),
        };
        set_attribute_value(cx, &self.element, &self.attribute, value.into());
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedString__animVal>
    fn AnimVal(&self) -> DOMString {
        // Without SMIL animation the animated value is the base value.
        self.value()
    }
}
