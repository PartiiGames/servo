/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use euclid::default::{Point2D, Rect, Size2D, Transform2D};
use fonts::FontIdentifier;
use kurbo::Shape;
use malloc_size_of::MallocSizeOf;
use paint_api::SerializableImageData;
use pixels::{Snapshot, SnapshotAlphaMode, SnapshotPixelFormat};
use profile_traits::mem::ReportKind;
use servo_base::generic_channel::GenericSharedMemory;
use servo_canvas_traits::canvas::{
    CompositionOptions, CompositionOrBlending, CompositionStyle, FillOrStrokeStyle, FillRule,
    LineOptions, Path, ShadowOptions, TextRun,
};
use servo_config::pref;
use vello_cpu::{RenderSettings, kurbo, peniko};
use webrender_api::{ImageDescriptor, ImageDescriptorFlags};

use crate::backend::{CanvasStoreSizesPerType, Convert, GenericDrawTarget};
use crate::canvas_data::Filter;
use crate::peniko_conversions::canvas_gradient;

thread_local! {
    /// The shared font cache used by all canvases that render on a thread.
    static SHARED_FONT_CACHE: RefCell<HashMap<FontIdentifier, peniko::FontData>> = RefCell::default();
}

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
enum State {
    /// Scene is drawing. It will be consumed when rendered.
    Drawing,
    /// Scene is already rendered
    /// Before next draw we need to put current rendering
    /// in the background by calling [`VelloCPUDrawTarget::ensure_drawing`].
    Rendered,
}

pub(crate) struct VelloCPUDrawTarget {
    /// Because this is stateful context
    /// caller cannot assume anything about transform, paint, stroke,
    /// so it should provide it's own used by each command
    /// but it can assume paint_transform to be identity
    /// and fill rule to be `peniko::Fill::NonZero`
    ///
    /// This is because paint_transform is rarely set,
    /// so it's cheaper to always reset it after use.
    ctx: vello_cpu::RenderContext,
    resources: vello_cpu::Resources,
    pixmap: vello_cpu::Pixmap,
    /// Partii: with each clip's fill rule, so a clip pushed back after a render keeps it.
    clips: Vec<(Path, kurbo::Affine, peniko::Fill)>,
    state: State,
}

impl VelloCPUDrawTarget {
    fn with_composition(
        &mut self,
        composition_operation: CompositionOrBlending,
        f: impl FnOnce(&mut Self),
    ) {
        // Fast-path for default and most common composition operation
        if composition_operation == CompositionOrBlending::Composition(CompositionStyle::SourceOver)
        {
            f(self);
            return;
        }
        self.ctx.push_blend_layer(composition_operation.convert());
        f(self);
        self.ctx.pop_layer();
    }

    fn ignore_clips(&mut self, f: impl FnOnce(&mut Self)) {
        // pop all clip layers
        for _ in &self.clips {
            self.ctx.pop_layer();
        }
        f(self);
        // push all clip layers back
        for (path, affine, fill_rule) in &self.clips {
            self.ctx.set_transform(*affine);
            self.ctx.set_fill_rule(*fill_rule);
            self.ctx.push_clip_layer(&path.0);
        }
        self.ctx.set_fill_rule(peniko::Fill::NonZero);
    }

    fn ensure_drawing(&mut self) {
        match self.state {
            State::Drawing => {},
            State::Rendered => {
                self.ignore_clips(|self_| {
                    self_.ctx.set_transform(kurbo::Affine::IDENTITY);
                    self_.ctx.set_paint(vello_cpu::Image {
                        image: vello_cpu::ImageSource::Pixmap(Arc::new(self_.pixmap.clone())),
                        sampler: peniko::ImageSampler {
                            x_extend: peniko::Extend::Pad,
                            y_extend: peniko::Extend::Pad,
                            quality: peniko::ImageQuality::Low,
                            alpha: 1.0,
                        },
                    });
                    self_.ctx.fill_rect(&kurbo::Rect::from_origin_size(
                        (0., 0.),
                        self_.size().cast(),
                    ));
                });
                self.state = State::Drawing;
            },
        }
    }

