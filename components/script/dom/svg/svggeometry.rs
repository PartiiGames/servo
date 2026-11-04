/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii: geometry of SVG elements inside an inline `<svg>`.
//!
//! Servo renders an inline `<svg>` as one replaced box (its serialized source drawn as a vector
//! image), so the elements inside it have no layout boxes and every box query answers nothing
//! for them. This computes their client rects from their attributes instead: the outer `<svg>`'s
//! content box, `viewBox` / `preserveAspectRatio`, nested `<svg>` viewports, `transform` (the CSS
//! property and the attribute), shape geometry and stroke. It also gives the clip that applies to
//! them (the outer and nested `<svg>` viewports, and `clip-path="url(#…)"` boxes), for
//! IntersectionObserver. Used by `getBoundingClientRect()` and IntersectionObserver.
//!
//! Supported: rect, image, foreignObject, use (x/y/width/height), svg, circle, ellipse, line,
//! polyline, polygon, path (control-point hull), and containers (g, a, switch, svg) as the union
//! of their children. Text has no geometry here (None).

use app_units::Au;
use euclid::default::{Box2D, Point2D, Transform2D};
use euclid::{Rect, Size2D};
use html5ever::{LocalName, ns};
use layout_api::BoxAreaType;
use style_traits::CSSPixel;

use crate::dom::bindings::codegen::Bindings::NodeBinding::NodeMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::DomRoot;
use crate::dom::element::Element;
use crate::dom::iterators::ShadowIncluding;
use crate::dom::node::{Node, NodeTraits};

/// An SVG element's geometry in client (viewport) coordinates.
pub(crate) struct SvgGeometry {
    /// The element's bounding box (with stroke), in client coordinates.
    pub(crate) rect: Rect<Au, CSSPixel>,
    /// The clip that applies to the element (viewports, clip paths), in client coordinates.
    /// `None` when it is clipped away entirely.
    pub(crate) clip: Option<Rect<Au, CSSPixel>>,
    /// The outer `<svg>` element (the one with a layout box).
    pub(crate) outer_svg: DomRoot<Element>,
}

fn is_svg(element: &Element) -> bool {
    *element.namespace() == ns!(svg)
}

fn attr(element: &Element, name: &str) -> Option<String> {
    element.get_attribute_string_value_with_namespace(&ns!(), &LocalName::from(name))
}

/// Which viewport dimension a percentage refers to.
#[derive(Clone, Copy)]
enum Axis {
    X,
    Y,
    Diagonal,
}

/// The context lengths resolve in: the nearest viewport (CSS px in user units) and the window.
#[derive(Clone, Copy)]
struct LengthContext {
    viewport: (f64, f64),
    window: (f64, f64),
    font_size: f64,
}

impl LengthContext {
    fn resolve(&self, value: &str, axis: Axis) -> Option<f64> {
        let value = value.trim();
        let split = value
            .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E'))
            .unwrap_or(value.len());
        // An exponent 'e' followed by a unit letter ("1em") is a unit, not an exponent.
        let (number, unit) = if value[..split].ends_with(['e', 'E']) {
            (&value[..split - 1], &value[split - 1..])
        } else {
            (&value[..split], &value[split..])
        };
        let number: f64 = number.parse().ok()?;
        let (vw, vh) = self.viewport;
        let px = match unit.trim().to_ascii_lowercase().as_str() {
            "" | "px" => number,
            "%" => {
                let basis = match axis {
                    Axis::X => vw,
                    Axis::Y => vh,
                    Axis::Diagonal => ((vw * vw + vh * vh) / 2.0).sqrt(),
                };
                number * basis / 100.0
            },
            "em" | "rem" => number * self.font_size,
            "ex" => number * self.font_size / 2.0,
            "vw" => number * self.window.0 / 100.0,
            "vh" => number * self.window.1 / 100.0,
            "vmin" => number * self.window.0.min(self.window.1) / 100.0,
            "vmax" => number * self.window.0.max(self.window.1) / 100.0,
            "in" => number * 96.0,
            "cm" => number * 96.0 / 2.54,
            "mm" => number * 96.0 / 25.4,
            "q" => number * 96.0 / 101.6,
            "pt" => number * 96.0 / 72.0,
            "pc" => number * 16.0,
            _ => return None,
        };
        px.is_finite().then_some(px)
    }

