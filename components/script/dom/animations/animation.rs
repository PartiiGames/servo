/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use dom_struct::dom_struct;
use js::context::JSContext;
use js::rust::HandleObject;
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericBindings::AnimationBinding::{
    AnimationPlayState, AnimationReplaceState,
};
use script_bindings::codegen::GenericBindings::DocumentBinding::DocumentMethods;
use script_bindings::inheritance::Castable;
use script_bindings::num::Finite;
use script_bindings::reflector::reflect_dom_object_with_proto;
use script_bindings::root::DomRoot;
use script_bindings::str::DOMString;
use style::animation::ScriptAnimation;
use stylo_atoms::Atom;

use crate::dom::animationeffect::AnimationEffect;
use crate::dom::bindings::codegen::Bindings::AnimationBinding::AnimationMethods;
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::MutNullableDom;
use crate::dom::document::Document;
use crate::dom::element::Element;
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::keyframeeffect::KeyframeEffect;
use crate::dom::node::Node;
use crate::dom::promise::{Promise, RootedPromise, TracedPromise};
use crate::dom::types::AnimationTimeline;
use crate::dom::window::Window;

/// Source of [`Animation::uid`]: creation order, which is also the composite order of
/// script-created animations.
static NEXT_ANIMATION_UID: AtomicU64 = AtomicU64::new(1);

/// <https://drafts.csswg.org/web-animations-1/#animation>
///
/// Times are kept in seconds (the unit of the animation timeline value style uses) and converted
/// to milliseconds at the API. There are no pending play / pause tasks: play() and pause() take
/// effect at once, at the timeline's current time, so `pending` is always false and `ready` is
/// always resolved.
#[dom_struct]
pub(crate) struct Animation {
    event_target: EventTarget,

    /// <https://drafts.csswg.org/web-animations-1/#timeline>
    timeline: MutNullableDom<AnimationTimeline>,

    /// <https://drafts.csswg.org/web-animations-1/#animation-associated-effect>
    associated_effect: MutNullableDom<AnimationEffect>,

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-id>
    id: DomRefCell<DOMString>,

    /// Unique per animation; also its composite order.
    uid: u64,

    /// <https://drafts.csswg.org/web-animations-1/#animation-start-time>
    start_time: Cell<Option<f64>>,

    /// <https://drafts.csswg.org/web-animations-1/#animation-hold-time>
    hold_time: Cell<Option<f64>>,

    /// <https://drafts.csswg.org/web-animations-1/#playback-rate>
    playback_rate: Cell<f64>,

    /// <https://drafts.csswg.org/web-animations-1/#previous-current-time>
    previous_current_time: Cell<Option<f64>>,

    /// <https://drafts.csswg.org/web-animations-1/#animation-replace-state>
    replace_state: Cell<AnimationReplaceState>,

    /// <https://drafts.csswg.org/web-animations-1/#current-finished-promise>
    finished_promise: DomRefCell<Option<TracedPromise>>,

    /// <https://drafts.csswg.org/web-animations-1/#current-ready-promise>
    ready_promise: DomRefCell<Option<TracedPromise>>,

    /// Whether the finish notification steps ran for the current finished state.
    finish_notified: Cell<bool>,

    /// The element style currently applies this animation to, if any.
    applied_target: MutNullableDom<Element>,

    /// The [`KeyframeEffect::generation`] style last computed the keyframes of.
    applied_generation: Cell<Option<u64>>,
}

impl Animation {
    pub(crate) fn new_inherited() -> Self {
        Self {
            event_target: EventTarget::new_inherited(),
            timeline: Default::default(),
            associated_effect: Default::default(),
            id: Default::default(),
            uid: NEXT_ANIMATION_UID.fetch_add(1, Ordering::Relaxed),
            start_time: Cell::new(None),
            hold_time: Cell::new(None),
            playback_rate: Cell::new(1.),
            previous_current_time: Cell::new(None),
            replace_state: Cell::new(AnimationReplaceState::Active),
            finished_promise: Default::default(),
            ready_promise: Default::default(),
            finish_notified: Cell::new(false),
            applied_target: Default::default(),
            applied_generation: Cell::new(None),
        }
    }

