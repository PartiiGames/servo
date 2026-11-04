/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii: precise `:has()` invalidation, wiring Stylo's relative selector invalidator into
//! Servo (Gecko does the same from its glue). Two entry points:
//!
//! - [`invalidate_for_snapshots`]: during a restyle, for every element whose attributes, classes,
//!   id or state changed (the snapshot table), find the `:has()` anchors that may change and give
//!   them a restyle hint, setting dirty-descendant bits up to the root so that a traversal from
//!   the root reaches them.
//! - [`invalidate_for_insertion`] / [`invalidate_for_removal`]: called by script around DOM
//!   mutations (layout never sees removals); they return the elements script must mark dirty.

#![allow(unsafe_code)] // TElement::set_dirty_descendants (dirty bits, as Stylo's invalidator sets them)

use std::cell::RefCell;
use std::marker::PhantomData;

use selectors::matching::ElementSelectorFlags;
use smallvec::SmallVec;
use style::dom::{OpaqueNode, TElement, TNode};
use style::invalidation::element::element_wrapper::{ElementSnapshot, ElementWrapper};
use style::invalidation::element::invalidator::{InvalidationResult, SiblingTraversalMap};
use style::invalidation::element::relative_selector::{
    DomMutationOperation, RelativeSelectorInvalidator,
};
use style::invalidation::element::restyle_hints::RestyleHint;
use style::selector_parser::{Snapshot, SnapshotMap};
use style::stylist::Stylist;
use stylo_atoms::Atom;