    fn pixmap(&mut self) -> &[u8] {
        if self.state == State::Drawing {
            self.ignore_clips(|self_| {
                self_.ctx.flush();
                self_.ctx.render(&mut self_.pixmap, &mut self_.resources);
                self_.ctx.reset();
                self_.state = State::Rendered;
            });
        }

        self.pixmap.data_as_u8_slice()
    }

    fn size(&self) -> Size2D<u32> {
        Size2D::new(self.ctx.width(), self.ctx.height()).cast()
    }

    /// Partii: copy `source` of `surface` to `destination` in the backing pixmap, row by row
    /// (both are premultiplied RGBA8). Pending drawing is rendered first. Returns false if the
    /// rectangles don't fit the expected shapes (then the caller draws instead).
    fn copy_into_pixmap(
        &mut self,
        surface: &vello_cpu::Pixmap,
        source: Rect<i32>,
        destination: Point2D<i32>,
    ) -> bool {
        let (sw, sh) = (i32::from(surface.width()), i32::from(surface.height()));
        if source.origin.x < 0 ||
            source.origin.y < 0 ||
            source.max_x() > sw ||
            source.max_y() > sh
        {
            return false;
        }
        let _ = self.pixmap(); // render pending drawing; state is Rendered now
        let (dw, dh) = (
            i32::from(self.pixmap.width()),
            i32::from(self.pixmap.height()),
        );
        // Clip to the canvas.
        let x0 = destination.x.max(0);
        let y0 = destination.y.max(0);
        let x1 = (destination.x + source.size.width).min(dw);
        let y1 = (destination.y + source.size.height).min(dh);
        if x0 >= x1 || y0 >= y1 {
            return true;
        }
        let n = (x1 - x0) as usize;
        let src = surface.data();
        let dst = self.pixmap.data_mut();
        for y in y0..y1 {
            let sy = source.origin.y + (y - destination.y);
            let sx = source.origin.x + (x0 - destination.x);
            let s0 = (sy * sw + sx) as usize;
            let d0 = (y * dw + x0) as usize;
            dst[d0..d0 + n].copy_from_slice(&src[s0..s0 + n]);
        }
        self.pixmap.set_may_have_transparency(true);
        true
    }

    /// Partii: whether filling `rect` with this paint covers every pixel of the canvas with an
    /// opaque colour, so whatever was drawn before can't show (the usual first call of a game's
    /// frame: fillStyle = background; fillRect(0, 0, w, h)).
    fn fill_covers_canvas(
        &mut self,
        rect: &Rect<f32>,
        paint: &vello_cpu::PaintType,
        composition_options: &CompositionOptions,
        transform: &Transform2D<f64>,
    ) -> bool {
        let opaque = matches!(paint, vello_cpu::PaintType::Solid(c) if c.components[3] >= 1.0);
        opaque &&
            composition_options.composition_operation ==
                CompositionOrBlending::Composition(CompositionStyle::SourceOver) &&
            // Axis-aligned: the bounding box of a rotated rect is more than the rect.
            transform.m12 == 0.0 &&
            transform.m21 == 0.0 &&
            self.is_viewport_cleared(rect, *transform)
    }

    fn is_viewport_cleared(&mut self, rect: &Rect<f32>, transform: Transform2D<f64>) -> bool {
        let transformed_rect = transform.outer_transformed_rect(&rect.cast());
        if transformed_rect.is_empty() {
            return false;
        }
        let viewport: Rect<f64> = Rect::from_size(self.get_size().cast());
        // Partii: every clip must leave the whole viewport drawable; a bounding box covering the
        // viewport is not enough (clip paths with holes: 2d.path.clip.winding.*).
        let unclipped = self.clips.iter().all(|(path, affine, _)| {
            let mut bez = path.0.clone();
            bez.apply_affine(*affine);
            crate::backend::clip_covers_viewport(&bez, &viewport)
        });
        transformed_rect.cast().contains_rect(&viewport) && // whole viewport is cleared
            unclipped // viewport is not clipped
    }
}