    fn new_with_proto(
        cx: &mut JSContext,
        global: &GlobalScope,
        proto: Option<HandleObject>,
    ) -> DomRoot<Self> {
        reflect_dom_object_with_proto(cx, Box::new(Self::new_inherited()), global, proto)
    }

    pub(crate) fn new(cx: &mut JSContext, global: &GlobalScope) -> DomRoot<Self> {
        Self::new_with_proto(cx, global, None)
    }

    pub(crate) fn uid(&self) -> u64 {
        self.uid
    }

    pub(crate) fn is_persisted(&self) -> bool {
        self.replace_state.get() == AnimationReplaceState::Persisted
    }

    pub(crate) fn playback_rate(&self) -> f64 {
        self.playback_rate.get()
    }

    /// The target element of the associated keyframe effect.
    pub(crate) fn target(&self) -> Option<DomRoot<Element>> {
        self.keyframe_effect().and_then(|effect| effect.target())
    }

    /// <https://drafts.csswg.org/web-animations-1/#relevant-animation>: its effect is current
    /// (not finished) or in effect (filling).
    pub(crate) fn is_relevant(&self) -> bool {
        if self.replace_state.get() == AnimationReplaceState::Removed {
            return false;
        }
        match self.play_state() {
            AnimationPlayState::Idle => false,
            AnimationPlayState::Running | AnimationPlayState::Paused => true,
            AnimationPlayState::Finished => self.associated_effect.get().is_some_and(|effect| {
                effect
                    .script_timing()
                    .compute(self.current_time(), self.playback_rate.get())
                    .active_time
                    .is_some()
            }),
        }
    }

    pub(crate) fn set_id(&self, id: DOMString) {
        *self.id.borrow_mut() = id;
    }

    /// <https://drafts.csswg.org/web-animations-1/#animation-set-the-timeline-of-an-animation>
    fn set_the_timeline(&self, cx: &mut JSContext, timeline: Option<&AnimationTimeline>) {
        // Step 1. Let old timeline be the current timeline of animation, if any.
        // Step 2. If new timeline is the same object as old timeline, abort this procedure.
        if self.timeline.get().as_deref() == timeline {
            return;
        }
        // Step 3. Let previous play state be animation's play state.
        // Step 4. Let previous current time be the animation's current time.
        let previous_current_time = self.current_time();

        // Step 5-6. Set the timeline of animation to new timeline.
        self.timeline.set(timeline);

        // Step 7. If the start time of animation is resolved, make animation's hold time
        // unresolved. (Monotonic timelines only, which is all we have.)
        if self.start_time.get().is_some() {
            self.hold_time.set(None);
        }
        if timeline.is_none() && previous_current_time.is_some() && self.hold_time.get().is_none()
        {
            // Without a timeline the animation keeps its current time.
            self.start_time.set(None);
            self.hold_time.set(previous_current_time);
        }

        // Step 8. Run the procedure to update an animation's finished state for animation with
        // the did seek flag set to false, and the synchronously notify flag set to false.
        self.update_finished_state(cx, false, false);
        self.apply_to_style(cx);
    }

    /// <https://drafts.csswg.org/web-animations-1/#animation-set-the-associated-effect-of-an-animation>
    fn set_the_associated_effect(&self, cx: &mut JSContext, effect: Option<&AnimationEffect>) {
        // Step 1. Let old effect be the current associated effect of animation, if any.
        let old_effect = self.associated_effect.get();
        // Step 2. If new effect is the same object as old effect, abort this procedure.
        if old_effect.as_deref() == effect {
            return;
        }

        // Step 4. If new effect is not null and if new effect is the associated effect of another
        // animation, previous animation, run the procedure to set the associated effect of an
        // animation on previous animation passing null as new effect.
        if let Some(previous_animation) = effect.and_then(|effect| effect.animation()) {
            previous_animation.set_the_associated_effect(cx, None);
        }

        // Step 5. Let the associated effect of animation be new effect.
        if let Some(old_effect) = old_effect {
            old_effect.set_animation(None);
        }
        self.associated_effect.set(effect);
        if let Some(effect) = effect {
            effect.set_animation(Some(self));
        }
        self.applied_generation.set(None);

        // Step 6. Run the procedure to update an animation's finished state for animation with
        // the did seek flag set to false, and the synchronously notify flag set to false.
        self.update_finished_state(cx, false, false);
        self.apply_to_style(cx);
    }

