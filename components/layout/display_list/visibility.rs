/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The visibility algorithm of IntersectionObserver v2
//! (<https://w3c.github.io/IntersectionObserver/#calculate-visibility-algo>), run over the
//! stacking context tree in paint order:
//!
//! - the target has visual effects if any stacking context it is painted in (its own included)
//!   has opacity < 1, a filter, a clip-path or a non-normal blend mode (masks: TODO), or if its
//!   transform to the viewport is anything but a translation and a proportional upscale;
//! - the target is occluded if anything painted after it, other than its own subtree, with a
//!   non-zero effective opacity and `visibility: visible`, overlaps the target's visible rect
//!   with its ink bounds (border box plus outset box-shadows; text plus text-shadows), as
//!   Blink's occlusion test counts shadows.

use std::collections::HashSet;
use std::sync::Arc;

use app_units::Au;
use servo_base::id::ScrollTreeNodeId;
use style::computed_values::mix_blend_mode::T as MixBlendMode;
use style::computed_values::visibility::T as Visibility;
use style::dom::OpaqueNode;
use style::properties::ComputedValues;
use style::values::computed::basic_shape::ClipPath;
use webrender_api::units::{LayoutPoint, LayoutRect, LayoutVector2D};

use crate::display_list::paint_traversal::{PaintTraversal, PaintTraversalHandler};
use crate::display_list::{StackingContext, StackingContextTree, ToWebRender, TraversalState};
use crate::fragment_tree::{BoxFragmentWithStyle, FragmentFlags, TextFragment};
use crate::geom::PhysicalRect;

struct Layer {
    opacity: f32,
    has_effects: bool,
}

pub(crate) struct VisibilityTest<'a> {
    tree: &'a StackingContextTree,
    target: OpaqueNode,
    subtree: &'a HashSet<OpaqueNode>,
    layers: Vec<Layer>,
    /// The target's border boxes in viewport coordinates, once painted.
    target_rect: Option<LayoutRect>,
    target_has_effects: bool,
    /// Ink bounds (viewport coordinates) of everything painted after the target.
    occluders: Vec<LayoutRect>,
}

impl<'a> VisibilityTest<'a> {
    /// Whether `target` (whose inclusive descendants are `subtree`) is visible.
    pub(crate) fn run(
        tree: &'a StackingContextTree,
        target: OpaqueNode,
        subtree: &'a HashSet<OpaqueNode>,
    ) -> bool {
        let mut test = Self {
            tree,
            target,
            subtree,
            layers: Vec::new(),
            target_rect: None,
            target_has_effects: false,
            occluders: Vec::new(),
        };
        PaintTraversal::traverse(&tree.root_stacking_context, &mut test);

        let Some(target_rect) = test.target_rect else {
            return false;
        };
        if test.target_has_effects {
            return false;
        }
        let viewport = LayoutRect::from_origin_and_size(
            LayoutPoint::zero(),
            tree.paint_info.viewport_details.size.cast_unit(),
        );
        let Some(visible) = target_rect.intersection(&viewport) else {
            return false;
        };
        if visible.is_empty() {
            return false;
        }
        !test
            .occluders
            .iter()
            .any(|occluder| occluder.intersection(&visible).is_some_and(|r| !r.is_empty()))
    }

    fn effective_opacity(&self) -> f32 {
        self.layers.iter().map(|layer| layer.opacity).product()
    }

    /// `rect` (in the coordinates of `spatial_id`) in viewport coordinates.
    fn to_viewport(&self, spatial_id: ScrollTreeNodeId, rect: LayoutRect) -> Option<LayoutRect> {
        let transform = self
            .tree
            .paint_info
            .scroll_tree
            .cumulative_node_to_root_transform(spatial_id);
        transform.to_transform().outer_transformed_box2d(&rect)
    }

    /// Whether the transform from `spatial_id` to the viewport is a translation plus a
    /// proportional upscale (the only transforms the visibility algorithm allows).
    fn transform_allowed(&self, spatial_id: ScrollTreeNodeId) -> bool {
        let transform = self
            .tree
            .paint_info
            .scroll_tree
            .cumulative_node_to_root_transform(spatial_id);
        let m = transform.to_transform();
        if !m.is_2d() {
            return false;
        }
        m.m12 == 0. && m.m21 == 0. && m.m11 == m.m22 && m.m11 >= 1.
    }