    fn length(&self, element: &Element, name: &str, axis: Axis, default: f64) -> f64 {
        attr(element, name)
            .and_then(|value| self.resolve(&value, axis))
            .unwrap_or(default)
    }
}

fn numbers(value: &str) -> Vec<f64> {
    let mut out = Vec::new();
    let mut current = String::new();
    let flush = |current: &mut String, out: &mut Vec<f64>| {
        if let Ok(n) = current.parse::<f64>() {
            out.push(n);
        }
        current.clear();
    };
    let mut prev = ' ';
    for c in value.chars() {
        let starts_new = (c == '-' || c == '+') && !matches!(prev, 'e' | 'E') ||
            (c == '.' && current.contains('.') && !current.contains(['e', 'E']));
        if c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E' {
            if starts_new && !current.is_empty() {
                flush(&mut current, &mut out);
            }
            current.push(c);
        } else {
            flush(&mut current, &mut out);
        }
        prev = c;
    }
    flush(&mut current, &mut out);
    out
}

/// The transform from a viewport with this `viewBox` to the viewport's own (width, height) box.
fn view_box_transform(element: &Element, width: f64, height: f64) -> (Transform2D<f64>, (f64, f64)) {
    let Some(view_box) = attr(element, "viewBox").map(|value| numbers(&value)) else {
        return (Transform2D::identity(), (width, height));
    };
    let [min_x, min_y, vb_width, vb_height] = view_box[..] else {
        return (Transform2D::identity(), (width, height));
    };
    if vb_width <= 0.0 || vb_height <= 0.0 {
        return (Transform2D::identity(), (width, height));
    }
    let aspect = attr(element, "preserveAspectRatio").unwrap_or_default();
    let mut words = aspect.split_whitespace();
    let align = words.next().unwrap_or("xMidYMid");
    let slice = words.next() == Some("slice");
    let (sx, sy) = (width / vb_width, height / vb_height);
    let (sx, sy) = if align == "none" {
        (sx, sy)
    } else {
        let s = if slice { sx.max(sy) } else { sx.min(sy) };
        (s, s)
    };
    let (mut tx, mut ty) = (-min_x * sx, -min_y * sy);
    if align != "none" {
        let extra_x = width - vb_width * sx;
        let extra_y = height - vb_height * sy;
        if align.contains("xMid") {
            tx += extra_x / 2.0;
        } else if align.contains("xMax") {
            tx += extra_x;
        }
        if align.contains("YMid") {
            ty += extra_y / 2.0;
        } else if align.contains("YMax") {
            ty += extra_y;
        }
    }
    (
        Transform2D::scale(sx, sy).then_translate(euclid::default::Vector2D::new(tx, ty)),
        (vb_width, vb_height),
    )
}