    fn timeline_time(&self) -> Option<f64> {
        self.timeline
            .get()
            .map(|timeline| timeline.current_time_in_seconds())
    }

    fn keyframe_effect(&self) -> Option<DomRoot<KeyframeEffect>> {
        self.associated_effect
            .get()
            .and_then(|effect| DomRoot::downcast::<KeyframeEffect>(effect))
    }

    /// <https://drafts.csswg.org/web-animations-1/#associated-effect-end>
    fn effect_end(&self) -> f64 {
        self.associated_effect
            .get()
            .map_or(0., |effect| effect.script_timing().end_time())
    }

    /// <https://drafts.csswg.org/web-animations-1/#animation-current-time>, in seconds.
    pub(crate) fn current_time(&self) -> Option<f64> {
        if let Some(hold_time) = self.hold_time.get() {
            return Some(hold_time);
        }
        let start_time = self.start_time.get()?;
        let timeline_time = self.timeline_time()?;
        Some((timeline_time - start_time) * self.playback_rate.get())
    }

    /// <https://drafts.csswg.org/web-animations-1/#play-states>
    fn play_state(&self) -> AnimationPlayState {
        let current_time = self.current_time();
        if current_time.is_none() && self.start_time.get().is_none() {
            return AnimationPlayState::Idle;
        }
        if self.start_time.get().is_none() {
            return AnimationPlayState::Paused;
        }
        let rate = self.playback_rate.get();
        if let Some(current_time) = current_time {
            if (rate > 0. && current_time >= self.effect_end()) || (rate < 0. && current_time <= 0.)
            {
                return AnimationPlayState::Finished;
            }
        }
        AnimationPlayState::Running
    }

    /// <https://drafts.csswg.org/web-animations-1/#silently-set-the-current-time>
    fn silently_set_the_current_time(&self, seek_time: Option<f64>) -> Fallible<()> {
        // Step 1. If seek time is an unresolved time value, then perform the following steps.
        let Some(seek_time) = seek_time else {
            // Step 1.1. If the current time is resolved, then throw a TypeError.
            if self.current_time().is_some() {
                return Err(Error::Type(
                    c"currentTime cannot be set to null on a playing animation".to_owned(),
                ));
            }
            // Step 1.2. Abort these steps.
            return Ok(());
        };

        // Step 2. Update either animation's hold time or start time as follows:
        // If any of the following conditions are true: animation's hold time is resolved, or
        // animation's start time is unresolved, or animation has no associated timeline or the
        // associated timeline is inactive, or animation's playback rate is 0, set animation's
        // hold time to seek time.
        let rate = self.playback_rate.get();
        let timeline_time = self.timeline_time();
        match timeline_time {
            Some(timeline_time)
                if self.hold_time.get().is_none() && self.start_time.get().is_some() && rate != 0. =>
            {
                // Otherwise, set animation's start time to the result of evaluating
                // timeline time - (seek time / playback rate).
                self.start_time.set(Some(timeline_time - seek_time / rate));
            },
            _ => self.hold_time.set(Some(seek_time)),
        }

        // Step 3. If animation has no associated timeline or the associated timeline is inactive,
        // make animation's start time unresolved.
        if timeline_time.is_none() {
            self.start_time.set(None);
        }

        // Step 4. Make animation's previous current time unresolved.
        self.previous_current_time.set(None);
        Ok(())
    }

    /// <https://drafts.csswg.org/web-animations-1/#set-the-current-time>
    fn set_the_current_time(&self, cx: &mut JSContext, seek_time: Option<f64>) -> Fallible<()> {
        // Step 1. Run the steps to silently set the current time of animation to seek time.
        self.silently_set_the_current_time(seek_time)?;
        // Step 2-3: no pending pause task.
        // Step 4. Run the procedure to update an animation's finished state for animation with
        // the did seek flag set to true, and the synchronously notify flag set to false.
        self.update_finished_state(cx, true, false);
        self.apply_to_style(cx);
        Ok(())
    }

