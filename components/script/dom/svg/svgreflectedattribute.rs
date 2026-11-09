/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Shared helpers for the `SVGAnimated*` interfaces. Each of those objects reflects one
//! content attribute of its element: reading `baseVal` parses the attribute (falling back
//! to the attribute's initial value when it is missing or invalid) and writing `baseVal`
//! serializes the new value back into the attribute. Servo does not implement SMIL
//! animation, so every `animVal` is equal to its `baseVal`.

use std::fmt::Display;

use html5ever::{LocalName, ns};
use js::context::JSContext;

use crate::dom::bindings::str::DOMString;
use crate::dom::element::Element;

/// Returns the value of the null-namespace attribute `name` of `element`, if present.
///
/// SVG attribute names may be mixed case (`stdDeviation`), so this does not go through the
/// lowercase-only helpers.
pub(crate) fn attribute_value(element: &Element, name: &LocalName) -> Option<String> {
    element.get_attribute_string_value_with_namespace(&ns!(), name)
}

/// Sets the null-namespace attribute `name` of `element` to `value`.
pub(crate) fn set_attribute_value(
    cx: &mut JSContext,
    element: &Element,
    name: &LocalName,
    value: String,
) {
    let value = element.parse_attribute(&ns!(), name, DOMString::from(value));
    element.set_attribute_with_namespace(cx, name.clone(), value, name.clone(), ns!(), None);
}

/// <https://infra.spec.whatwg.org/#ascii-whitespace>, which is SVG's `wsp`.
fn is_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0C')
}

/// Splits off the longest prefix of `input` that is an SVG `<number>`
/// (`[+-]? ( [0-9]+ ( "." [0-9]+ )? | "." [0-9]+ ) ( [eE] [+-]? [0-9]+ )?`) and returns its
/// value together with the rest of the input. Values that do not fit in an `f32` are
/// rejected.
///
/// <https://svgwg.org/svg2-draft/types.html#syntax>
pub(crate) fn parse_number_prefix(input: &str) -> Option<(f32, &str)> {
    let bytes = input.as_bytes();
    let digits_from = |mut index: usize| {
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        index
    };

    let mut end = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let integer_end = digits_from(end);
    let mut has_digits = integer_end > end;
    end = integer_end;
    if bytes.get(end) == Some(&b'.') {
        let fraction_end = digits_from(end + 1);
        if fraction_end > end + 1 {
            has_digits = true;
            end = fraction_end;
        }
    }
    if !has_digits {
        return None;
    }
    // An exponent only counts when it has digits, so that "1em" and "1ex" keep their unit.
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        let sign_end = end + 1 + usize::from(matches!(bytes.get(end + 1), Some(b'+' | b'-')));
        let exponent_end = digits_from(sign_end);
        if exponent_end > sign_end {
            end = exponent_end;
        }
    }

    let value = input[..end].parse::<f64>().ok()? as f32;
    value.is_finite().then_some((value, &input[end..]))
}

/// Parses an SVG `<number>`, allowing surrounding whitespace.
pub(crate) fn parse_number(input: &str) -> Option<f32> {
    match parse_number_prefix(input.trim_matches(is_whitespace))? {
        (value, "") => Some(value),
        _ => None,
    }
}

/// Parses an SVG `<integer>` (`[+-]? [0-9]+`), allowing surrounding whitespace.
pub(crate) fn parse_integer(input: &str) -> Option<i32> {
    let input = input.trim_matches(is_whitespace);
    let digits = input.strip_prefix(['+', '-']).unwrap_or(input);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    input.parse().ok()
}

/// Splits an "x [y]" attribute value (as used by `<number-optional-number>` and
/// `<integer-optional-integer>`) at its `comma-wsp` separator.
fn split_optional_pair(input: &str) -> Option<(&str, Option<&str>)> {
    let input = input.trim_matches(is_whitespace);
    let Some(separator) = input.find(|c| is_whitespace(c) || c == ',') else {
        return (!input.is_empty()).then_some((input, None));
    };
    let (first, rest) = input.split_at(separator);
    let rest = rest.trim_start_matches(is_whitespace);
    let second = rest
        .strip_prefix(',')
        .unwrap_or(rest)
        .trim_start_matches(is_whitespace);
    if first.is_empty() || second.is_empty() || second.contains(|c| is_whitespace(c) || c == ',') {
        return None;
    }
    Some((first, Some(second)))
}

/// Which part of its attribute an `SVGAnimatedNumber` or `SVGAnimatedInteger` reflects.
///
/// Some attributes hold an optional pair, "x [y]" with y defaulting to x (`stdDeviation`,
/// `order`, `radius`, `baseFrequency`, `kernelUnitLength`), and are exposed as two IDL
/// attributes such as `stdDeviationX` and `stdDeviationY`.
#[derive(Clone, Copy, JSTraceable, MallocSizeOf, PartialEq)]
pub(crate) enum AttributePart {
    /// The whole attribute is a single value.
    Whole,
    /// The first number of an "x [y]" pair.
    First,
    /// The second number of an "x [y]" pair, which defaults to the first.
    Second,
}

impl AttributePart {
    /// Reads this part of the attribute value `input`, or `None` if `input` is invalid.
    pub(crate) fn read<T: Copy>(self, input: &str, parse: impl Fn(&str) -> Option<T>) -> Option<T> {
        if self == AttributePart::Whole {
            return parse(input);
        }
        let (first, second) = split_optional_pair(input)?;
        let first = parse(first)?;
        let second = match second {
            Some(second) => parse(second)?,
            None => first,
        };
        Some(if self == AttributePart::First {
            first
        } else {
            second
        })
    }

    /// Returns the attribute value that results from setting this part of the attribute
    /// (whose current value is `current`) to `value`. The other half of a pair keeps its
    /// current value, or `initial` if the current attribute value is missing or invalid.
    pub(crate) fn write<T: Copy + Display>(
        self,
        current: Option<&str>,
        initial: T,
        value: T,
        parse: impl Fn(&str) -> Option<T>,
    ) -> String {
        let current_part = |part: AttributePart| {
            current
                .and_then(|current| part.read(current, &parse))
                .unwrap_or(initial)
        };
        match self {
            AttributePart::Whole => value.to_string(),
            AttributePart::First => format!("{} {}", value, current_part(AttributePart::Second)),
            AttributePart::Second => format!("{} {}", current_part(AttributePart::First), value),
        }
    }
}