    #[allow(clippy::too_many_arguments)]
    fn visit_painted(
        &mut self,
        state: &TraversalState,
        node: OpaqueNode,
        style: &ComputedValues,
        rect: PhysicalRect<Au>,
        shadows: impl Iterator<Item = (f32, f32, f32)>,
        ignore_visibility: bool,
    ) {
        let rect = rect.translate(state.origin.to_vector()).to_webrender();
        if node == self.target {
            if !self.transform_allowed(state.spatial_id) ||
                self.layers.iter().any(|layer| layer.has_effects)
            {
                self.target_has_effects = true;
            }
            if let Some(rect) = self.to_viewport(state.spatial_id, rect) {
                self.target_rect = Some(match self.target_rect {
                    Some(previous) => previous.union(&rect),
                    None => rect,
                });
            }
            return;
        }
        if self.target_rect.is_none() || self.subtree.contains(&node) {
            return;
        }
        if (!ignore_visibility && style.get_inherited_box().visibility != Visibility::Visible) ||
            self.effective_opacity() <= 0.
        {
            return;
        }
        let mut ink = rect;
        for (dx, dy, grow) in shadows {
            let shadow = rect
                .translate(LayoutVector2D::new(dx, dy))
                .inflate(grow, grow);
            ink = ink.union(&shadow);
        }
        if ink.is_empty() {
            return;
        }
        if let Some(ink) = self.to_viewport(state.spatial_id, ink) {
            self.occluders.push(ink);
        }
    }
}

fn has_effects(style: &ComputedValues) -> bool {
    let effects = style.get_effects();
    effects.opacity < 1.0 ||
        !effects.filter.0.is_empty() ||
        effects.mix_blend_mode != MixBlendMode::Normal ||
        style.get_svg().clip_path != ClipPath::None
}

impl PaintTraversalHandler for VisibilityTest<'_> {
    type StackingContextState = bool;

    fn visit_stacking_context(&mut self, stacking_context: &StackingContext) -> bool {
        let Some(fragment) = stacking_context.fragment() else {
            return false;
        };
        let fragment = fragment.with_style();
        let style = fragment.style();
        self.layers.push(Layer {
            opacity: style.get_effects().opacity,
            has_effects: has_effects(style),
        });
        true
    }

    fn leave_stacking_context(&mut self, _: &TraversalState, pushed: bool) {
        if pushed {
            self.layers.pop();
        }
    }

    fn visit_box(&mut self, state: &TraversalState, fragment: &BoxFragmentWithStyle<'_>) {
        let Some(node) = fragment.base.tag.as_ref().map(|tag| tag.node) else {
            return;
        };
        let style = fragment.style();
        let shadows = style
            .get_effects()
            .box_shadow
            .0
            .iter()
            .filter(|shadow| !shadow.inset)
            .map(|shadow| {
                (
                    shadow.base.horizontal.px(),
                    shadow.base.vertical.px(),
                    shadow.spread.px() + shadow.base.blur.px(),
                )
            })
            .collect::<Vec<_>>();
        self.visit_painted(
            state,
            node,
            &style,
            fragment.border_rect(),
            shadows.into_iter(),
            // An inline <svg>'s content can be visible (and draw shadows anywhere in its box)
            // while the <svg> itself is visibility: hidden: its whole box counts.
            fragment.base.flags.contains(FragmentFlags::IS_SVG_ELEMENT),
        );
    }

    fn visit_text(
        &mut self,
        state: &TraversalState,
        _line_box_rect: PhysicalRect<Au>,
        fragment: &Arc<TextFragment>,
    ) {
        let Some(node) = fragment.base.tag.as_ref().map(|tag| tag.node) else {
            return;
        };
        let style = fragment.style();
        let shadows = style
            .get_inherited_text()
            .text_shadow
            .0
            .iter()
            .map(|shadow| {
                (
                    shadow.horizontal.px(),
                    shadow.vertical.px(),
                    shadow.blur.px(),
                )
            })
            .collect::<Vec<_>>();
        self.visit_painted(
            state,
            node,
            &style,
            fragment.base.rect(),
            shadows.into_iter(),
            false,
        );
    }
}