fn worker_thread_count(size: &Size2D<u16>) -> u16 {
    // TODO: Somewhat arbitrary chosen, should be based on a benchmark,
    // measuring where we start to benefit from multithreading
    const SMALL_CANVAS_SIZE: u32 = 512 * 512;
    // For small sizes single-threaded is better.
    if u32::from(size.width) * u32::from(size.height) < SMALL_CANVAS_SIZE {
        0
    } else {
        // The fallback `3` is the default value from `prefs.rs`
        pref!(thread_pool_canvas_workers).try_into().unwrap_or(3)
    }
}

impl GenericDrawTarget for VelloCPUDrawTarget {
    type SourceSurface = Arc<vello_cpu::Pixmap>;

    fn new(size: Size2D<u32>) -> Self {
        let size = size.cast();
        let settings = RenderSettings {
            num_threads: worker_thread_count(&size),
            ..Default::default()
        };
        let ctx = vello_cpu::RenderContext::new_with(size.width, size.height, settings);
        Self {
            ctx,
            resources: vello_cpu::Resources::new(),
            pixmap: vello_cpu::Pixmap::new(size.width, size.height),
            clips: Vec::new(),
            state: State::Rendered,
        }
    }

    fn canvas_store_sizes(
        &self,
        ops: &mut malloc_size_of::MallocSizeOfOps,
    ) -> Option<Vec<CanvasStoreSizesPerType>> {
        Some(vec![CanvasStoreSizesPerType {
            name: "backing-buffer",
            size: self.pixmap.size_of(ops),
            kind: ReportKind::ExplicitJemallocHeapSize,
        }])
    }

    fn clear_rect(&mut self, rect: &Rect<f32>, transform: Transform2D<f64>) {
        // vello_cpu RenderingContext only ever grows,
        // so we need to use every opportunity to shrink it
        if self.is_viewport_cleared(rect, transform) {
            self.ctx.reset();
            self.clips.clear(); // no clips are affecting rendering
            self.state = State::Drawing;
            return;
        }
        self.ensure_drawing();
        let rect: kurbo::Rect = rect.cast().into();
        let mut clip_path = rect.to_path(0.1);
        clip_path.apply_affine(transform.cast().into());
        let blend_mode = peniko::Compose::Clear;
        self.ctx.push_layer(
            Some(&clip_path.to_path(0.1)),
            Some(blend_mode.into()),
            None,
            None,
            None,
        );
        self.ctx.pop_layer();
    }

    fn copy_surface(
        &mut self,
        surface: Self::SourceSurface,
        source: Rect<i32>,
        destination: Point2D<i32>,
    ) {
        // Partii: putImageData (the only caller) replaces pixels, ignoring the clip, transform,
        // global alpha and compositing. Copy the rows into the backing pixmap instead of drawing
        // the data as an image fill: no re-rasterizing the old content plus the new one, and
        // translucent pixels replace rather than blend, as the spec says.
        if self.copy_into_pixmap(&surface, source, destination) {
            return;
        }
        self.ensure_drawing();
        let destination: kurbo::Point = destination.cast::<f64>().into();
        let rect = kurbo::Rect::from_origin_size(destination, source.size.cast());
        self.ctx.set_transform(kurbo::Affine::IDENTITY);
        self.ignore_clips(|self_| {
            // Clipped blending does not work correctly:
            // https://github.com/linebender/vello/issues/1119
            // self_.push_layer(Some(rect.to_path(0.1)), Some(peniko::Compose::Copy.into()), None, None);

            self_.ctx.set_paint(vello_cpu::Image {
                image: vello_cpu::ImageSource::Pixmap(surface),
                sampler: peniko::ImageSampler {
                    x_extend: peniko::Extend::Pad,
                    y_extend: peniko::Extend::Pad,
                    quality: peniko::ImageQuality::Low,
                    alpha: 1.0,
                },
            });
            self_.ctx.fill_rect(&rect);

            // self_.ctx.pop_layer();
        });
    }

