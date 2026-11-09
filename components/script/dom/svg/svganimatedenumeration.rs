/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::LocalName;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object};

use crate::dom::bindings::codegen::Bindings::SVGAnimatedEnumerationBinding::SVGAnimatedEnumerationMethods;
use crate::dom::bindings::error::{Error, ErrorResult};
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::element::Element;
use crate::dom::node::NodeTraits;
use crate::dom::svg::svgreflectedattribute::{attribute_value, set_attribute_value};

/// The keyword sets reflected through `SVGAnimatedEnumeration`. The keyword at index `i`
/// has the numeric value `i + 1`; 0 is always the `*_UNKNOWN` constant.
#[derive(Clone, Copy, JSTraceable, MallocSizeOf)]
pub(crate) enum SVGEnumeration {
    /// <https://svgwg.org/svg2-draft/types.html#InterfaceSVGUnitTypes>
    UnitTypes,
    /// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGFEBlendElement>
    BlendMode,
    /// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGFEColorMatrixElement>
    ColorMatrixType,
    /// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGComponentTransferFunctionElement>
    ComponentTransferType,
    /// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGFECompositeElement>
    CompositeOperator,
    /// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGFEConvolveMatrixElement>
    EdgeMode,
    /// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGFEDisplacementMapElement>
    ChannelSelector,
    /// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGFEMorphologyElement>
    MorphologyOperator,
    /// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGFETurbulenceElement>
    TurbulenceType,
    /// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGFETurbulenceElement>
    StitchType,
}

impl SVGEnumeration {
    fn keywords(self) -> &'static [&'static str] {
        match self {
            SVGEnumeration::UnitTypes => &["userSpaceOnUse", "objectBoundingBox"],
            SVGEnumeration::BlendMode => &[
                "normal",
                "multiply",
                "screen",
                "darken",
                "lighten",
                "overlay",
                "color-dodge",
                "color-burn",
                "hard-light",
                "soft-light",
                "difference",
                "exclusion",
                "hue",
                "saturation",
                "color",
                "luminosity",
            ],
            SVGEnumeration::ColorMatrixType => {
                &["matrix", "saturate", "hueRotate", "luminanceToAlpha"]
            },
            SVGEnumeration::ComponentTransferType => {
                &["identity", "table", "discrete", "linear", "gamma"]
            },
            SVGEnumeration::CompositeOperator => {
                &["over", "in", "out", "atop", "xor", "arithmetic"]
            },
            SVGEnumeration::EdgeMode => &["duplicate", "wrap", "none"],
            SVGEnumeration::ChannelSelector => &["R", "G", "B", "A"],
            SVGEnumeration::MorphologyOperator => &["erode", "dilate"],
            SVGEnumeration::TurbulenceType => &["fractalNoise", "turbulence"],
            SVGEnumeration::StitchType => &["stitch", "noStitch"],
        }
    }
}

/// <https://svgwg.org/svg2-draft/types.html#InterfaceSVGAnimatedEnumeration>
#[dom_struct]
pub(crate) struct SVGAnimatedEnumeration {
    reflector_: Reflector,
    element: Dom<Element>,
    /// The reflected content attribute.
    #[no_trace]
    attribute: LocalName,
    /// The keywords the attribute accepts.
    enumeration: SVGEnumeration,
    /// The numeric value used when the attribute is missing or not one of the keywords.
    initial: u16,
}

impl SVGAnimatedEnumeration {
    fn new_inherited(
        element: &Element,
        attribute: LocalName,
        enumeration: SVGEnumeration,
        initial: u16,
    ) -> SVGAnimatedEnumeration {
        SVGAnimatedEnumeration {
            reflector_: Reflector::new(),
            element: Dom::from_ref(element),
            attribute,
            enumeration,
            initial,
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        element: &Element,
        attribute: LocalName,
        enumeration: SVGEnumeration,
        initial: u16,
    ) -> DomRoot<SVGAnimatedEnumeration> {
        reflect_dom_object(
            cx,
            Box::new(SVGAnimatedEnumeration::new_inherited(
                element,
                attribute,
                enumeration,
                initial,
            )),
            &*element.owner_window(),
        )
    }
}

impl SVGAnimatedEnumerationMethods<crate::DomTypeHolder> for SVGAnimatedEnumeration {
    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedEnumeration__baseVal>
    fn BaseVal(&self) -> u16 {
        attribute_value(&self.element, &self.attribute)
            .and_then(|value| {
                self.enumeration
                    .keywords()
                    .iter()
                    .position(|keyword| *keyword == value)
            })
            .map_or(self.initial, |index| index as u16 + 1)
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedEnumeration__baseVal>
    fn SetBaseVal(&self, cx: &mut JSContext, value: u16) -> ErrorResult {
        // A value of 0 (UNKNOWN) or one that names no keyword of the reflected attribute
        // throws a TypeError; otherwise the attribute is set to that keyword.
        let Some(keyword) = usize::from(value)
            .checked_sub(1)
            .and_then(|index| self.enumeration.keywords().get(index))
        else {
            return Err(Error::Type(
                c"Not a valid value for this enumeration".to_owned(),
            ));
        };
        set_attribute_value(cx, &self.element, &self.attribute, keyword.to_string());
        Ok(())
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGAnimatedEnumeration__animVal>
    fn AnimVal(&self) -> u16 {
        // Without SMIL animation the animated value is the base value.
        self.BaseVal()
    }
}