thread_local! {
    /// The elements the invalidator invalidated during the current call (anchors, or the parent
    /// of invalidated siblings).
    static INVALIDATED: RefCell<Vec<OpaqueNode>> = const { RefCell::new(Vec::new()) };
    /// Whether the current call comes from script (a DOM mutation). Then script marks the
    /// returned elements dirty (Node::dirty keeps the document's dirty root right), and layout
    /// must not set dirty-descendant bits itself: script's dirty-root walk stops at a bit that's
    /// already set, so the dirty root would miss the anchor (and its new, unstyled children).
    static FROM_SCRIPT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether any stylesheet in use has a `:has()` selector (so relative selector invalidation is
/// needed at all).
pub(crate) fn uses_relative_selectors(stylist: &Stylist) -> bool {
    stylist
        .iter_origins()
        .any(|(data, _)| data.relative_invalidation_map_attributes().used)
}

/// Set the dirty-descendants bit on every ancestor of `element` (up to the first one that has it
/// already), so that a traversal from the root reaches `element`.
pub(crate) fn mark_ancestors_dirty<E: TElement>(element: E) {
    let mut current = element.traversal_parent();
    while let Some(ancestor) = current {
        if ancestor.has_dirty_descendants() {
            break;
        }
        unsafe { ancestor.set_dirty_descendants() };
        current = ancestor.traversal_parent();
    }
}

/// The invalidator's callback, once per anchor it invalidated.
fn invalidated_at<E: TElement>(element: E, result: &InvalidationResult) {
    let marked = if result.has_invalidated_siblings() {
        // Restyle the parent's subtree (the siblings and their descendants), conservatively.
        match element.traversal_parent() {
            Some(parent) => {
                if let Some(mut data) = parent.mutate_data() {
                    data.hint.insert(RestyleHint::restyle_subtree());
                }
                parent
            },
            None => element,
        }
    } else {
        element
    };
    if FROM_SCRIPT.with(|from_script| from_script.get()) {
        // Script marks `marked` dirty; descendants the invalidator touched are restyled with it.
        if result.has_invalidated_descendants() {
            if let Some(mut data) = marked.mutate_data() {
                data.hint.insert(RestyleHint::restyle_subtree());
            }
        }
    } else {
        if result.has_invalidated_descendants() {
            unsafe { marked.set_dirty_descendants() };
        }
        mark_ancestors_dirty(marked);
    }
    INVALIDATED.with(|invalidated| invalidated.borrow_mut().push(marked.as_node().opaque()));
}

/// Restyle `element`'s whole subtree (the conservative fallback for sibling side effects).
fn invalidate_subtree<E: TElement>(element: E) {
    if let Some(mut data) = element.mutate_data() {
        data.hint.insert(RestyleHint::restyle_subtree());
    }
    if !FROM_SCRIPT.with(|from_script| from_script.get()) {
        mark_ancestors_dirty(element);
    }
    INVALIDATED.with(|invalidated| invalidated.borrow_mut().push(element.as_node().opaque()));
}

fn take_invalidated() -> Vec<OpaqueNode> {
    INVALIDATED.with(|invalidated| std::mem::take(&mut *invalidated.borrow_mut()))
}

/// The classes added or removed, from the snapshot.
fn classes_changed<E: TElement>(element: &E, snapshot: &Snapshot) -> SmallVec<[Atom; 8]> {
    let mut changed = SmallVec::new();
    if !snapshot.class_changed() {
        return changed;
    }
    let mut old: SmallVec<[Atom; 8]> = SmallVec::new();
    snapshot.each_class(|class| old.push(class.0.clone()));
    let mut new: SmallVec<[Atom; 8]> = SmallVec::new();
    element.each_class(|class| new.push(class.0.clone()));
    for class in old.iter() {
        if !new.contains(class) {
            changed.push(class.clone());
        }
    }
    for class in new.into_iter() {
        if !old.contains(&class) {
            changed.push(class);
        }
    }
    changed
}

/// Invalidate `:has()` anchors for the attribute, class, id and state changes recorded in
/// `snapshot_map` for `elements`. Returns whether anything was invalidated (then the traversal
/// must start at the root).
pub(crate) fn invalidate_for_snapshots<E: TElement>(
    stylist: &Stylist,
    snapshot_map: &SnapshotMap,
    elements: impl Iterator<Item = E>,
) -> bool {
    let quirks_mode = stylist.quirks_mode();
    for element in elements {
        let Some(snapshot) = snapshot_map.get(&element) else {
            continue;
        };
        let mut states = None;
        let mut classes: Option<SmallVec<[Atom; 8]>> = None;
        let invalidator = RelativeSelectorInvalidator {
            element,
            quirks_mode,
            snapshot_table: Some(snapshot_map),
            invalidated: invalidated_at::<E>,
            sibling_traversal_map: SiblingTraversalMap::default(),
            _marker: PhantomData,
        };
        invalidator.invalidate_relative_selectors_for_this(
            stylist,
            |element, scope, data, quirks_mode, collector| {
                let map = data.relative_selector_invalidation_map();
                let states = *states.get_or_insert_with(|| {
                    ElementWrapper::new(*element, snapshot_map).state_changes()
                });
                let classes =
                    classes.get_or_insert_with(|| classes_changed(element, snapshot));
                if snapshot.id_changed() {
                    let ids = [element.id().map(|id| id.clone()), snapshot.id_attr().cloned()];
                    for id in ids.iter().flatten() {
                        if let Some(dependencies) = map.id_to_selector.get(id, quirks_mode) {
                            for dependency in dependencies {
                                collector.add_dependency(dependency, *element, scope);
                            }
                        }
                    }
                }
                for class in classes.iter() {
                    if let Some(dependencies) = map.class_to_selector.get(class, quirks_mode) {
                        for dependency in dependencies {
                            collector.add_dependency(dependency, *element, scope);
                        }
                    }
                }
                snapshot.each_attr_changed(|attribute| {
                    if let Some(dependencies) = map.other_attribute_affecting_selectors.get(attribute)
                    {
                        for dependency in dependencies {
                            collector.add_dependency(dependency, *element, scope);
                        }
                    }
                });
                if !states.is_empty() {
                    map.state_affecting_selectors.lookup_with_additional(
                        *element,
                        quirks_mode,
                        None,
                        &[],
                        states,
                        |dependency| {
                            if dependency.state.intersects(states) {
                                collector.add_dependency(&dependency.dep, *element, scope);
                            }
                            true
                        },
                    );
                }
            },
        );
    }
    !take_invalidated().is_empty()
}

/// The relative selector search directions an element inherits from its parent and previous
/// sibling (Gecko's `inherit_relative_selector_search_direction`).
fn inherited_search_direction<E: TElement>(
    parent: Option<E>,
    prev_sibling: Option<E>,
) -> ElementSelectorFlags {
    let mut inherited = ElementSelectorFlags::empty();
    if let Some(parent) = parent {
        inherited |= parent
            .relative_selector_search_direction()
            .intersection(ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR);
    }
    if let Some(sibling) = prev_sibling {
        inherited |= sibling.relative_selector_search_direction().intersection(
            ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR_SIBLING,
        );
    }
    inherited
}

fn prev_sibling_element<E: TElement>(element: E) -> Option<E> {
    let mut node = element.as_node().prev_sibling();
    while let Some(sibling) = node {
        if let Some(element) = sibling.as_element() {
            return Some(element);
        }
        node = sibling.prev_sibling();
    }
    None
}

fn next_sibling_element<E: TElement>(element: E) -> Option<E> {
    let mut node = element.as_node().next_sibling();
    while let Some(sibling) = node {
        if let Some(element) = sibling.as_element() {
            return Some(element);
        }
        node = sibling.next_sibling();
    }
    None
}

/// After `element` was inserted: invalidate `:has()` anchors it may now satisfy. Returns the
/// elements script must mark dirty. (Gecko's MaybeInvalidateRelativeSelectorForInsertion, without
/// the sibling side-effect special case: a sibling-relative `:has()` across the insertion point
/// restyles the parent's subtree instead.)
pub(crate) fn invalidate_for_insertion<E: TElement>(stylist: &Stylist, element: E) -> Vec<OpaqueNode> {
    FROM_SCRIPT.with(|from_script| from_script.set(true));
    let result = invalidate_for_insertion_inner(stylist, element);
    FROM_SCRIPT.with(|from_script| from_script.set(false));
    result
}

fn invalidate_for_insertion_inner<E: TElement>(stylist: &Stylist, element: E) -> Vec<OpaqueNode> {
    let parent = element.traversal_parent();
    let prev_sibling = prev_sibling_element(element);
    let inherited = inherited_search_direction(parent, prev_sibling);
    if inherited.is_empty() {
        return Vec::new();
    }
    if inherited.intersects(ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_SIBLING) &&
        prev_sibling.is_some() &&
        next_sibling_element(element).is_some()
    {
        if let Some(parent) = parent {
            invalidate_subtree(parent);
        }
    }
    let invalidator = RelativeSelectorInvalidator {
        element,
        quirks_mode: stylist.quirks_mode(),
        snapshot_table: None,
        invalidated: invalidated_at::<E>,
        sibling_traversal_map: SiblingTraversalMap::default(),
        _marker: PhantomData,
    };
    invalidator.invalidate_relative_selectors_for_dom_mutation(
        true,
        stylist,
        inherited,
        DomMutationOperation::Insert,
    );
    take_invalidated()
}

/// Before `element` is removed: invalidate `:has()` anchors that may stop matching. Returns the
/// elements script must mark dirty. (Gecko's MaybeInvalidateRelativeSelectorForRemoval, with the
/// same simplification as insertion for the sibling side effect.)
pub(crate) fn invalidate_for_removal<E: TElement>(stylist: &Stylist, element: E) -> Vec<OpaqueNode> {
    FROM_SCRIPT.with(|from_script| from_script.set(true));
    let result = invalidate_for_removal_inner(stylist, element);
    FROM_SCRIPT.with(|from_script| from_script.set(false));
    result
}

fn invalidate_for_removal_inner<E: TElement>(stylist: &Stylist, element: E) -> Vec<OpaqueNode> {
    let search_direction = element.relative_selector_search_direction();
    if search_direction.is_empty() {
        return Vec::new();
    }
    let parent = element.traversal_parent();
    let prev_sibling = prev_sibling_element(element);
    let next_sibling = next_sibling_element(element);
    let inherited = inherited_search_direction(parent, prev_sibling);
    if inherited.is_empty() {
        return Vec::new();
    }
    if let Some(sibling) = next_sibling {
        sibling.apply_selector_flags(search_direction);
    }
    if prev_sibling.is_some() && next_sibling.is_some() {
        if let Some(parent) = parent {
            invalidate_subtree(parent);
        }
    }
    let invalidator = RelativeSelectorInvalidator {
        element,
        quirks_mode: stylist.quirks_mode(),
        snapshot_table: None,
        invalidated: invalidated_at::<E>,
        sibling_traversal_map: SiblingTraversalMap::default(),
        _marker: PhantomData,
    };
    invalidator.invalidate_relative_selectors_for_dom_mutation(
        true,
        stylist,
        inherited,
        DomMutationOperation::Remove,
    );
    take_invalidated()
}
