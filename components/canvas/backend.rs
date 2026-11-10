/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use euclid::default::{Point2D, Rect, Size2D, Transform2D};
use paint_api::SerializableImageData;
use pixels::Snapshot;
use profile_traits::mem::ReportKind;
use servo_canvas_traits::canvas::{
    CompositionOptions, FillOrStrokeStyle, FillRule, LineOptions, Path, ShadowOptions, TextRun,
};
use webrender_api::ImageDescriptor;

use crate::canvas_data::Filter;

pub(crate) struct CanvasStoreSizesPerType {
    pub name: &'static str,
    pub size: usize,
    pub kind: ReportKind,
}

// This defines required methods for a DrawTarget. The prototypes are derived from the now-removed
// Azure backend's methods.
pub(crate) trait GenericDrawTarget {
    type SourceSurface;

    fn new(size: Size2D<u32>) -> Self;
    fn create_similar_draw_target(&self, size: &Size2D<i32>) -> Self;
    fn canvas_store_sizes(
        &self,
        ops: &mut malloc_size_of::MallocSizeOfOps,
    ) -> Option<Vec<CanvasStoreSizesPerType>>;

    fn clear_rect(&mut self, rect: &Rect<f32>, transform: Transform2D<f64>);
    fn copy_surface(
        &mut self,
        surface: Self::SourceSurface,
        source: Rect<i32>,
        destination: Point2D<i32>,
    );
    fn create_source_surface_from_data(&self, data: Snapshot) -> Option<Self::SourceSurface>;
    fn draw_surface(
        &mut self,
        surface: Self::SourceSurface,
        dest: Rect<f64>,
        source: Rect<f64>,
        filter: Filter,
        composition_options: CompositionOptions,
        transform: Transform2D<f64>,
    );
    fn draw_surface_with_shadow(
        &mut self,
        surface: Self::SourceSurface,
        dest: &Point2D<f32>,
        shadow_options: &ShadowOptions,
        composition_options: CompositionOptions,
    );
    fn fill(
        &mut self,
        path: &Path,
        fill_rule: FillRule,
        style: FillOrStrokeStyle,
        composition_options: CompositionOptions,
        transform: Transform2D<f64>,
    );
    fn fill_text(
        &mut self,
        text_runs: Vec<TextRun>,
        style: FillOrStrokeStyle,
        composition_options: CompositionOptions,
        transform: Transform2D<f64>,
    );
    fn fill_rect(
        &mut self,
        rect: &Rect<f32>,
        style: FillOrStrokeStyle,
        composition_options: CompositionOptions,
        transform: Transform2D<f64>,
    );
    fn get_size(&self) -> Size2D<i32>;
    fn pop_clip(&mut self);
    fn push_clip(&mut self, path: &Path, fill_rule: FillRule, transform: Transform2D<f64>);
    fn push_clip_rect(&mut self, rect: &Rect<i32>);
    fn stroke(
        &mut self,
        path: &Path,
        style: FillOrStrokeStyle,
        line_options: LineOptions,
        composition_options: CompositionOptions,
        transform: Transform2D<f64>,
    );
    fn stroke_text(
        &mut self,
        text_runs: Vec<TextRun>,
        style: FillOrStrokeStyle,
        line_options: LineOptions,
        composition_options: CompositionOptions,
        transform: Transform2D<f64>,
    );
    fn stroke_rect(
        &mut self,
        rect: &Rect<f32>,
        style: FillOrStrokeStyle,
        line_options: LineOptions,
        composition_options: CompositionOptions,
        transform: Transform2D<f64>,
    );
    fn surface(&mut self) -> Self::SourceSurface;
    fn image_descriptor_and_serializable_data(
        &mut self,
    ) -> (ImageDescriptor, SerializableImageData);
    fn snapshot(&mut self) -> Snapshot;
}

/// A version of the `Into<T>` trait from the standard library that can be used
/// to convert between two types that are not defined in the canvas crate.
pub(crate) trait Convert<T> {
    fn convert(self) -> T;
}

/// Partii: whether a clip path (already in device space) leaves all of `viewport` drawable: it is
/// one axis-aligned rectangle that contains the viewport. A clip whose bounding box covers the
/// viewport may still have holes (several subpaths, a winding rule, a self-intersecting outline), so
/// the bounding box alone does not tell.
pub(crate) fn clip_covers_viewport(path: &kurbo::BezPath, viewport: &Rect<f64>) -> bool {
    use kurbo::{PathEl, Shape};
    // A lone trailing move-to (rect() starts a new subpath at its origin) draws nothing.
    let mut elements = path.elements();
    while let [rest @ .., PathEl::MoveTo(_)] = elements {
        if rest.is_empty() {
            break;
        }
        elements = rest;
    }
    let mut points = Vec::with_capacity(5);
    for (i, element) in elements.iter().enumerate() {
        match element {
            PathEl::MoveTo(p) if i == 0 => points.push(*p),
            PathEl::LineTo(p) if i > 0 => points.push(*p),
            PathEl::ClosePath if i > 0 && i == elements.len() - 1 => {},
            _ => return false,
        }
    }
    if points.len() == 5 && points[4] == points[0] {
        points.pop();
    }
    if points.len() != 4 {
        return false;
    }
    let axis_aligned = (0..4).all(|k| {
        let (a, b) = (points[k], points[(k + 1) % 4]);
        a.x == b.x || a.y == b.y
    });
    let bounds = path.bounding_box();
    axis_aligned &&
        bounds.x0 <= viewport.min_x() &&
        bounds.y0 <= viewport.min_y() &&
        bounds.x1 >= viewport.max_x() &&
        bounds.y1 >= viewport.max_y()
}