    fn create_similar_draw_target(&self, size: &Size2D<i32>) -> Self {
        Self::new(size.cast())
    }

    fn draw_surface(
        &mut self,
        mut surface: Self::SourceSurface,
        dest: Rect<f64>,
        source: Rect<f64>,
        filter: Filter,
        composition_options: CompositionOptions,
        transform: Transform2D<f64>,
    ) {
        self.ensure_drawing();
        let scale_up = dest.size.width > source.size.width || dest.size.height > source.size.height;
        if composition_options.alpha != 1.0 {
            Arc::get_mut(&mut surface)
                .expect("surface should be owned")
                .multiply_alpha((composition_options.alpha * 255.0) as u8);
        }
        self.with_composition(composition_options.composition_operation, move |self_| {
            self_.ctx.set_transform(transform.cast().into());
            self_.ctx.set_paint(vello_cpu::Image {
                image: vello_cpu::ImageSource::Pixmap(surface),
                sampler: peniko::ImageSampler {
                    x_extend: peniko::Extend::Pad,
                    y_extend: peniko::Extend::Pad,
                    // we should only do bicubic when scaling up
                    quality: if scale_up {
                        filter.convert()
                    } else {
                        peniko::ImageQuality::Low
                    },
                    alpha: 1.0,
                },
            });
            self_.ctx.set_paint_transform(
                kurbo::Affine::translate((dest.origin.x, dest.origin.y)).pre_scale_non_uniform(
                    dest.size.width / source.size.width,
                    dest.size.height / source.size.height,
                ),
            );
            self_.ctx.fill_rect(&dest.into());
            self_.ctx.reset_paint_transform();
        })
    }

    /// Partii: <https://html.spec.whatwg.org/multipage/#shadows>. `surface` is image A (the shape,
    /// drawn source-over with the global alpha) whose top-left lands at `dest` in device space.
    /// The shadow is A's alpha in the shadow colour, blurred (Gaussian, sigma = shadowBlur / 2,
    /// approximated by three box blurs) and offset by shadowOffsetX/Y (not transformed); it is
    /// composited first, then A, both with the current operator and the clip.
    fn draw_surface_with_shadow(
        &mut self,
        surface: Self::SourceSurface,
        dest: &Point2D<f32>,
        shadow_options: &ShadowOptions,
        composition_options: CompositionOptions,
    ) {
        let (w, h) = (usize::from(surface.width()), usize::from(surface.height()));
        if w == 0 || h == 0 {
            return;
        }
        let pad = crate::canvas_data::shadow_blur_padding(shadow_options.blur) as usize;
        let (sw, sh) = (w + 2 * pad, h + 2 * pad);
        let options = CompositionOptions {
            alpha: 1.0,
            ..composition_options
        };
        let dest = Point2D::new(f64::from(dest.x), f64::from(dest.y));
        if sw <= usize::from(u16::MAX) && sh <= usize::from(u16::MAX) {
            let mut alpha = vec![0f32; sw * sh];
            for (y, row) in surface.data().chunks_exact(w).enumerate() {
                let out = &mut alpha[(y + pad) * sw + pad..(y + pad) * sw + pad + w];
                for (o, p) in out.iter_mut().zip(row) {
                    *o = f32::from(p.a) / 255.0;
                }
            }
            if shadow_options.blur > 0.0 {
                gaussian_blur(&mut alpha, sw, sh, shadow_options.blur / 2.0);
            }
            let color: peniko::Color = shadow_options.color.convert();
            let [r, g, b, a] = color.components;
            let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            let pixels: Vec<peniko::color::PremulRgba8> = alpha
                .iter()
                .map(|&v| {
                    let ca = (v * a).clamp(0.0, 1.0);
                    peniko::color::PremulRgba8 {
                        r: to_u8(r * ca),
                        g: to_u8(g * ca),
                        b: to_u8(b * ca),
                        a: to_u8(ca),
                    }
                })
                .collect();
            let shadow = Arc::new(vello_cpu::Pixmap::from_parts(pixels, sw as u16, sh as u16));
            let shadow_dest = Rect::new(
                Point2D::new(
                    dest.x - pad as f64 + shadow_options.offset_x,
                    dest.y - pad as f64 + shadow_options.offset_y,
                ),
                Size2D::new(sw as f64, sh as f64),
            );
            self.draw_surface(
                shadow,
                shadow_dest,
                Rect::from_size(Size2D::new(sw as f64, sh as f64)),
                Filter::Nearest,
                options,
                Transform2D::identity(),
            );
        }
        self.draw_surface(
            surface,
            Rect::new(dest, Size2D::new(w as f64, h as f64)),
            Rect::from_size(Size2D::new(w as f64, h as f64)),
            Filter::Nearest,
            options,
            Transform2D::identity(),
        );
    }

