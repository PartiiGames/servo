/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::ffi::CString;
use std::ops::ControlFlow;
use std::ptr::{self, NonNull};
use std::sync::LazyLock;

use cssparser::Parser;
use dom_struct::dom_struct;
use js::context::JSContext;
use js::conversions::{
    ConversionResult, FromJSValConvertible, ToJSValConvertible, jsstr_to_string,
};
use js::gc::{HandleValue, RootedVec};
use js::jsapi::{HandleId, Heap, JS_GetPropertyById, JSITER_OWNONLY, JSObject, JSPROP_ENUMERATE};
use js::jsval::{ObjectValue, UndefinedValue};
use js::rust::wrappers2::{GetPropertyKeys, JS_DefineProperty, JS_IdToValue, JS_NewObject};
use js::rust::{
    ForOfIterationFailure, HandleObject, IdVector, IntoHandle, IntoMutableHandle, for_of,
};
use rustc_hash::FxHashMap;
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericBindings::AnimationEffectBinding::OptionalEffectTiming;
use script_bindings::codegen::GenericBindings::KeyframeEffectBinding::{
    BaseKeyframe, CompositeOperation, CompositeOperationOrAuto,
};
use script_bindings::codegen::GenericBindings::WindowBinding::WindowMethods;
use script_bindings::codegen::GenericUnionTypes::UnrestrictedDoubleOrKeyframeEffectOptions;
use script_bindings::conversions::StringificationBehavior;
use script_bindings::error::{Error, Fallible};
use script_bindings::inheritance::Castable;
use script_bindings::num::Finite;
use script_bindings::reflector::reflect_dom_object_with_proto;
use script_bindings::root::DomRoot;
use script_bindings::str::DOMString;
use style::animation::ScriptKeyframe;
use style::parser::{Parse, ParserContext};
use style::properties::longhands::animation_composition::computed_value::single_value::T as AnimationComposition;
use style::properties::generated::PropertyDeclaration;
use style::properties::{
    Importance, LonghandId, NonCustomPropertyId, PropertyDeclarationBlock, PropertyId,
    SourcePropertyDeclaration,
};
use style::stylesheets::CssRuleType;
use style::values::computed::TimingFunction as ComputedTimingFunction;
use style::values::specified::TimingFunction;
use style_traits::{CssWriter, ParsingMode, ToCss};

use crate::css::css::parser_context_for_document;
use crate::dom::Document;
use crate::dom::animationeffect::AnimationEffect;
use crate::dom::bindings::codegen::Bindings::KeyframeEffectBinding::{
    BaseComputedKeyframe, KeyframeEffectMethods,
};
use crate::dom::bindings::codegen::UnionTypes::UnrestrictedDoubleOrString;
use crate::dom::bindings::root::MutNullableDom;
use crate::dom::element::Element;
use crate::dom::window::Window;

/// <https://drafts.csswg.org/web-animations-1/#keyframeeffect>
#[dom_struct]
pub(crate) struct KeyframeEffect {
    animationeffect: AnimationEffect,

    /// <https://drafts.csswg.org/web-animations-1/#effect-target-target-element>
    // FIXME: Store a target pseudo-selector
    // to fully match the concept of the effect target
    //
    // https://drafts.csswg.org/web-animations-1/#effect-target-target-pseudo-selector
    // https://drafts.csswg.org/web-animations-1/#keyframe-effect-effect-target.
    target_element: MutNullableDom<Element>,

    /// <https://drafts.csswg.org/web-animations-1/#keyframe>
    keyframes: DomRefCell<Vec<Keyframe>>,

    /// <https://drafts.csswg.org/web-animations-1/#effect-composite-operation>
    composite: Cell<CompositeOperation>,

    /// Bumped whenever the keyframes or the target change, so the animation knows when style
    /// has to recompute the keyframe values.
    generation: Cell<u64>,
}

