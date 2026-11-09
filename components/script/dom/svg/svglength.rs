/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::fmt;

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object};

use crate::dom::bindings::codegen::Bindings::SVGLengthBinding::SVGLengthMethods;
use crate::dom::bindings::error::{Error, ErrorResult};
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::svg::svganimatedlength::SVGAnimatedLength;
use crate::dom::svg::svgreflectedattribute::parse_number_prefix;
use crate::dom::window::Window;

/// The unit of an SVG length; the discriminants are the `SVG_LENGTHTYPE_*` constants.
///
/// <https://svgwg.org/svg2-draft/types.html#InterfaceSVGLength>
#[derive(Clone, Copy, Debug, JSTraceable, MallocSizeOf, PartialEq)]
pub(crate) enum SVGLengthUnit {
    Number = 1,
    Percentage = 2,
    Ems = 3,
    Exs = 4,
    Px = 5,
    Cm = 6,
    Mm = 7,
    In = 8,
    Pt = 9,
    Pc = 10,
}

impl SVGLengthUnit {
    const ALL: [SVGLengthUnit; 10] = [
        SVGLengthUnit::Number,
        SVGLengthUnit::Percentage,
        SVGLengthUnit::Ems,
        SVGLengthUnit::Exs,
        SVGLengthUnit::Px,
        SVGLengthUnit::Cm,
        SVGLengthUnit::Mm,
        SVGLengthUnit::In,
        SVGLengthUnit::Pt,
        SVGLengthUnit::Pc,
    ];

    /// The unit for an `SVG_LENGTHTYPE_*` value, or `None` for `SVG_LENGTHTYPE_UNKNOWN`
    /// and values that name no unit.
    fn from_unit_type(unit_type: u16) -> Option<SVGLengthUnit> {
        Self::ALL.into_iter().find(|unit| *unit as u16 == unit_type)
    }

    fn suffix(self) -> &'static str {
        match self {
            SVGLengthUnit::Number => "",
            SVGLengthUnit::Percentage => "%",
            SVGLengthUnit::Ems => "em",
            SVGLengthUnit::Exs => "ex",
            SVGLengthUnit::Px => "px",
            SVGLengthUnit::Cm => "cm",
            SVGLengthUnit::Mm => "mm",
            SVGLengthUnit::In => "in",
            SVGLengthUnit::Pt => "pt",
            SVGLengthUnit::Pc => "pc",
        }
    }

    /// The unit with this suffix. CSS units are ASCII case-insensitive.
    fn from_suffix(suffix: &str) -> Option<SVGLengthUnit> {
        Self::ALL
            .into_iter()
            .find(|unit| unit.suffix().eq_ignore_ascii_case(suffix))
    }

    /// The number of user units (CSS px) in one of this unit.
    ///
    /// Absolute units are exact. The relative units need a layout context that these
    /// reflected attribute values do not have, so they use fixed values: `em` is the
    /// initial `font-size: medium` (16px), `ex` is half of that (8px), and percentages
    /// resolve against a 100 user unit reference length (1% is one user unit).
    fn user_units(self) -> f32 {
        match self {
            SVGLengthUnit::Number | SVGLengthUnit::Px => 1.,
            SVGLengthUnit::Percentage => 1.,
            SVGLengthUnit::Ems => 16.,
            SVGLengthUnit::Exs => 8.,
            SVGLengthUnit::Cm => 96. / 2.54,
            SVGLengthUnit::Mm => 96. / 25.4,
            SVGLengthUnit::In => 96.,
            SVGLengthUnit::Pt => 96. / 72.,
            SVGLengthUnit::Pc => 16.,
        }
    }
}

/// A number with a unit, as held by a `<length>` or `<length-percentage>` attribute.
#[derive(Clone, Copy, Debug, JSTraceable, MallocSizeOf, PartialEq)]
pub(crate) struct SVGLengthValue {
    value: f32,
    unit: SVGLengthUnit,
}

impl SVGLengthValue {
    pub(crate) const fn new(value: f32, unit: SVGLengthUnit) -> SVGLengthValue {
        SVGLengthValue { value, unit }
    }

    /// Parses `<number> <unit>?`, allowing surrounding whitespace.
    pub(crate) fn parse(input: &str) -> Option<SVGLengthValue> {
        let input = input.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0C'));
        let (value, suffix) = parse_number_prefix(input)?;
        let unit = SVGLengthUnit::from_suffix(suffix)?;
        Some(SVGLengthValue { value, unit })
    }

    fn in_user_units(self) -> f32 {
        self.value * self.unit.user_units()
    }