/// A transform list (SVG `transform` attribute syntax, which also covers the CSS functions used
/// with px and deg units).
fn parse_transform_list(value: &str) -> Transform2D<f64> {
    let mut result = Transform2D::identity();
    for item in value.split(')') {
        let Some((name, args)) = item.split_once('(') else {
            continue;
        };
        let name = name.trim_matches(|c: char| c.is_whitespace() || c == ',');
        let a = numbers(args);
        let get = |i: usize, default: f64| a.get(i).copied().unwrap_or(default);
        let t = match name {
            "translate" => Transform2D::translation(get(0, 0.0), get(1, 0.0)),
            "translateX" => Transform2D::translation(get(0, 0.0), 0.0),
            "translateY" => Transform2D::translation(0.0, get(0, 0.0)),
            "scale" => Transform2D::scale(get(0, 1.0), get(1, get(0, 1.0))),
            "scaleX" => Transform2D::scale(get(0, 1.0), 1.0),
            "scaleY" => Transform2D::scale(1.0, get(0, 1.0)),
            "rotate" => {
                let angle = euclid::Angle::degrees(get(0, 0.0));
                let (cx, cy) = (get(1, 0.0), get(2, 0.0));
                Transform2D::translation(-cx, -cy)
                    .then_rotate(angle)
                    .then_translate(euclid::default::Vector2D::new(cx, cy))
            },
            "skewX" => Transform2D::new(1.0, 0.0, get(0, 0.0).to_radians().tan(), 1.0, 0.0, 0.0),
            "skewY" => Transform2D::new(1.0, get(0, 0.0).to_radians().tan(), 0.0, 1.0, 0.0, 0.0),
            "matrix" if a.len() == 6 => Transform2D::new(a[0], a[1], a[2], a[3], a[4], a[5]),
            _ => continue,
        };
        // Items apply right to left: the rightmost is applied to the element first.
        result = t.then(&result);
    }
    result
}

/// The value of a property in the element's `style` attribute.
fn inline_style(element: &Element, property: &str) -> Option<String> {
    let style = attr(element, "style")?;
    style.split(';').find_map(|declaration| {
        let (name, value) = declaration.split_once(':')?;
        (name.trim().eq_ignore_ascii_case(property)).then(|| value.trim().to_owned())
    })
}

/// The element's own transform, from its user space to its parent's. The CSS `transform`
/// property wins over the `transform` attribute (SVG 2). Servo hides SVG children
/// (`svg > * { display: none }`), so deeper ones have no computed style: for those the inline
/// style is read directly.
fn own_transform(element: &Element) -> Transform2D<f64> {
    let css = match element.style_without_reflow() {
        Some(style) => {
            let transform = &style.get_box().transform;
            if transform.0.is_empty() {
                None
            } else {
                transform.to_transform_3d_matrix(None).ok().map(|(m, _)| {
                    Transform2D::new(
                        m.m11 as f64,
                        m.m12 as f64,
                        m.m21 as f64,
                        m.m22 as f64,
                        m.m41 as f64,
                        m.m42 as f64,
                    )
                })
            }
        },
        None => inline_style(element, "transform")
            .filter(|value| value != "none" && !value.is_empty())
            .map(|value| parse_transform_list(&value)),
    };
    css.unwrap_or_else(|| {
        attr(element, "transform")
            .map(|value| parse_transform_list(&value))
            .unwrap_or_else(Transform2D::identity)
    })
}

fn element_children(element: &Element) -> impl Iterator<Item = DomRoot<Element>> + '_ {
    element
        .upcast::<Node>()
        .children()
        .filter_map(DomRoot::downcast::<Element>)
}

/// `display: none` set on the element itself. (Its computed style can't tell: Servo's UA sheet
/// hides every child of an `<svg>`.)
fn is_hidden(element: &Element) -> bool {
    attr(element, "display").is_some_and(|value| value.trim() == "none") ||
        inline_style(element, "display").is_some_and(|value| value == "none")
}

fn union(a: Option<Box2D<f64>>, b: Option<Box2D<f64>>) -> Option<Box2D<f64>> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.union(&b)),
        (a, b) => a.or(b),
    }
}

fn points_box(points: &[f64]) -> Option<Box2D<f64>> {
    let pts: Vec<Point2D<f64>> = points
        .chunks_exact(2)
        .map(|p| Point2D::new(p[0], p[1]))
        .collect();
    (!pts.is_empty()).then(|| Box2D::from_points(pts))
}

