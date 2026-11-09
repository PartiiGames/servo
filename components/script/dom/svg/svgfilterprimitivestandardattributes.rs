/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use html5ever::local_name;
use js::context::JSContext;

use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::element::Element;
use crate::dom::svg::svganimatedlength::SVGAnimatedLength;
use crate::dom::svg::svganimatedstring::SVGAnimatedString;
use crate::dom::svg::svglength::{SVGLengthUnit, SVGLengthValue};

/// The `[SameObject]` reflections of the `SVGFilterPrimitiveStandardAttributes` mixin,
/// held by every filter primitive element.
///
/// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGFilterPrimitiveStandardAttributes>
#[derive(Default, JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
pub(crate) struct SVGFilterPrimitiveStandardAttributes {
    x: MutNullableDom<SVGAnimatedLength>,
    y: MutNullableDom<SVGAnimatedLength>,
    width: MutNullableDom<SVGAnimatedLength>,
    height: MutNullableDom<SVGAnimatedLength>,
    result: MutNullableDom<SVGAnimatedString>,
}

/// The filter primitive subregion defaults to the whole filter region: x and y are 0%,
/// width and height are 100%.
///
/// <https://drafts.fxtf.org/filter-effects/#element-attrdef-filter-primitive-x>
const ZERO_PERCENT: SVGLengthValue = SVGLengthValue::new(0., SVGLengthUnit::Percentage);
const HUNDRED_PERCENT: SVGLengthValue = SVGLengthValue::new(100., SVGLengthUnit::Percentage);

impl SVGFilterPrimitiveStandardAttributes {
    pub(crate) fn x(&self, cx: &mut JSContext, element: &Element) -> DomRoot<SVGAnimatedLength> {
        self.x
            .or_init(|| SVGAnimatedLength::new(cx, element, local_name!("x"), ZERO_PERCENT))
    }

    pub(crate) fn y(&self, cx: &mut JSContext, element: &Element) -> DomRoot<SVGAnimatedLength> {
        self.y
            .or_init(|| SVGAnimatedLength::new(cx, element, local_name!("y"), ZERO_PERCENT))
    }

    pub(crate) fn width(
        &self,
        cx: &mut JSContext,
        element: &Element,
    ) -> DomRoot<SVGAnimatedLength> {
        self.width
            .or_init(|| SVGAnimatedLength::new(cx, element, local_name!("width"), HUNDRED_PERCENT))
    }

    pub(crate) fn height(
        &self,
        cx: &mut JSContext,
        element: &Element,
    ) -> DomRoot<SVGAnimatedLength> {
        self.height
            .or_init(|| SVGAnimatedLength::new(cx, element, local_name!("height"), HUNDRED_PERCENT))
    }

    pub(crate) fn result(
        &self,
        cx: &mut JSContext,
        element: &Element,
    ) -> DomRoot<SVGAnimatedString> {
        self.result
            .or_init(|| SVGAnimatedString::new(cx, element, local_name!("result")))
    }
}

/// Implements the `SVGFilterPrimitiveStandardAttributes` mixin methods inside an
/// `SVG*ElementMethods` impl, for an element with a `standard_attributes:
/// SVGFilterPrimitiveStandardAttributes` field.
macro_rules! filter_primitive_standard_attributes(
    () => (
        /// <https://drafts.fxtf.org/filter-effects/#dom-svgfilterprimitivestandardattributes-x>
        fn X(
            &self,
            cx: &mut js::context::JSContext,
        ) -> $crate::dom::bindings::root::DomRoot<$crate::dom::svg::svganimatedlength::SVGAnimatedLength> {
            self.standard_attributes.x(cx, $crate::dom::bindings::inheritance::Castable::upcast(self))
        }

        /// <https://drafts.fxtf.org/filter-effects/#dom-svgfilterprimitivestandardattributes-y>
        fn Y(
            &self,
            cx: &mut js::context::JSContext,
        ) -> $crate::dom::bindings::root::DomRoot<$crate::dom::svg::svganimatedlength::SVGAnimatedLength> {
            self.standard_attributes.y(cx, $crate::dom::bindings::inheritance::Castable::upcast(self))
        }

        /// <https://drafts.fxtf.org/filter-effects/#dom-svgfilterprimitivestandardattributes-width>
        fn Width(
            &self,
            cx: &mut js::context::JSContext,
        ) -> $crate::dom::bindings::root::DomRoot<$crate::dom::svg::svganimatedlength::SVGAnimatedLength> {
            self.standard_attributes.width(cx, $crate::dom::bindings::inheritance::Castable::upcast(self))
        }

        /// <https://drafts.fxtf.org/filter-effects/#dom-svgfilterprimitivestandardattributes-height>
        fn Height(
            &self,
            cx: &mut js::context::JSContext,
        ) -> $crate::dom::bindings::root::DomRoot<$crate::dom::svg::svganimatedlength::SVGAnimatedLength> {
            self.standard_attributes.height(cx, $crate::dom::bindings::inheritance::Castable::upcast(self))
        }

        /// <https://drafts.fxtf.org/filter-effects/#dom-svgfilterprimitivestandardattributes-result>
        fn Result(
            &self,
            cx: &mut js::context::JSContext,
        ) -> $crate::dom::bindings::root::DomRoot<$crate::dom::svg::svganimatedstring::SVGAnimatedString> {
            self.standard_attributes.result(cx, $crate::dom::bindings::inheritance::Castable::upcast(self))
        }
    )
);

pub(crate) use filter_primitive_standard_attributes;