    /// <https://drafts.csswg.org/web-animations-1/#playing-an-animation-section>
    fn play_an_animation(&self, cx: &mut JSContext, auto_rewind: bool) -> Fallible<()> {
        let rate = self.playback_rate.get();
        let current_time = self.current_time();
        let end = self.effect_end();

        // Step 4. Let seek time be a time value that is initially unresolved.
        let mut seek_time = None;
        // Step 5. If the auto-rewind flag is true, perform the steps corresponding to the first
        // matching condition from the following, if any:
        if auto_rewind {
            if rate >= 0. && current_time.is_none_or(|time| time < 0. || time >= end) {
                seek_time = Some(0.);
            } else if rate < 0. && current_time.is_none_or(|time| time <= 0. || time > end) {
                if end.is_infinite() {
                    return Err(Error::InvalidState(None));
                }
                seek_time = Some(end);
            }
        }

        // Step 6. If the following three conditions are all satisfied: seek time is unresolved,
        // and animation's start time is unresolved, and animation's current time is unresolved,
        // set seek time to zero.
        if seek_time.is_none() && self.start_time.get().is_none() && current_time.is_none() {
            seek_time = Some(0.);
        }

        // Step 8. If seek time is resolved, set animation's hold time to seek time and make the
        // start time unresolved.
        if let Some(seek_time) = seek_time {
            self.hold_time.set(Some(seek_time));
            self.start_time.set(None);
        }

        // Steps 9-12: the pending play task runs at once (no pending state here).
        // <https://drafts.csswg.org/web-animations-1/#playing-an-animation-section>, "commit
        // pending play": with a resolved hold time, start time = ready time - hold time / rate.
        if let (Some(hold_time), Some(ready_time)) = (self.hold_time.get(), self.timeline_time())
        {
            if rate == 0. {
                self.start_time.set(Some(ready_time));
            } else {
                self.start_time.set(Some(ready_time - hold_time / rate));
                self.hold_time.set(None);
            }
        }

        // Step 13. Run the procedure to update an animation's finished state for animation with
        // the did seek flag set to false, and the synchronously notify flag set to false.
        self.update_finished_state(cx, false, false);
        self.apply_to_style(cx);
        Ok(())
    }

    /// <https://drafts.csswg.org/web-animations-1/#pausing-an-animation-section>
    fn pause_an_animation(&self, cx: &mut JSContext) -> Fallible<()> {
        // Step 1. If animation has a pending pause task, abort these steps.
        // Step 2. If the play state of animation is paused, abort these steps.
        if self.play_state() == AnimationPlayState::Paused {
            return Ok(());
        }

        // Step 3-4. If the animation's current time is unresolved, seek to 0 (or the end when
        // playing backwards).
        let mut seek_time = None;
        if self.current_time().is_none() {
            if self.playback_rate.get() >= 0. {
                seek_time = Some(0.);
            } else {
                let end = self.effect_end();
                if end.is_infinite() {
                    return Err(Error::InvalidState(None));
                }
                seek_time = Some(end);
            }
        }

        // Step 5. If seek time is resolved, set the hold time to seek time.
        if let Some(seek_time) = seek_time {
            self.hold_time.set(Some(seek_time));
        }

        // Step 7-10: the pending pause task runs at once.
        // <https://drafts.csswg.org/web-animations-1/#pause-an-animation>, "commit pending
        // pause": if start time is resolved and hold time is not, hold time = current time.
        if let (Some(start_time), None) = (self.start_time.get(), self.hold_time.get()) {
            if let Some(ready_time) = self.timeline_time() {
                self.hold_time
                    .set(Some((ready_time - start_time) * self.playback_rate.get()));
            }
        }
        self.start_time.set(None);

        // Step 11. Run the procedure to update an animation's finished state for animation with
        // the did seek flag set to false, and the synchronously notify flag set to false.
        self.update_finished_state(cx, false, false);
        self.apply_to_style(cx);
        Ok(())
    }