/// The points of a path's control polygon (a superset of the path's extent).
fn path_box(d: &str) -> Option<Box2D<f64>> {
    let mut pts = Vec::new();
    let (mut x, mut y, mut start) = (0.0, 0.0, (0.0, 0.0));
    let mut command = 'M';
    let mut tokens: Vec<(char, Vec<f64>)> = Vec::new();
    let mut current = String::new();
    for c in d.chars() {
        if c.is_ascii_alphabetic() && c != 'e' && c != 'E' {
            tokens.push((command, numbers(&current)));
            current.clear();
            command = c;
        } else {
            current.push(c);
        }
    }
    tokens.push((command, numbers(&current)));
    for (command, args) in tokens {
        let relative = command.is_ascii_lowercase();
        let (ox, oy) = if relative { (x, y) } else { (0.0, 0.0) };
        match command.to_ascii_uppercase() {
            'M' | 'L' | 'T' => {
                for (i, p) in args.chunks_exact(2).enumerate() {
                    let (ox, oy) = if relative && i > 0 { (x, y) } else { (ox, oy) };
                    x = ox + p[0];
                    y = oy + p[1];
                    if i == 0 && command.eq_ignore_ascii_case(&'M') {
                        start = (x, y);
                    }
                    pts.push(Point2D::new(x, y));
                }
            },
            'H' => {
                for v in args {
                    x = if relative { x + v } else { v };
                    pts.push(Point2D::new(x, y));
                }
            },
            'V' => {
                for v in args {
                    y = if relative { y + v } else { v };
                    pts.push(Point2D::new(x, y));
                }
            },
            'C' | 'S' | 'Q' => {
                let n = match command.to_ascii_uppercase() {
                    'C' => 6,
                    _ => 4,
                };
                for seg in args.chunks_exact(n) {
                    let (bx, by) = if relative { (x, y) } else { (0.0, 0.0) };
                    for p in seg.chunks_exact(2) {
                        pts.push(Point2D::new(bx + p[0], by + p[1]));
                    }
                    x = bx + seg[n - 2];
                    y = by + seg[n - 1];
                }
            },
            'A' => {
                for seg in args.chunks_exact(7) {
                    let (bx, by) = if relative { (x, y) } else { (0.0, 0.0) };
                    let (rx, ry) = (seg[0].abs(), seg[1].abs());
                    let (nx, ny) = (bx + seg[5], by + seg[6]);
                    // The arc stays within its radii of both endpoints.
                    for (px, py) in [(x, y), (nx, ny)] {
                        pts.push(Point2D::new(px - rx, py - ry));
                        pts.push(Point2D::new(px + rx, py + ry));
                    }
                    x = nx;
                    y = ny;
                }
            },
            'Z' => {
                (x, y) = start;
            },
            _ => {},
        }
    }
    (!pts.is_empty()).then(|| Box2D::from_points(pts))
}

fn stroke_inflation(element: &Element, lengths: &LengthContext) -> f64 {
    let stroke = attr(element, "stroke");
    if stroke.is_none_or(|stroke| stroke.trim() == "none") {
        return 0.0;
    }
    lengths.length(element, "stroke-width", Axis::Diagonal, 1.0).max(0.0) / 2.0
}

