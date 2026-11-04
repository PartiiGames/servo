/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! <https://webaudio.github.io/web-audio-api/#AudioParamMap>

use dom_struct::dom_struct;
use indexmap::IndexMap;
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::like::Maplike;
use script_bindings::reflector::{Reflector, reflect_dom_object};

use crate::dom::audio::audioparam::AudioParam;
use crate::dom::bindings::codegen::Bindings::AudioParamMapBinding::AudioParamMapMethods;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::window::Window;
use crate::maplike;

#[dom_struct]
pub(crate) struct AudioParamMap {
    reflector: Reflector,
    #[custom_trace]
    internal: DomRefCell<IndexMap<DOMString, DomRoot<AudioParam>>>,
}

impl AudioParamMap {
    pub(crate) fn new(cx: &mut JSContext, window: &Window) -> DomRoot<AudioParamMap> {
        reflect_dom_object(
            cx,
            Box::new(AudioParamMap {
                reflector: Reflector::new(),
                internal: DomRefCell::new(IndexMap::new()),
            }),
            window,
        )
    }

    pub(crate) fn insert(&self, cx: &mut JSContext, name: DOMString, param: &AudioParam) {
        self.internal.set(cx, name, DomRoot::from_ref(param));
    }
}

impl AudioParamMapMethods<crate::DomTypeHolder> for AudioParamMap {
    fn Size(&self) -> u32 {
        self.internal.borrow().len() as u32
    }
}

impl Maplike for AudioParamMap {
    type Key = DOMString;
    type Value = DomRoot<AudioParam>;

    maplike!(self, internal);
}