impl KeyframeEffect {
    pub(crate) fn new_inherited(window: &Window) -> Self {
        Self {
            animationeffect: AnimationEffect::new_inherited(window),
            target_element: Default::default(),
            keyframes: Default::default(),
            composite: Cell::new(CompositeOperation::Replace),
            generation: Cell::new(0),
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#effect-target-target-element>
    pub(crate) fn target(&self) -> Option<DomRoot<Element>> {
        self.target_element.get()
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.get()
    }

    pub(crate) fn composite_for_style(&self) -> AnimationComposition {
        composite_for_style(self.composite.get())
    }

    fn changed(&self, cx: &mut JSContext) {
        self.generation.set(self.generation.get() + 1);
        if let Some(animation) = self.upcast::<AnimationEffect>().animation() {
            animation.effect_timing_changed(cx);
        }
    }

    /// The keyframes as style uses them: computed offsets, parsed easing functions, one
    /// declaration block per keyframe.
    /// <https://drafts.csswg.org/web-animations-1/#compute-missing-keyframe-offsets>
    pub(crate) fn keyframes_for_style(&self) -> Vec<ScriptKeyframe> {
        let keyframes = self.keyframes.borrow();
        let offsets =
            compute_missing_keyframe_offsets(&keyframes.iter().map(|k| k.offset).collect::<Vec<_>>());
        let document = self.upcast::<AnimationEffect>().window().Document();
        let urlextradata = document.url().into_url().into();
        let parser_context = parser_context_for_document(
            &document,
            CssRuleType::Style,
            ParsingMode::DEFAULT,
            &urlextradata,
        );
        keyframes
            .iter()
            .zip(offsets)
            .map(|(keyframe, offset)| {
                let mut declarations = PropertyDeclarationBlock::new();
                for pair in keyframe.declarations.iter() {
                    for declaration in pair.block.declarations() {
                        declarations.push(declaration.clone(), Importance::Normal);
                    }
                }
                ScriptKeyframe {
                    offset,
                    timing_function: parse_easing(&keyframe.easing_function.str(), &parser_context)
                        .map(|easing| easing.to_computed_value_without_context())
                        .unwrap_or_else(ComputedTimingFunction::linear),
                    composite: match keyframe.composite {
                        CompositeOperationOrAuto::Auto => None,
                        CompositeOperationOrAuto::Replace => Some(AnimationComposition::Replace),
                        CompositeOperationOrAuto::Add => Some(AnimationComposition::Add),
                        CompositeOperationOrAuto::Accumulate => {
                            Some(AnimationComposition::Accumulate)
                        },
                    },
                    declarations,
                }
            })
            .collect()
    }

    fn new_with_proto(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
    ) -> DomRoot<Self> {
        reflect_dom_object_with_proto(cx, Box::new(Self::new_inherited(window)), window, proto)
    }

    pub(crate) fn new(cx: &mut JSContext, window: &Window) -> DomRoot<Self> {
        Self::new_with_proto(cx, window, None)
    }
}

impl KeyframeEffectMethods<crate::DomTypeHolder> for KeyframeEffect {
    /// <https://drafts.csswg.org/web-animations-1/#dom-keyframeeffect-keyframeeffect>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        _: Option<HandleObject>,
        target: Option<&Element>,
        keyframes: *mut JSObject,
        options: UnrestrictedDoubleOrKeyframeEffectOptions,
    ) -> Fallible<DomRoot<KeyframeEffect>> {
        // Step 1. Create a new KeyframeEffect object, effect.
        let effect = KeyframeEffect::new(cx, window);

        // Step 2. Set the target element of effect to target.
        effect.target_element.set(target);

        // TODO: Step 3. Set the target pseudo-selector to the result corresponding to
        // the first matching condition below:

        // Step 4. Let timing input be the result corresponding to the first matching
        // condition below:
        // If options is a KeyframeEffectOptions object, let timing input be options.
        // Otherwise, if options is a double, let timing input be a new EffectTiming object with all
        // members set to their default values and duration set to options.
        let (timing, composite) = match &options {
            UnrestrictedDoubleOrKeyframeEffectOptions::UnrestrictedDouble(duration) => (
                OptionalEffectTiming {
                    delay: Some(Finite::wrap(0.)),
                    endDelay: Some(Finite::wrap(0.)),
                    fill: None,
                    iterationStart: Some(Finite::wrap(0.)),
                    iterations: Some(1.),
                    duration: Some(UnrestrictedDoubleOrString::UnrestrictedDouble(*duration)),
                    direction: None,
                    easing: None,
                },
                CompositeOperation::Replace,
            ),
            UnrestrictedDoubleOrKeyframeEffectOptions::KeyframeEffectOptions(options) => {
                let timing = &options.parent;
                (
                    OptionalEffectTiming {
                        delay: Some(timing.delay),
                        endDelay: Some(timing.endDelay),
                        fill: Some(timing.fill),
                        iterationStart: Some(timing.iterationStart),
                        iterations: Some(timing.iterations),
                        duration: Some(match &timing.duration {
                            UnrestrictedDoubleOrString::UnrestrictedDouble(duration) => {
                                UnrestrictedDoubleOrString::UnrestrictedDouble(*duration)
                            },
                            UnrestrictedDoubleOrString::String(string) => {
                                UnrestrictedDoubleOrString::String(string.clone())
                            },
                        }),
                        direction: Some(timing.direction),
                        easing: Some(timing.easing.clone()),
                    },
                    options.composite,
                )
            },
        };

        // Step 5. Call the procedure to update the timing properties of an animation effect of
        // effect from timing input.
        // If that procedure causes an exception to be thrown, propagate the exception and abort this procedure.
        effect
            .upcast::<AnimationEffect>()
            .update_the_timing_properties(&timing)?;

        // Step 6. If options is a KeyframeEffectOptions object, assign the composite property of effect
        // to the corresponding value from options.
        effect.composite.set(composite);

        // Step 7. Initialize the set of keyframes by performing the procedure defined for setKeyframes()
        // passing keyframes as the input.
        let document = window.Document();
        *effect.keyframes.safe_borrow_mut(cx.no_gc()) =
            process_a_keyframes_argument(cx, &document, keyframes)?;

        Ok(effect)
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-keyframeeffect-target>
    fn GetTarget(&self) -> Option<DomRoot<Element>> {
        self.target_element.get()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-keyframeeffect-target>
    fn SetTarget(&self, cx: &mut JSContext, target: Option<&Element>) {
        if self.target_element.get().as_deref() == target {
            return;
        }
        self.target_element.set(target);
        self.changed(cx);
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-keyframeeffect-composite>
    fn Composite(&self) -> CompositeOperation {
        self.composite.get()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-keyframeeffect-composite>
    fn SetComposite(&self, cx: &mut JSContext, composite: CompositeOperation) {
        self.composite.set(composite);
        self.changed(cx);
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-keyframeeffect-getkeyframes>
    #[expect(unsafe_code)]
    fn GetKeyframes(
        &self,
        cx: &mut JSContext,
        result: &mut RootedVec<'_, Box<Heap<*mut JSObject>>>,
    ) -> Fallible<()> {
        let mut layout = self.upcast::<AnimationEffect>().window().layout_mut();
        let stylist = layout.stylist_mut();

        // Step 1. Let result be an empty sequence of objects.
        debug_assert!(result.is_empty());

        // Step 2. Let keyframes be one of the following:
        // If this keyframe effect is associated with a CSSAnimation, and its keyframes have not been replaced
        // by a successful call to setKeyframes(),
        // the computed keyframes for this keyframe effect.
        // Otherwise,
        // the result of applying the procedure compute missing keyframe offsets to the keyframes for this keyframe effect.
        // TODO: We don't compute missing keyframe offsets yet. But that will likely happen in stylo, not here.
        let keyframes = self.keyframes.borrow();
        let computed_offsets =
            compute_missing_keyframe_offsets(&keyframes.iter().map(|k| k.offset).collect::<Vec<_>>());

        // Step 3. For each keyframe in keyframes perform the following steps:
        for (keyframe, computed_offset) in keyframes.iter().zip(computed_offsets) {
            // Step 3.1 Initialize a dictionary object, output keyframe, using the following definition:
            // TODO Step 3.2 Set the offset, computedOffset, easing, and composite members of output keyframe
            // to the respective keyframe offset, computed keyframe offset, keyframe-specific easing function,
            // and keyframe-specific composite operation values of keyframe.
            let base_keyframe = BaseComputedKeyframe {
                composite: keyframe.composite,
                offset: keyframe.offset,
                computedOffset: Some(Finite::wrap(computed_offset)),
                easing: keyframe.easing_function.clone(),
            };
            rooted!(&in(cx) let mut output_keyframe = unsafe { JS_NewObject(cx, ptr::null()) });
            base_keyframe.to_jsobject(cx, output_keyframe.handle_mut());

            // Step 3.3 For each animation property-value pair declaration in keyframe, perform the following steps:
            for property_value_pair in &keyframe.declarations {
                debug_assert!(property_value_pair.property_id.is_animatable());

                // Step 3.3.1 Let property name be the result of applying the animation property name to IDL attribute
                // name algorithm to the property name of declaration.
                let mut property_name = String::new();
                let mut writer = CssWriter::new(&mut property_name);
                if property_value_pair.property_id.to_css(&mut writer).is_err() {
                    continue;
                }
                let property_name = animation_property_name_to_idl_attribute_name(&property_name);

                // Step 3.3.2 Let IDL value be the result of serializing the property value of declaration
                // by passing declaration to the algorithm to serialize a CSS value [CSSOM].
                let mut value_string = String::new();
                if property_value_pair
                    .block
                    .single_value_to_css(
                        &property_value_pair.property_id,
                        &mut value_string,
                        None,
                        stylist,
                    )
                    .is_err()
                {
                    continue;
                }

                // Step 3.3.3 Let value be the result of converting IDL value to an ECMAScript String value.
                rooted!(&in(cx) let mut value = UndefinedValue());
                value_string.to_jsval(cx, value.handle_mut());

                // Step 3.3.4 Call the [[DefineOwnProperty]] internal method on output keyframe with property
                // name property name, Property Descriptor { [[Writable]]: true, [[Enumerable]]: true, [[Configurable]]:
                // true, [[Value]]: value } and Boolean flag false.
                let Ok(property_name) = CString::new(property_name) else {
                    continue;
                };

                let success = unsafe {
                    JS_DefineProperty(
                        cx,
                        output_keyframe.handle(),
                        property_name.as_ptr(),
                        value.handle(),
                        JSPROP_ENUMERATE as u32,
                    )
                };
                if !success {
                    if cfg!(debug_assertions) {
                        unreachable!("Setting a property on output_keyframe should never fail");
                    }
                    return Err(Error::Operation(None));
                }
            }

            // Step 3.4 Append output keyframe to result.
            result.push(Heap::boxed(output_keyframe.get()))
        }

        // Step 4. Return result.
        Ok(())
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-keyframeeffect-setkeyframes>
    fn SetKeyframes(&self, cx: &mut JSContext, keyframes: *mut JSObject) -> Fallible<()> {
        // > This effect’s set of keyframes is replaced with the result of performing the procedure to
        // > process a keyframes argument. If that procedure throws an exception, this effect’s
        // > keyframes are not modified.
        let document = self.upcast::<AnimationEffect>().window().Document();
        let keyframes = process_a_keyframes_argument(cx, &document, keyframes)?;
        *self.keyframes.safe_borrow_mut(cx.no_gc()) = keyframes;
        self.changed(cx);
        Ok(())
    }
}

/// <https://drafts.csswg.org/web-animations-1/#process-a-keyframes-argument>
fn process_a_keyframes_argument(
    cx: &mut JSContext,
    document: &Document,
    keyframes: *mut JSObject,
) -> Fallible<Vec<Keyframe>> {
    // Step 1. If object is null, return an empty sequence of keyframes.
    if keyframes.is_null() {
        return Ok(Vec::new());
    }

    // Step 2. Let processed keyframes be an empty sequence of keyframes.

    // Step 3. Let method be the result of GetMethod(object, @@iterator).
    // Step 4. Check the completion record of method.
    // Step 5. Perform the steps corresponding to the first matching condition below:
    rooted!(&in(cx) let iterable = ObjectValue(keyframes));
    let mut keyframes = Vec::new();
    let result = for_of(cx, iterable.handle(), |cx, iterator_element| {
        // Step 5.3.4 Let nextItem be IteratorValue(next).
        // Step 5.3.5 Check the completion record of nextItem.
        // Note: This happens inside the "for_of" call.

        // Step 5.3.6 If Type(nextItem) is not Undefined, Null or Object, then throw a TypeError
        // and abort these steps.
        if !iterator_element.is_null_or_undefined() && !iterator_element.is_object() {
            return Err(ForOfIterationFailure::Other(Error::Type(
                c"Keyframe must be an object, null or undefined".to_owned(),
            )));
        }

        // Step 5.3.7 Append to processed keyframes the result of running the procedure to process a
        // keyframe-like object passing nextItem as the keyframe input with the allow lists flag set to false.
        keyframes.push(process_a_keyframe_like_object(
            cx,
            document,
            iterator_element,
        )?);

        Ok(ControlFlow::Continue(()))
    });
    match result {
        Ok(()) => Ok(keyframes),
        Err(ForOfIterationFailure::ValueIsNotIterable) => {
            // TODO: Step 5, Otherwise:
            Err(Error::Operation(None))
        },
        Err(ForOfIterationFailure::JSFailed) => Err(Error::JSFailed),
        Err(ForOfIterationFailure::Other(error)) => Err(error),
    }
}

/// <https://drafts.csswg.org/web-animations-1/#keyframe>
#[derive(JSTraceable, MallocSizeOf)]
struct Keyframe {
    offset: Option<Finite<f64>>,
    easing_function: DOMString,
    composite: CompositeOperationOrAuto,
    declarations: Vec<KeyframePropertyDeclaration>,
}

#[derive(JSTraceable, MallocSizeOf)]
struct KeyframePropertyDeclaration {
    #[no_trace]
    property_id: PropertyId,
    /// The block is known to only contain declarations for `property_id`. There might
    /// be more than one value if `property_id` is a shorthand.
    #[no_trace]
    block: PropertyDeclarationBlock,
}

/// <https://drafts.csswg.org/web-animations-1/#process-a-keyframe-like-object>
fn process_a_keyframe_like_object(
    cx: &mut JSContext,
    document: &Document,
    keyframe_input: HandleValue,
) -> Fallible<Keyframe> {
    // Step 1. Run the procedure to convert an ECMAScript value to a dictionary type [WEBIDL] with keyframe input
    // as the ECMAScript value, and the dictionary type depending on the value of the allow lists flag as follows:
    // If allow lists is true,
    // Use the following dictionary type:
    //
    // dictionary BasePropertyIndexedKeyframe {
    //   (double? or sequence<double?>)                         offset = [];
    //   (DOMString or sequence<DOMString>)                     easing = [];
    //   (CompositeOperationOrAuto or sequence<CompositeOperationOrAuto>) composite = [];
    // };
    //
    // Otherwise,
    //     Use the following dictionary type:
    //
    //     dictionary BaseKeyframe {
    //       double?                  offset = null;
    //       DOMString                easing = "linear";
    //       CompositeOperationOrAuto composite = "auto";
    //     };
    //
    // Store the result of this procedure as keyframe output.
    //
    // Note: 'allow lists' is currently never true.
    // Use the following dictionary type:
    let Ok(keyframe_output) = BaseKeyframe::from_jsval(cx, keyframe_input, ()) else {
        return Err(Error::JSFailed);
    };
    let ConversionResult::Success(keyframe_output) = keyframe_output else {
        return Err(Error::Operation(None));
    };
    let mut keyframe_output = Keyframe {
        offset: keyframe_output.offset,
        easing_function: keyframe_output.easing,
        composite: keyframe_output.composite,
        declarations: Vec::new(),
    };

    // Step 2. If keyframe input is null or undefined, return keyframe output.
    if keyframe_input.is_null_or_undefined() {
        return Ok(keyframe_output);
    }

    // From Step 3 onwards our implementation diverges from the specification. The spec
    // wants us to build a list of animatable CSS properties and a list of properties on
    // the object, then compute the union between the two.
    //
    // Instead, we iterate over all properties on the object and then check if they correspond
    // to an animatable property.
    let urlextradata = document.url().into_url().into();
    let parser_context = parser_context_for_document(
        document,
        CssRuleType::Style,
        ParsingMode::DEFAULT,
        &urlextradata,
    );
    rooted!(&in(cx) let object = keyframe_input.to_object());

    // Steps 2 - 6 are in get_property_declarations
    keyframe_output.declarations = get_property_declarations(cx, object.handle(), &parser_context)?;

    // Step 7. Return keyframe output.
    Ok(keyframe_output)
}

/// Implements Step 2-6 of  <https://drafts.csswg.org/web-animations-1/#process-a-keyframe-like-object>.
#[expect(unsafe_code)]
fn get_property_declarations(
    cx: &mut JSContext,
    object: HandleObject,
    parser_context: &ParserContext<'_>,
) -> Fallible<Vec<KeyframePropertyDeclaration>> {
    // The spec tells us to iterate over all animatable properties and see if they're defined
    // on the object. Instead we can iterate over the own properties of the object and see
    // if they're animated properties, that's easier.
    let mut ids = IdVector::new(cx);
    if !unsafe { GetPropertyKeys(cx, object, JSITER_OWNONLY, ids.handle_mut()) } {
        return Ok(Vec::new());
    }

    let mut declarations = Vec::with_capacity(ids.len());
    for id in ids.iter() {
        rooted!(&in(cx) let id = *id);

        // See if the id for the current property on the object represents a animatable CSS property.
        if !id.is_string() {
            continue;
        }
        rooted!(&in(cx) let mut key_value = UndefinedValue());
        let raw_id: HandleId = id.handle().into();
        if !unsafe { JS_IdToValue(cx, *raw_id.ptr, key_value.handle_mut()) } {
            continue;
        }
        rooted!(&in(cx) let js_string = key_value.to_string());
        let Some(js_string) = NonNull::new(js_string.get()) else {
            continue;
        };
        let property_name = unsafe { jsstr_to_string(cx, js_string) };

        let Some(property_id) = lookup_css_property_by_idl_attribute_name(&property_name) else {
            continue;
        };
        debug_assert!(property_id.is_animatable());

        // Step 6.1 Let raw value be the result of calling the [[Get]] internal method on keyframe input,
        // with property name as the property key and keyframe input as the receiver.
        // Step 6.2 Check the completion record of raw value.
        rooted!(&in(cx) let mut property_value = UndefinedValue());
        if !unsafe {
            JS_GetPropertyById(
                cx.raw_cx(),
                object.into_handle(),
                id.handle().into_handle(),
                property_value.handle_mut().into_handle_mut(),
            )
        } {
            continue;
        }

        // Step 6.3 Convert raw value to a DOMString or to a sequence of DOMStrings property values as follows:
        // If allow lists is true, [..] (Note: We don't implement "allow lists")
        // Otherwise,
        // Let property values be the result of converting raw value to a DOMString using the procedure
        // for converting an ECMAScript value to a DOMString [WEBIDL].
        let property_value = match DOMString::from_jsval(
            cx,
            property_value.handle(),
            StringificationBehavior::Default,
        ) {
            Ok(ConversionResult::Success(property_value)) => property_value,
            Ok(ConversionResult::Failure(error_message)) => {
                return Err(Error::Operation(
                    error_message
                        .to_str()
                        .ok()
                        .map(|message| message.to_owned()),
                ));
            },
            Err(_) => return Err(Error::JSFailed),
        };

        // Step 6.4 Calculate the normalized property name as the result of applying the IDL attribute name
        // to animation property name algorithm to property name.
        // Note: Due to the way our implementation differs from the spec (refer to the comment at
        // the top of this function), we already have the normalized property name.

        // Parse the property value as a value for the given animatable CSS property.
        // The specification continues to treat the value as a plain string, but there's not much point.
        let Some(declaration) =
            parse_single_property_declaration(property_id, &property_value.str(), parser_context)
        else {
            continue;
        };

        // Step 6.5 Add a property to keyframe output with normalized property name as the property name,
        // and property values as the property value.
        declarations.push(declaration);
    }

    Ok(declarations)
}

/// Parses `input` as a value for `property`, returning `None` on failure.
fn parse_single_property_declaration(
    property: NonCustomPropertyId,
    input: &str,
    parser_context: &ParserContext<'_>,
) -> Option<KeyframePropertyDeclaration> {
    let mut declaration = SourcePropertyDeclaration::default();
    let mut parser = Parser::new(input);

    // TODO: Consider reporting parse errors somewhere useful, like the devtools console.
    parser
        .parse_entirely(|parser| {
            PropertyDeclaration::parse_into(
                &mut declaration,
                PropertyId::NonCustom(property),
                parser_context,
                parser,
            )
        })
        .ok()?;

    let mut block = PropertyDeclarationBlock::new();
    block.extend(declaration.drain(), Importance::Normal);

    Some(KeyframePropertyDeclaration {
        property_id: PropertyId::NonCustom(property),
        block,
    })
}

/// This is the inverse of <https://drafts.csswg.org/web-animations-1/#animation-property-name-to-idl-attribute-name>.
fn lookup_css_property_by_idl_attribute_name(attribute_name: &str) -> Option<NonCustomPropertyId> {
    // TODO: Step 1. If property follows the <custom-property-name> production, return property.

    // Step 2. If property refers to the CSS float property, return the string "cssFloat".
    if attribute_name == "cssFloat" {
        return Some(LonghandId::Float.into());
    }

    // Step 3. If property refers to the CSS offset property, return the string "cssOffset".
    // NOTE: "offset" is not supported yet.

    // Step 4. Otherwise, return the result of applying the CSS property to IDL attribute algorithm
    // [CSSOM] to property.
    static IDL_ATTRIBUTE_TO_ANIMATED_PROPERTY_LOOKUP_TABLE: LazyLock<
        FxHashMap<String, NonCustomPropertyId>,
    > = LazyLock::new(|| {
        log::debug!("Initializing map from IDL attribute names to CSS properties");

        NonCustomPropertyId::iter()
            .filter(|non_custom_property| non_custom_property.is_animatable())
            .filter(|non_custom_property| {
                non_custom_property
                    .to_property_id()
                    .enabled_for_all_content()
            })
            .map(|non_custom_property| {
                let idl_attribute_name =
                    animation_property_name_to_idl_attribute_name(non_custom_property.name());
                (idl_attribute_name, non_custom_property)
            })
            .collect()
    });
    IDL_ATTRIBUTE_TO_ANIMATED_PROPERTY_LOOKUP_TABLE
        .get(attribute_name)
        .copied()
}

/// <https://drafts.csswg.org/web-animations-1/#animation-property-name-to-idl-attribute-name>
///
/// # Panics
/// Panics when the property name is empty or consists only of `-` characters.
/// In general it is assumed that `property_name` is a CSS property.
fn animation_property_name_to_idl_attribute_name(property_name: &str) -> String {
    let mut idl_attribute_name = String::with_capacity(property_name.len());

    let mut chunks = property_name.split('-');
    let Some(first_chunk) = chunks.next() else {
        unreachable!("CSS property name should not consist only of dashes");
    };
    idl_attribute_name.push_str(first_chunk);
    for chunk in chunks {
        let mut characters = chunk.chars();
        let Some(to_capitalize) = characters.next() else {
            continue;
        };
        idl_attribute_name.push(to_capitalize.to_ascii_uppercase());
        idl_attribute_name.push_str(characters.as_str());
    }

    idl_attribute_name
}

fn composite_for_style(composite: CompositeOperation) -> AnimationComposition {
    match composite {
        CompositeOperation::Replace => AnimationComposition::Replace,
        CompositeOperation::Add => AnimationComposition::Add,
        CompositeOperation::Accumulate => AnimationComposition::Accumulate,
    }
}

fn parse_easing(input: &str, parser_context: &ParserContext<'_>) -> Option<TimingFunction> {
    let mut parser = Parser::new(input);
    parser
        .parse_entirely(|parser| TimingFunction::parse(parser_context, parser))
        .ok()
}

/// <https://drafts.csswg.org/web-animations-1/#compute-missing-keyframe-offsets>
fn compute_missing_keyframe_offsets(offsets: &[Option<Finite<f64>>]) -> Vec<f64> {
    let mut computed: Vec<Option<f64>> = offsets.iter().map(|offset| offset.map(|o| *o)).collect();
    // Step 2. If keyframes contains more than one keyframe and the computed keyframe offset of the
    // first keyframe in keyframes is null, set the computed keyframe offset of the first keyframe to 0.
    if computed.len() > 1 && computed[0].is_none() {
        computed[0] = Some(0.);
    }
    // Step 3. If the computed keyframe offset of the last keyframe in keyframes is null, set its
    // computed keyframe offset to 1.
    if let Some(last) = computed.last_mut() {
        if last.is_none() {
            *last = Some(1.);
        }
    }
    // Step 4. Spread the keyframes with null offsets evenly between their neighbours.
    let mut index = 0;
    while index < computed.len() {
        if computed[index].is_some() {
            index += 1;
            continue;
        }
        let previous = index - 1;
        let next = (index..computed.len())
            .find(|i| computed[*i].is_some())
            .unwrap_or(computed.len() - 1);
        let start = computed[previous].unwrap_or(0.);
        let end = computed[next].unwrap_or(1.);
        let span = (next - previous) as f64;
        for i in index..next {
            computed[i] = Some(start + (end - start) * (i - previous) as f64 / span);
        }
        index = next;
    }
    computed.into_iter().map(|offset| offset.unwrap_or(0.)).collect()
}