/// The element's bounding box in its own user space (before its own transform).
fn local_box(element: &Element, lengths: &LengthContext) -> Option<Box2D<f64>> {
    let rect = |x: f64, y: f64, w: f64, h: f64| {
        (w >= 0.0 && h >= 0.0).then(|| Box2D::new(Point2D::new(x, y), Point2D::new(x + w, y + h)))
    };
    let name = element.local_name();
    let geometry = match &**name {
        "rect" | "image" | "foreignObject" => rect(
            lengths.length(element, "x", Axis::X, 0.0),
            lengths.length(element, "y", Axis::Y, 0.0),
            lengths.length(element, "width", Axis::X, 0.0),
            lengths.length(element, "height", Axis::Y, 0.0),
        ),
        "svg" => {
            // A nested viewport: its viewport rectangle in the parent's user space.
            rect(
                lengths.length(element, "x", Axis::X, 0.0),
                lengths.length(element, "y", Axis::Y, 0.0),
                lengths.length(element, "width", Axis::X, lengths.viewport.0),
                lengths.length(element, "height", Axis::Y, lengths.viewport.1),
            )
        },
        "circle" => {
            let (cx, cy) = (
                lengths.length(element, "cx", Axis::X, 0.0),
                lengths.length(element, "cy", Axis::Y, 0.0),
            );
            let r = lengths.length(element, "r", Axis::Diagonal, 0.0);
            rect(cx - r, cy - r, 2.0 * r, 2.0 * r)
        },
        "ellipse" => {
            let (cx, cy) = (
                lengths.length(element, "cx", Axis::X, 0.0),
                lengths.length(element, "cy", Axis::Y, 0.0),
            );
            let (rx, ry) = (
                lengths.length(element, "rx", Axis::X, 0.0),
                lengths.length(element, "ry", Axis::Y, 0.0),
            );
            rect(cx - rx, cy - ry, 2.0 * rx, 2.0 * ry)
        },
        "line" => points_box(&[
            lengths.length(element, "x1", Axis::X, 0.0),
            lengths.length(element, "y1", Axis::Y, 0.0),
            lengths.length(element, "x2", Axis::X, 0.0),
            lengths.length(element, "y2", Axis::Y, 0.0),
        ]),
        "polyline" | "polygon" => attr(element, "points").and_then(|p| points_box(&numbers(&p))),
        "path" => attr(element, "d").and_then(|d| path_box(&d)),
        "g" | "a" | "switch" => children_box(element, lengths),
        _ => None,
    }?;
    let inflate = match &**name {
        "rect" | "circle" | "ellipse" | "line" | "polyline" | "polygon" | "path" => {
            stroke_inflation(element, lengths)
        },
        _ => 0.0,
    };
    Some(geometry.inflate(inflate, inflate))
}

/// A container's children's boxes, each through its own transform, in the container's space.
fn children_box(element: &Element, lengths: &LengthContext) -> Option<Box2D<f64>> {
    let mut result = None;
    for child in element_children(element) {
        if !is_svg(&child) || is_hidden(&child) {
            continue;
        }
        if matches!(
            &**child.local_name(),
            "defs" | "clipPath" | "mask" | "marker" | "pattern" | "symbol" | "linearGradient" |
                "radialGradient" | "filter" | "style" | "script" | "title" | "desc" | "metadata"
        ) {
            continue;
        }
        let child_box = local_box(&child, lengths)
            .map(|b| own_transform(&child).outer_transformed_box(&b));
        result = union(result, child_box);
    }
    result
}

fn to_au_rect(b: Box2D<f64>) -> Rect<Au, CSSPixel> {
    Rect::new(
        euclid::Point2D::new(Au::from_f64_px(b.min.x), Au::from_f64_px(b.min.y)),
        Size2D::new(
            Au::from_f64_px(b.width().max(0.0)),
            Au::from_f64_px(b.height().max(0.0)),
        ),
    )
}

fn intersect(a: Option<Box2D<f64>>, b: Box2D<f64>) -> Option<Box2D<f64>> {
    let a = a?;
    let r = a.intersection_unchecked(&b);
    (!r.is_negative()).then_some(r)
}

/// The box of the `clipPath` an element refers to with `clip-path="url(#id)"`, in the element's
/// user space; `Err(())` when there's no usable reference (no clip).
fn clip_path_box(
    element: &Element,
    outer_svg: &Element,
    lengths: &LengthContext,
) -> Result<Option<Box2D<f64>>, ()> {
    let value = attr(element, "clip-path").ok_or(())?;
    let id = value
        .trim()
        .strip_prefix("url(")
        .and_then(|v| v.strip_suffix(')'))
        .map(|v| v.trim().trim_matches(['"', '\'']))
        .and_then(|v| v.strip_prefix('#'))
        .ok_or(())?;
    let clip = outer_svg
        .upcast::<Node>()
        .traverse_preorder(ShadowIncluding::No)
        .filter_map(DomRoot::downcast::<Element>)
        .find(|e| &**e.local_name() == "clipPath" && e.get_id().is_some_and(|i| &*i == id))
        .ok_or(())?;
    let in_clip_space = children_box(&clip, lengths).map(|b| own_transform(&clip).outer_transformed_box(&b));
    if attr(&clip, "clipPathUnits").is_some_and(|u| u.trim() == "objectBoundingBox") {
        // Unit square = the referencing element's bounding box.
        let Some(object) = local_box(element, lengths) else {
            return Ok(None);
        };
        let to_object = Transform2D::scale(object.width(), object.height())
            .then_translate(object.min.to_vector());
        return Ok(in_clip_space.map(|b| to_object.outer_transformed_box(&b)));
    }
    Ok(in_clip_space)
}