    fn fill(
        &mut self,
        path: &Path,
        fill_rule: FillRule,
        style: FillOrStrokeStyle,
        composition_options: CompositionOptions,
        transform: Transform2D<f64>,
    ) {
        self.ensure_drawing();
        self.with_composition(composition_options.composition_operation, |self_| {
            self_.ctx.set_transform(transform.cast().into());
            self_.ctx.set_fill_rule(fill_rule.convert());
            self_.ctx.set_paint(paint(style, composition_options.alpha));
            self_.ctx.fill_path(&path.0);
        });
        self.ctx.set_fill_rule(peniko::Fill::NonZero);
    }

    fn fill_text(
        &mut self,
        text_runs: Vec<TextRun>,
        style: FillOrStrokeStyle,
        composition_options: CompositionOptions,
        transform: Transform2D<f64>,
    ) {
        self.ensure_drawing();
        self.ctx.set_paint(paint(style, composition_options.alpha));
        self.ctx.set_transform(transform.cast().into());
        self.with_composition(composition_options.composition_operation, |self_| {
            for text_run in text_runs.iter() {
                SHARED_FONT_CACHE.with(|font_cache| {
                    let identifier = &text_run.font.identifier;
                    if !font_cache.borrow().contains_key(identifier) {
                        let Some(font_data_and_index) = text_run.font.font_data_and_index() else {
                            return;
                        };
                        let font = font_data_and_index.convert();
                        font_cache.borrow_mut().insert(identifier.clone(), font);
                    }

                    let font_cache = font_cache.borrow();
                    let Some(font) = font_cache.get(identifier) else {
                        return;
                    };
                    self_
                        .ctx
                        .glyph_run(&mut self_.resources, font)
                        .font_size(text_run.pt_size)
                        .fill_glyphs(text_run.glyphs_and_positions.iter().map(
                            |glyph_and_position| vello_cpu::Glyph {
                                id: glyph_and_position.id,
                                x: glyph_and_position.point.x,
                                y: glyph_and_position.point.y,
                            },
                        ));
                });
            }
        })
    }

    fn fill_rect(
        &mut self,
        rect: &Rect<f32>,
        style: FillOrStrokeStyle,
        composition_options: CompositionOptions,
        transform: Transform2D<f64>,
    ) {
        let paint = paint(style, composition_options.alpha);
        if self.fill_covers_canvas(rect, &paint, &composition_options, &transform) {
            // Partii: drop the old content and everything queued, instead of re-drawing the
            // whole previous frame as an image under this fill (ensure_drawing).
            self.ignore_clips(|self_| self_.ctx.reset());
            self.state = State::Drawing;
        } else {
            self.ensure_drawing();
        }
        self.with_composition(composition_options.composition_operation, |self_| {
            self_.ctx.set_transform(transform.cast().into());
            self_.ctx.set_paint(paint);
            self_.ctx.fill_rect(&rect.cast().into());
        })
    }