    /// The same length expressed in `unit`.
    fn converted_to(self, unit: SVGLengthUnit) -> SVGLengthValue {
        SVGLengthValue {
            value: self.in_user_units() / unit.user_units(),
            unit,
        }
    }
}

impl fmt::Display for SVGLengthValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}{}", self.value, self.unit.suffix())
    }
}

/// Converts a computed value for the bindings. Values can overflow `f32` after a unit
/// conversion; those saturate rather than becoming infinite.
fn finite(value: f32) -> Finite<f32> {
    Finite::wrap(if value.is_nan() {
        0.
    } else {
        value.clamp(f32::MIN, f32::MAX)
    })
}

/// <https://svgwg.org/svg2-draft/types.html#InterfaceSVGLength>
///
/// Every `SVGLength` here is the `baseVal` or `animVal` of an `SVGAnimatedLength`: it
/// reflects that object's attribute, and writes go back to the attribute.
#[dom_struct]
pub(crate) struct SVGLength {
    reflector_: Reflector,
    /// The animated length whose attribute this length reflects.
    animated_length: Dom<SVGAnimatedLength>,
    /// Whether this is an `animVal`, which cannot be modified.
    read_only: bool,
}

impl SVGLength {
    fn new_inherited(animated_length: &SVGAnimatedLength, read_only: bool) -> SVGLength {
        SVGLength {
            reflector_: Reflector::new(),
            animated_length: Dom::from_ref(animated_length),
            read_only,
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        animated_length: &SVGAnimatedLength,
        read_only: bool,
    ) -> DomRoot<SVGLength> {
        reflect_dom_object(
            cx,
            Box::new(SVGLength::new_inherited(animated_length, read_only)),
            window,
        )
    }

    fn length(&self) -> SVGLengthValue {
        self.animated_length.base_value()
    }

    /// Writes `length` to the reflected attribute, unless this length is read only.
    fn set_length(&self, cx: &mut JSContext, length: SVGLengthValue) -> ErrorResult {
        if self.read_only {
            return Err(Error::NoModificationAllowed(None));
        }
        self.animated_length.set_base_value(cx, length);
        Ok(())
    }
}

impl SVGLengthMethods<crate::DomTypeHolder> for SVGLength {
    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGLength__unitType>
    fn UnitType(&self) -> u16 {
        self.length().unit as u16
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGLength__value>
    fn Value(&self) -> Finite<f32> {
        finite(self.length().in_user_units())
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGLength__value>
    fn SetValue(&self, cx: &mut JSContext, value: Finite<f32>) -> ErrorResult {
        // Keep the current unit and store the value converted into it.
        let unit = self.length().unit;
        let value = SVGLengthValue::new(*value, SVGLengthUnit::Number).converted_to(unit);
        self.set_length(cx, value)
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGLength__valueInSpecifiedUnits>
    fn ValueInSpecifiedUnits(&self) -> Finite<f32> {
        finite(self.length().value)
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGLength__valueInSpecifiedUnits>
    fn SetValueInSpecifiedUnits(&self, cx: &mut JSContext, value: Finite<f32>) -> ErrorResult {
        let unit = self.length().unit;
        self.set_length(cx, SVGLengthValue::new(*value, unit))
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGLength__valueAsString>
    fn ValueAsString(&self) -> DOMString {
        self.length().to_string().into()
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGLength__valueAsString>
    fn SetValueAsString(&self, cx: &mut JSContext, value: DOMString) -> ErrorResult {
        if self.read_only {
            return Err(Error::NoModificationAllowed(None));
        }
        let Some(length) = SVGLengthValue::parse(&value.str()) else {
            return Err(Error::Syntax(None));
        };
        self.set_length(cx, length)
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGLength__newValueSpecifiedUnits>
    fn NewValueSpecifiedUnits(
        &self,
        cx: &mut JSContext,
        unit_type: u16,
        value: Finite<f32>,
    ) -> ErrorResult {
        if self.read_only {
            return Err(Error::NoModificationAllowed(None));
        }
        let Some(unit) = SVGLengthUnit::from_unit_type(unit_type) else {
            return Err(Error::NotSupported(None));
        };
        self.set_length(cx, SVGLengthValue::new(*value, unit))
    }

    /// <https://svgwg.org/svg2-draft/types.html#__svg__SVGLength__convertToSpecifiedUnits>
    fn ConvertToSpecifiedUnits(&self, cx: &mut JSContext, unit_type: u16) -> ErrorResult {
        if self.read_only {
            return Err(Error::NoModificationAllowed(None));
        }
        let Some(unit) = SVGLengthUnit::from_unit_type(unit_type) else {
            return Err(Error::NotSupported(None));
        };
        self.set_length(cx, self.length().converted_to(unit))
    }
}