/// The geometry of `element`, an SVG element inside an inline `<svg>` (not the outer `<svg>`
/// itself, which has a layout box). `reflow`: whether the outer box query may reflow.
pub(crate) fn svg_geometry(element: &Element, reflow: bool) -> Option<SvgGeometry> {
    if !is_svg(element) {
        return None;
    }
    // The chain of SVG ancestors, from the outer <svg> down to the element.
    let mut chain = vec![DomRoot::from_ref(element)];
    loop {
        let parent = chain.last().unwrap().upcast::<Node>().GetParentElement();
        match parent {
            Some(parent) if is_svg(&parent) => chain.push(parent),
            _ => break,
        }
    }
    chain.reverse();
    let outer = chain[0].clone();
    if chain.len() < 2 || &**outer.local_name() != "svg" {
        return None;
    }
    if chain.iter().any(|e| is_hidden(e)) {
        return None;
    }

    let window = element.owner_window();
    let content = if reflow {
        outer.upcast::<Node>().content_box()
    } else {
        window.box_area_query_without_reflow(outer.upcast(), BoxAreaType::Content, false)
    }?;
    let size = window.viewport_details().size;
    let font_size = outer
        .style_without_reflow()
        .map(|style| style.get_font().font_size.computed_size().px() as f64)
        .unwrap_or(16.0);

    let (width, height) = (
        content.size.width.to_f64_px(),
        content.size.height.to_f64_px(),
    );
    let (view_box, viewport) = view_box_transform(&outer, width, height);
    let mut to_client = view_box.then_translate(euclid::default::Vector2D::new(
        content.origin.x.to_f64_px(),
        content.origin.y.to_f64_px(),
    ));
    let mut lengths = LengthContext {
        viewport,
        window: (size.width as f64, size.height as f64),
        font_size,
    };
    let content_box = Box2D::new(
        Point2D::new(content.origin.x.to_f64_px(), content.origin.y.to_f64_px()),
        Point2D::new(
            content.origin.x.to_f64_px() + width,
            content.origin.y.to_f64_px() + height,
        ),
    );
    let mut clip = Some(content_box);

    // Walk down to the element's parent, composing user space -> client transforms.
    for ancestor in &chain[1..chain.len() - 1] {
        if &**ancestor.local_name() == "svg" {
            let viewport_rect = local_box(ancestor, &lengths)?;
            let to_parent_client = own_transform(ancestor).then(&to_client);
            clip = intersect(clip, to_parent_client.outer_transformed_box(&viewport_rect));
            let (inner, inner_viewport) =
                view_box_transform(ancestor, viewport_rect.width(), viewport_rect.height());
            to_client = inner
                .then_translate(viewport_rect.min.to_vector())
                .then(&to_parent_client);
            lengths.viewport = inner_viewport;
        } else {
            to_client = own_transform(ancestor).then(&to_client);
        }
        if let Ok(clip_box) = clip_path_box(ancestor, &outer, &lengths) {
            clip = clip_box.and_then(|b| intersect(clip, to_client.outer_transformed_box(&b)));
        }
    }

    let to_client = own_transform(element).then(&to_client);
    let local = local_box(element, &lengths)?;
    if let Ok(clip_box) = clip_path_box(element, &outer, &lengths) {
        clip = clip_box.and_then(|b| intersect(clip, to_client.outer_transformed_box(&b)));
    }
    Some(SvgGeometry {
        rect: to_au_rect(to_client.outer_transformed_box(&local)),
        clip: clip.map(to_au_rect),
        outer_svg: outer,
    })
}