    fn get_size(&self) -> Size2D<i32> {
        self.size().cast()
    }

    fn pop_clip(&mut self) {
        if self.clips.pop().is_some() {
            self.ctx.pop_layer();
        }
    }

    fn push_clip(&mut self, path: &Path, fill_rule: FillRule, transform: Transform2D<f64>) {
        let affine = transform.cast().into();
        self.ctx.set_transform(affine);
        let fill_rule = fill_rule.convert();
        self.ctx.set_fill_rule(fill_rule);
        self.ctx.push_clip_layer(&path.0);
        self.clips.push((path.clone(), affine, fill_rule));
        self.ctx.set_fill_rule(peniko::Fill::NonZero);
    }

    fn push_clip_rect(&mut self, rect: &Rect<i32>) {
        let mut path = Path::new();
        let rect = rect.cast();
        path.rect(
            rect.origin.x,
            rect.origin.y,
            rect.size.width,
            rect.size.height,
        );
        self.push_clip(&path, FillRule::Nonzero, Transform2D::identity());
    }

    fn stroke(
        &mut self,
        path: &Path,
        style: FillOrStrokeStyle,
        line_options: LineOptions,
        composition_options: CompositionOptions,
        transform: Transform2D<f64>,
    ) {
        self.ensure_drawing();
        self.with_composition(composition_options.composition_operation, |self_| {
            self_.ctx.set_transform(transform.cast().into());
            self_.ctx.set_paint(paint(style, composition_options.alpha));
            self_.ctx.set_stroke(line_options.convert());
            self_.ctx.stroke_path(&path.0);
        })
    }

    fn stroke_text(
        &mut self,
        text_runs: Vec<TextRun>,
        style: FillOrStrokeStyle,
        line_options: LineOptions,
        composition_options: CompositionOptions,
        transform: Transform2D<f64>,
    ) {
        self.ensure_drawing();
        self.ctx.set_paint(paint(style, composition_options.alpha));
        self.ctx.set_stroke(line_options.convert());
        self.ctx.set_transform(transform.cast().into());
        self.with_composition(composition_options.composition_operation, |self_| {
            for text_run in text_runs.iter() {
                SHARED_FONT_CACHE.with(|font_cache| {
                    let identifier = &text_run.font.identifier;
                    if !font_cache.borrow().contains_key(identifier) {
                        let Some(font_data_and_index) = text_run.font.font_data_and_index() else {
                            return;
                        };
                        let font = font_data_and_index.convert();
                        font_cache.borrow_mut().insert(identifier.clone(), font);
                    }

                    let font_cache = font_cache.borrow();
                    let Some(font) = font_cache.get(identifier) else {
                        return;
                    };
                    self_
                        .ctx
                        .glyph_run(&mut self_.resources, font)
                        .font_size(text_run.pt_size)
                        .stroke_glyphs(text_run.glyphs_and_positions.iter().map(
                            |glyph_and_position| vello_cpu::Glyph {
                                id: glyph_and_position.id,
                                x: glyph_and_position.point.x,
                                y: glyph_and_position.point.y,
                            },
                        ));
                });
            }
        })
    }

    fn stroke_rect(
        &mut self,
        rect: &Rect<f32>,
        style: FillOrStrokeStyle,
        line_options: LineOptions,
        composition_options: CompositionOptions,
        transform: Transform2D<f64>,
    ) {
        self.ensure_drawing();
        self.with_composition(composition_options.composition_operation, |self_| {
            self_.ctx.set_transform(transform.cast().into());
            self_.ctx.set_paint(paint(style, composition_options.alpha));
            self_.ctx.set_stroke(line_options.convert());
            self_.ctx.stroke_rect(&rect.cast().into());
        })
    }