    /// <https://drafts.csswg.org/web-animations-1/#update-an-animations-finished-state>
    fn update_finished_state(&self, cx: &mut JSContext, did_seek: bool, _synchronously: bool) {
        let rate = self.playback_rate.get();

        // Step 1. Let the unconstrained current time be the result of calculating the current
        // time substituting an unresolved time value for the hold time if did seek is false. If
        // did seek is true, the unconstrained current time is equal to the current time.
        let unconstrained_current_time = if did_seek {
            self.current_time()
        } else {
            self.start_time
                .get()
                .zip(self.timeline_time())
                .map(|(start_time, timeline_time)| (timeline_time - start_time) * rate)
        };

        // Step 2. If all three of the following conditions are true: the unconstrained current
        // time is resolved, and animation's start time is resolved, and animation does not have a
        // pending play task or a pending pause task, then update the hold time.
        if let (Some(unconstrained), Some(_)) = (unconstrained_current_time, self.start_time.get())
        {
            let end = self.effect_end();
            let previous = self.previous_current_time.get();
            if rate > 0. && unconstrained >= end {
                self.hold_time.set(Some(if did_seek {
                    unconstrained
                } else {
                    previous.map_or(end, |previous| previous.max(end))
                }));
            } else if rate < 0. && unconstrained <= 0. {
                self.hold_time.set(Some(if did_seek {
                    unconstrained
                } else {
                    previous.map_or(0., |previous| previous.min(0.))
                }));
            } else if rate != 0. {
                if let (true, Some(hold_time), Some(timeline_time)) =
                    (did_seek, self.hold_time.get(), self.timeline_time())
                {
                    self.start_time.set(Some(timeline_time - hold_time / rate));
                }
                self.hold_time.set(None);
            }
        }

        // Step 3. Set the previous current time of animation be the result of calculating its
        // current time.
        self.previous_current_time.set(self.current_time());

        // Step 4. Let current finished state be true if the play state of animation is
        // finished. Otherwise, let it be false.
        let finished = self.play_state() == AnimationPlayState::Finished;

        // Step 5. If current finished state is true and the current finished promise is not yet
        // resolved, run the finish notification steps.
        if finished {
            if !self.finish_notified.get() {
                self.finish_notification_steps(cx);
            }
            return;
        }

        // Step 6. If current finished state is false and animation's current finished promise
        // is already resolved, set animation's current finished promise to a new promise.
        self.finish_notified.set(false);
        let fulfilled = self
            .finished_promise
            .borrow()
            .as_ref()
            .is_some_and(|promise| promise.is_fulfilled());
        if fulfilled {
            *self.finished_promise.borrow_mut() = None;
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#finish-notification-steps>
    fn finish_notification_steps(&self, cx: &mut JSContext) {
        self.finish_notified.set(true);

        // Step 2. Resolve animation's current finished promise object with animation.
        if let Some(promise) = self.finished_promise.borrow().as_ref() {
            if promise.is_pending() {
                promise.root(cx).resolve_native(cx, &DomRoot::from_ref(self));
            }
        }

        // Step 3. Create an AnimationPlaybackEvent, finishEvent, of type finish, with
        // currentTime the current time of animation and timelineTime the timeline's time.
        // Step 4-5. Queue it (sent with the other animation events).
        self.queue_event(Atom::from("finish"), self.current_time());
    }

    fn queue_event(&self, event_type: Atom, current_time: Option<f64>) {
        let Some(document) = self.document() else {
            return;
        };
        document.animation_manager().queue_playback_event(
            self,
            event_type,
            current_time.map(|time| time * 1000.),
            self.timeline_time().map(|time| time * 1000.),
        );
    }

    fn document(&self) -> Option<DomRoot<Document>> {
        let global = self.global();
        global.downcast::<Window>().map(|window| window.Document())
    }

    /// The timing or keyframes of the associated effect changed.
    pub(crate) fn effect_timing_changed(&self, cx: &mut JSContext) {
        self.update_finished_state(cx, false, false);
        self.apply_to_style(cx);
    }

    /// Run once per animation frame for registered animations: finish notifications.
    pub(crate) fn update_for_new_timeline_value(&self, cx: &mut JSContext) {
        let was_finished = self.finish_notified.get();
        let hold_time = self.hold_time.get();
        self.update_finished_state(cx, false, false);
        if self.hold_time.get() != hold_time || was_finished != self.finish_notified.get() {
            self.apply_to_style(cx);
        }
    }

    /// The animation was removed because later animations replace all of its properties.
    /// <https://drafts.csswg.org/web-animations-1/#removing-replaced-animations>
    pub(crate) fn remove_replaced(&self, cx: &mut JSContext) {
        self.replace_state.set(AnimationReplaceState::Removed);
        self.applied_target.set(None);
        self.applied_generation.set(None);
        if let Some(document) = self.document() {
            document.animation_manager().unregister_script_animation(self);
        }
        let _ = cx;
        self.queue_event(Atom::from("remove"), self.current_time());
    }

    /// Bring style's copy of this animation up to date: add, replace or remove it.
    fn apply_to_style(&self, cx: &mut JSContext) {
        let Some(document) = self.document() else {
            return;
        };
        let manager = document.animation_manager();
        let effect = self.keyframe_effect();
        let target = effect.as_ref().and_then(|effect| effect.target());
        // Idle, removed, and finished without a fill: nothing to apply, nothing to keep alive.
        let inactive = !self.is_relevant();

        // Leave the previous target if the target changed or the animation no longer applies.
        if let Some(previous) = self.applied_target.get() {
            if target.as_deref() != Some(&*previous) || inactive {
                manager.remove_script_animation(cx.no_gc(), previous.upcast::<Node>(), self.uid);
                self.applied_target.set(None);
                self.applied_generation.set(None);
            }
        }

        let (Some(effect), Some(target)) = (effect, target) else {
            if inactive {
                manager.unregister_script_animation(self);
            } else {
                manager.register_script_animation(self);
            }
            return;
        };
        if inactive {
            manager.unregister_script_animation(self);
            return;
        }
        // Style only runs in the target's own document.
        if target.upcast::<Node>().owner_doc() != document {
            return;
        }

        let generation = effect.generation();
        let keyframes_changed = self.applied_generation.get() != Some(generation);
        let animation_effect = effect.upcast::<AnimationEffect>();
        let animation = ScriptAnimation::new(
            self.uid,
            effect.keyframes_for_style(),
            effect.composite_for_style(),
            effect.upcast::<AnimationEffect>().script_timing(),
            self.start_time.get(),
            self.hold_time.get(),
            self.playback_rate.get(),
        );
        manager.set_script_animation(
            cx.no_gc(),
            target.upcast::<Node>(),
            animation,
            keyframes_changed,
        );
        self.applied_target.set(Some(&target));
        self.applied_generation.set(Some(generation));
        manager.register_script_animation(self);
    }
}

impl AnimationMethods<crate::DomTypeHolder> for Animation {
    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-animation>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        effect: Option<&AnimationEffect>,
        timeline: Option<Option<&AnimationTimeline>>,
    ) -> DomRoot<Self> {
        // Step 1. Let animation be a new Animation object.
        let animation = Animation::new_with_proto(cx, window.upcast(), proto);

        // Step 2. Run the procedure to set the timeline of an animation on animation passing timeline
        // as the new timeline; or, if the timeline argument is missing, passing the default document
        // timeline of the Document associated with the Window that is the current global object.
        match timeline {
            Some(timeline) => animation.set_the_timeline(cx, timeline),
            None => {
                let document = window.Document();
                let timeline = document.Timeline();
                animation.set_the_timeline(cx, Some(timeline.upcast()));
            },
        }

        // Step 3. Run the procedure to set the associated effect of an animation on animation passing
        // source as the new effect.
        animation.set_the_associated_effect(cx, effect);

        animation
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-id>
    fn Id(&self) -> DOMString {
        self.id.borrow().clone()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-id>
    fn SetId(&self, _cx: &mut JSContext, id: DOMString) {
        self.set_id(id);
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-effect>
    fn GetEffect(&self) -> Option<DomRoot<AnimationEffect>> {
        self.associated_effect.get()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-effect>
    fn SetEffect(&self, cx: &mut JSContext, effect: Option<&AnimationEffect>) {
        // > Setting this attribute updates the object’s associated effect using
        // > the procedure to set the associated effect of an animation.
        self.set_the_associated_effect(cx, effect);
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-animation-effect-timeline-timeline>
    fn GetTimeline(&self) -> Option<DomRoot<AnimationTimeline>> {
        self.timeline.get()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-animation-effect-timeline-timeline>
    fn SetTimeline(&self, cx: &mut JSContext, timeline: Option<&AnimationTimeline>) {
        // > Setting this attribute updates the object’s timeline using the procedure
        // > to set the timeline of an animation.
        self.set_the_timeline(cx, timeline);
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-starttime>
    fn GetStartTime(&self) -> Option<Finite<f64>> {
        self.start_time
            .get()
            .and_then(|time| Finite::new(time * 1000.))
    }

    /// <https://drafts.csswg.org/web-animations-1/#set-the-start-time>
    fn SetStartTime(&self, cx: &mut JSContext, new_start_time: Option<Finite<f64>>) {
        let new_start_time = new_start_time.map(|time| *time / 1000.);
        // Step 1. Let timeline time be the current time value of the timeline that animation is
        // associated with.
        let timeline_time = self.timeline_time();
        // Step 2. If timeline time is unresolved and new start time is resolved, make animation's
        // hold time unresolved.
        if timeline_time.is_none() && new_start_time.is_some() {
            self.hold_time.set(None);
        }
        // Step 3. Let previous current time be animation's current time.
        let previous_current_time = self.current_time();
        // Step 5. Set animation's start time to new start time.
        self.start_time.set(new_start_time);
        // Step 6. Update animation's hold time based on the first matching condition.
        match new_start_time {
            Some(_) => {
                if self.playback_rate.get() != 0. {
                    self.hold_time.set(None);
                }
            },
            None => self.hold_time.set(previous_current_time),
        }
        // Step 8. Run the procedure to update an animation's finished state for animation with
        // the did seek flag set to true, and the synchronously notify flag set to false.
        self.update_finished_state(cx, true, false);
        self.apply_to_style(cx);
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-currenttime>
    fn GetCurrentTime(&self) -> Option<Finite<f64>> {
        self.current_time()
            .and_then(|time| Finite::new(time * 1000.))
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-currenttime>
    fn SetCurrentTime(
        &self,
        cx: &mut JSContext,
        current_time: Option<Finite<f64>>,
    ) -> Fallible<()> {
        self.set_the_current_time(cx, current_time.map(|time| *time / 1000.))
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-playbackrate>
    fn PlaybackRate(&self) -> Finite<f64> {
        Finite::wrap(self.playback_rate.get())
    }

    /// <https://drafts.csswg.org/web-animations-1/#set-the-playback-rate>
    fn SetPlaybackRate(&self, cx: &mut JSContext, rate: Finite<f64>) {
        // Step 2. Let previous time be the value of the current time of animation before
        // changing the playback rate.
        let previous_time = self.current_time();
        // Step 4. Set the playback rate to new playback rate.
        self.playback_rate.set(*rate);
        // Step 5. If previous time is resolved, set the current time of animation to previous
        // time.
        if previous_time.is_some() {
            let _ = self.set_the_current_time(cx, previous_time);
        } else {
            self.apply_to_style(cx);
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-playstate>
    fn PlayState(&self) -> AnimationPlayState {
        self.play_state()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-replacestate>
    fn ReplaceState(&self) -> AnimationReplaceState {
        self.replace_state.get()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-pending>
    fn Pending(&self) -> bool {
        false
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-ready>
    fn Ready(&self, cx: &mut JSContext) -> RootedPromise {
        if let Some(promise) = self.ready_promise.borrow().as_ref() {
            return promise.root(cx);
        }
        let promise = Promise::new_resolved(cx, &self.global(), DomRoot::from_ref(self));
        *self.ready_promise.borrow_mut() = Some(promise.to_traced());
        promise
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-finished>
    fn Finished(&self, cx: &mut JSContext) -> RootedPromise {
        if let Some(promise) = self.finished_promise.borrow().as_ref() {
            return promise.root(cx);
        }
        let promise = if self.finish_notified.get() {
            Promise::new_resolved(cx, &self.global(), DomRoot::from_ref(self))
        } else {
            Promise::new(cx, &self.global())
        };
        *self.finished_promise.borrow_mut() = Some(promise.to_traced());
        promise
    }

    event_handler!(finish, GetOnfinish, SetOnfinish);
    event_handler!(cancel, GetOncancel, SetOncancel);
    event_handler!(remove, GetOnremove, SetOnremove);

    /// <https://drafts.csswg.org/web-animations-1/#cancel-an-animation>
    fn Cancel(&self, cx: &mut JSContext) {
        // Step 1. If animation's play state is not idle, perform the following steps:
        if self.play_state() != AnimationPlayState::Idle {
            // Step 1.2. Reject the current finished promise with a DOMException named
            // "AbortError", and mark it handled.
            let pending = self
                .finished_promise
                .borrow()
                .as_ref()
                .is_some_and(|promise| promise.is_pending());
            if pending {
                let promise = self.finished_promise.borrow_mut().take();
                if let Some(promise) = promise {
                    let promise = promise.root(cx);
                    promise.reject_error(cx, Error::Abort(None));
                    promise.set_promise_is_handled(cx);
                }
            }
            // Step 1.4. Let current finished promise be a new promise.
            *self.finished_promise.borrow_mut() = None;
            self.finish_notified.set(false);

            // Step 1.5-1.7. Queue a cancel event with an unresolved current time.
            self.queue_event(Atom::from("cancel"), None);
        }

        // Step 2. Make animation's hold time unresolved.
        self.hold_time.set(None);
        // Step 3. Make animation's start time unresolved.
        self.start_time.set(None);
        self.previous_current_time.set(None);
        self.apply_to_style(cx);
    }

    /// <https://drafts.csswg.org/web-animations-1/#finish-an-animation>
    fn Finish(&self, cx: &mut JSContext) -> Fallible<()> {
        let rate = self.playback_rate.get();
        let end = self.effect_end();
        // Step 1. If animation's effective playback rate is zero, or if animation's effective
        // playback rate > 0 and associated effect end is infinity, throw an "InvalidStateError".
        if rate == 0. || (rate > 0. && end.is_infinite()) {
            return Err(Error::InvalidState(None));
        }
        // Step 3. Set limit as follows: playback rate > 0: associated effect end; otherwise 0.
        let limit = if rate > 0. { end } else { 0. };
        // Step 4. Silently set the current time to limit.
        self.silently_set_the_current_time(Some(limit))?;
        // Step 5. If animation's start time is unresolved and animation has an associated active
        // timeline, let the start time be timeline time - (limit / playback rate).
        if self.start_time.get().is_none() {
            if let Some(timeline_time) = self.timeline_time() {
                self.start_time.set(Some(timeline_time - limit / rate));
            }
        }
        // Step 8. Run the procedure to update an animation's finished state for animation with
        // the did seek flag set to true, and the synchronously notify flag set to true.
        self.update_finished_state(cx, true, true);
        self.apply_to_style(cx);
        Ok(())
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-play>
    fn Play(&self, cx: &mut JSContext) -> Fallible<()> {
        self.play_an_animation(cx, true)
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-pause>
    fn Pause(&self, cx: &mut JSContext) -> Fallible<()> {
        self.pause_an_animation(cx)
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-updateplaybackrate>
    fn UpdatePlaybackRate(&self, cx: &mut JSContext, rate: Finite<f64>) {
        // There are no pending tasks, so this is "set the playback rate" followed by an update
        // of the finished state.
        self.SetPlaybackRate(cx, rate);
        self.update_finished_state(cx, false, false);
        self.apply_to_style(cx);
    }

    /// <https://drafts.csswg.org/web-animations-1/#reverse-an-animation>
    fn Reverse(&self, cx: &mut JSContext) -> Fallible<()> {
        // Step 1. If there is no timeline associated with animation, or the associated timeline
        // is inactive throw an "InvalidStateError" DOMException and abort these steps.
        if self.timeline_time().is_none() {
            return Err(Error::InvalidState(None));
        }
        let original_rate = self.playback_rate.get();
        // Step 3. Set the playback rate to the additive inverse of its value, keeping the
        // current time.
        let previous_time = self.current_time();
        self.playback_rate.set(-original_rate);
        if previous_time.is_some() {
            self.silently_set_the_current_time(previous_time)?;
        }
        // Step 4. Run the steps to play an animation for animation with the auto-rewind flag set
        // to true. If the steps to play an animation throw an exception, restore the playback
        // rate and rethrow.
        if let Err(error) = self.play_an_animation(cx, true) {
            self.playback_rate.set(original_rate);
            if previous_time.is_some() {
                let _ = self.silently_set_the_current_time(previous_time);
            }
            self.apply_to_style(cx);
            return Err(error);
        }
        Ok(())
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-persist>
    fn Persist(&self) {
        self.replace_state.set(AnimationReplaceState::Persisted);
    }
}
