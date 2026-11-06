/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii: the `font` attribute of a 2D context with no canvas element (an OffscreenCanvas
//! without a placeholder, in a window or a worker).
//!
//! <https://html.spec.whatwg.org/multipage/#dom-context-2d-font>: the value is parsed as the CSS
//! `font` shorthand; with no element, relative values are computed against the default font,
//! 10px sans-serif. Without this, `ctx.font = ...` was ignored on such contexts and every
//! OffscreenCanvas drew text in the default font (no web fonts, no sizes).

use std::cell::OnceCell;
use std::fmt;

use cssparser::{Parser, ParserInput};
use euclid::{Scale, Size2D};
use servo_arc::Arc as ServoArc;
use style::context::QuirksMode;
use style::device::Device;
use style::device::servo::FontMetricsProvider;
use style::font_metrics::FontMetrics;
use style::media_queries::MediaType;
use style::properties::ComputedValues;
use style::properties::shorthands::font;
use style::properties::style_structs::Font;
use style::queries::values::PrefersColorScheme;
use style::servo::media_features::PointerCapabilities;
use style::stylesheets::{CssRuleType, UrlExtraData};
use style::values::computed::font::GenericFontFamily;
use style::values::computed::{
    CSSPixelLength, Context, FontSize, Length, NonNegativeLength, ToComputedValue,
};
use style::values::specified::font::{KeywordInfo, QueryFontMetricsFlags};
use style::values::specified::font as specified_font;
use style_traits::ParsingMode;

use crate::css::css::parser_context_for_anonymous_content;

/// The default font of a canvas context: 10px.
const CANVAS_DEFAULT_FONT_SIZE_PX: f32 = 10.0;

/// No font access here: keywords (medium, ...) use the usual default sizes; ex/ch/ic fall back
/// to stylo's defaults (fractions of the font size).
struct NoFontMetricsProvider;

impl fmt::Debug for NoFontMetricsProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NoFontMetricsProvider")
    }
}

impl FontMetricsProvider for NoFontMetricsProvider {
    fn query_font_metrics(
        &self,
        _vertical: bool,
        _font: &Font,
        _base_size: CSSPixelLength,
        _flags: QueryFontMetricsFlags,
    ) -> FontMetrics {
        FontMetrics::default()
    }

    fn base_size_for_generic(&self, generic: GenericFontFamily) -> Length {
        Length::new(match generic {
            GenericFontFamily::Monospace => 13.0,
            _ => 16.0,
        })
    }
}

fn default_font() -> Font {
    let mut font = Font::initial_values();
    font.font_size = FontSize {
        computed_size: NonNegativeLength::new(CANVAS_DEFAULT_FONT_SIZE_PX),
        used_size: NonNegativeLength::new(CANVAS_DEFAULT_FONT_SIZE_PX),
        keyword_info: KeywordInfo::none(),
    };
    font.compute_font_hash();
    font
}

thread_local! {
    /// One device per thread (script thread or worker), only for computing font values.
    static DEVICE: OnceCell<Device> = const { OnceCell::new() };
}

/// Parses `value` with the CSS font shorthand grammar alone, as the canvas `font` setter requires:
/// CSS-wide keywords (`inherit`, ...), `var()`, extra declarations and system fonts are syntax
/// errors here, so the setter keeps the old value. Used by element canvases too, whose layout
/// query would otherwise accept any declaration value.
pub(super) fn parse_canvas_font(value: &str, url: &UrlExtraData) -> Option<font::Longhands> {
    let context = parser_context_for_anonymous_content(CssRuleType::Style, ParsingMode::DEFAULT, url);
    let mut input = ParserInput::new(value);
    let mut parser = Parser::new(&mut input);
    let longhands = parser
        .parse_entirely(|parser| font::parse_value(&context, parser))
        .ok()?;
    let specified_font::FontFamily::Values(_) = longhands.font_family else {
        return None;
    };
    Some(longhands)
}

/// Parses `value` as the CSS font shorthand and computes it with no element. None on a syntax
/// error (the caller then keeps the old value).
pub(super) fn resolve_font_without_element(value: &str, url: &UrlExtraData) -> Option<ServoArc<Font>> {
    let longhands = parse_canvas_font(value, url)?;
    DEVICE.with(|cell| {
        let device = cell.get_or_init(|| {
            Device::new(
                MediaType::screen(),
                QuirksMode::NoQuirks,
                Size2D::new(300.0, 150.0),
                Size2D::new(300.0, 150.0),
                Scale::new(1.0),
                Box::new(NoFontMetricsProvider),
                ComputedValues::initial_values_with_font_override(default_font()),
                PrefersColorScheme::Light,
                PointerCapabilities::default(),
                PointerCapabilities::default(),
            )
        });
        Context::for_media_query_evaluation(device, QuirksMode::NoQuirks, |cx| {
            let mut font = default_font();
            font.font_family = longhands.font_family.to_computed_value(cx);
            font.font_size = longhands.font_size.to_computed_value(cx);
            font.font_style = longhands.font_style.to_computed_value(cx);
            font.font_weight = longhands.font_weight.to_computed_value(cx);
            font.font_width = longhands.font_width.to_computed_value(cx);
            font.font_variant_caps = longhands.font_variant_caps.to_computed_value(cx);
            // Font matching and the font cache key off this hash.
            font.compute_font_hash();
            Some(ServoArc::new(font))
        })
    })
}