    fn image_descriptor_and_serializable_data(
        &mut self,
    ) -> (ImageDescriptor, SerializableImageData) {
        let image_desc = ImageDescriptor {
            format: webrender_api::ImageFormat::RGBA8,
            size: self.size().cast().cast_unit(),
            stride: None,
            offset: 0,
            flags: ImageDescriptorFlags::empty(),
        };
        let data = SerializableImageData::Raw(GenericSharedMemory::from_bytes(self.pixmap()));
        (image_desc, data)
    }

    fn snapshot(&mut self) -> pixels::Snapshot {
        Snapshot::from_vec(
            self.size().cast(),
            SnapshotPixelFormat::RGBA,
            SnapshotAlphaMode::Transparent {
                premultiplied: true,
            },
            self.pixmap().to_vec(),
        )
    }

    fn surface(&mut self) -> Self::SourceSurface {
        self.pixmap(); // sync pixmap
        Arc::new(vello_cpu::Pixmap::from_parts(
            self.pixmap.clone().take(),
            self.pixmap.width(),
            self.pixmap.height(),
        ))
    }

    fn create_source_surface_from_data(&self, data: Snapshot) -> Option<Self::SourceSurface> {
        Some(snapshot_as_pixmap(data))
    }
}

fn snapshot_as_pixmap(mut snapshot: Snapshot) -> Arc<vello_cpu::Pixmap> {
    let size = snapshot.size().cast();
    snapshot.transform(
        SnapshotAlphaMode::Transparent {
            premultiplied: true,
        },
        SnapshotPixelFormat::RGBA,
    );

    Arc::new(vello_cpu::Pixmap::from_parts(
        bytemuck::cast_vec(snapshot.into()),
        size.width,
        size.height,
    ))
}

impl Convert<vello_cpu::PaintType> for FillOrStrokeStyle {
    fn convert(self) -> vello_cpu::PaintType {
        use servo_canvas_traits::canvas::FillOrStrokeStyle::*;
        match self {
            Color(absolute_color) => vello_cpu::PaintType::Solid(absolute_color.convert()),
            LinearGradient(style) => {
                let start = kurbo::Point::new(style.x0, style.y0);
                let end = kurbo::Point::new(style.x1, style.y1);
                let gradient = peniko::Gradient::new_linear(start, end);
                vello_cpu::PaintType::Gradient(canvas_gradient(gradient, style.stops))
            },
            RadialGradient(style) => {
                let center1 = kurbo::Point::new(style.x0, style.y0);
                let center2 = kurbo::Point::new(style.x1, style.y1);
                let gradient = peniko::Gradient::new_two_point_radial(
                    center1,
                    style.r0 as f32,
                    center2,
                    style.r1 as f32,
                );
                vello_cpu::PaintType::Gradient(canvas_gradient(gradient, style.stops))
            },
            ConicGradient(style) => {
                // Partii: one turn clockwise from the start angle. The sweep position is
                // measured from the start angle with pixel angles in [0, 2π), so repeat to wrap
                // the pixels that lie "before" a start angle past 0 (conic.positive.rotation).
                let start = style.angle.rem_euclid(std::f64::consts::TAU) as f32;
                let gradient = peniko::Gradient::new_sweep(
                    kurbo::Point::new(style.x, style.y),
                    start,
                    start + std::f32::consts::TAU,
                )
                .with_extend(peniko::Extend::Repeat);
                vello_cpu::PaintType::Gradient(canvas_gradient(gradient, style.stops))
            },
            Surface(surface_style) => {
                let pixmap = snapshot_as_pixmap(surface_style.surface_data.to_owned());
                vello_cpu::PaintType::Image(vello_cpu::Image {
                    image: vello_cpu::ImageSource::Pixmap(pixmap),
                    sampler: peniko::ImageSampler {
                        x_extend: if surface_style.repeat_x {
                            peniko::Extend::Repeat
                        } else {
                            peniko::Extend::Pad
                        },
                        y_extend: if surface_style.repeat_y {
                            peniko::Extend::Repeat
                        } else {
                            peniko::Extend::Pad
                        },
                        quality: peniko::ImageQuality::Low,
                        alpha: 1.0,
                    },
                })
            },
        }
    }
}

fn paint(style: FillOrStrokeStyle, alpha: f64) -> vello_cpu::PaintType {
    assert!((0.0..=1.0).contains(&alpha));
    let paint = style.convert();
    if alpha == 1.0 {
        paint
    } else {
        match paint {
            vello_cpu::PaintType::Solid(alpha_color) => {
                vello_cpu::PaintType::Solid(alpha_color.multiply_alpha(alpha as f32))
            },
            vello_cpu::PaintType::Gradient(gradient) => {
                vello_cpu::PaintType::Gradient(gradient.multiply_alpha(alpha as f32))
            },
            vello_cpu::PaintType::Image(mut image) => {
                match &mut image.image {
                    vello_cpu::ImageSource::Pixmap(pixmap) => Arc::get_mut(pixmap)
                        .expect("pixmap should not be shared with anyone at this point")
                        .multiply_alpha((alpha * 255.0) as u8),
                    vello_cpu::ImageSource::OpaqueId { .. } => unimplemented!(),
                };
                vello_cpu::PaintType::Image(image)
            },
        }
    }
}

/// Partii: an in-place Gaussian blur of a `w` x `h` coverage buffer, as three box blurs per axis
/// (W3C filter-effects' approximation, also what browsers use for canvas shadows).
fn gaussian_blur(buf: &mut [f32], w: usize, h: usize, sigma: f64) {
    // Box sizes for three passes: https://www.w3.org/TR/filter-effects-1/#feGaussianBlurElement
    let d = (sigma * 3.0 * (2.0 * std::f64::consts::PI).sqrt() / 4.0 + 0.5).floor() as usize;
    if d == 0 {
        return;
    }
    let mut tmp = vec![0f32; w.max(h)];
    let mut line = vec![0f32; w.max(h)];
    // Odd d: three centred boxes of size d. Even d: two boxes offset by half a pixel each way,
    // then one of size d + 1 centred.
    let passes: [(usize, usize); 3] = if d % 2 == 1 {
        [(d / 2, d / 2); 3]
    } else {
        [(d / 2, d / 2 - 1), (d / 2 - 1, d / 2), (d / 2, d / 2)]
    };
    for y in 0..h {
        line[..w].copy_from_slice(&buf[y * w..(y + 1) * w]);
        for &(left, right) in &passes {
            box_blur_line(&line[..w], &mut tmp[..w], left, right);
            line[..w].copy_from_slice(&tmp[..w]);
        }
        buf[y * w..(y + 1) * w].copy_from_slice(&line[..w]);
    }
    for x in 0..w {
        for y in 0..h {
            line[y] = buf[y * w + x];
        }
        for &(left, right) in &passes {
            box_blur_line(&line[..h], &mut tmp[..h], left, right);
            line[..h].copy_from_slice(&tmp[..h]);
        }
        for y in 0..h {
            buf[y * w + x] = line[y];
        }
    }
}

/// out[i] = mean of src[i - left ..= i + right] (zero outside), with a running sum.
fn box_blur_line(src: &[f32], out: &mut [f32], left: usize, right: usize) {
    let n = src.len();
    let size = (left + right + 1) as f32;
    let mut sum = 0f32;
    for v in src.iter().take(right.min(n)) {
        sum += v;
    }
    for i in 0..n {
        if i + right < n {
            sum += src[i + right];
        }
        out[i] = sum / size;
        if i >= left {
            sum -= src[i - left];
        }
    }
}
