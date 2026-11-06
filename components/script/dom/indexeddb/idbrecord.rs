/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii: <https://w3c.github.io/IndexedDB/#idbrecord>, the result items of getAllRecords().

use dom_struct::dom_struct;
use js::context::JSContext;
use js::jsapi::Heap;
use js::jsval::JSVal;
use js::rust::{HandleValue, MutableHandleValue};
use script_bindings::reflector::{Reflector, reflect_dom_object};

use crate::dom::bindings::codegen::Bindings::IDBRecordBinding::IDBRecordMethods;
use crate::dom::bindings::root::DomRoot;
use crate::dom::globalscope::GlobalScope;

#[dom_struct]
pub(crate) struct IDBRecord {
    reflector_: Reflector,
    /// <https://w3c.github.io/IndexedDB/#dom-idbrecord-key>
    #[ignore_malloc_size_of = "mozjs"]
    key: Heap<JSVal>,
    /// <https://w3c.github.io/IndexedDB/#dom-idbrecord-primarykey>
    #[ignore_malloc_size_of = "mozjs"]
    primary_key: Heap<JSVal>,
    /// <https://w3c.github.io/IndexedDB/#dom-idbrecord-value>
    #[ignore_malloc_size_of = "mozjs"]
    value: Heap<JSVal>,
}

impl IDBRecord {
    fn new_inherited() -> IDBRecord {
        IDBRecord {
            reflector_: Reflector::new(),
            key: Heap::default(),
            primary_key: Heap::default(),
            value: Heap::default(),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        key: HandleValue,
        primary_key: HandleValue,
        value: HandleValue,
    ) -> DomRoot<IDBRecord> {
        let record = reflect_dom_object(cx, Box::new(IDBRecord::new_inherited()), global);
        record.key.set(key.get());
        record.primary_key.set(primary_key.get());
        record.value.set(value.get());
        record
    }
}

impl IDBRecordMethods<crate::DomTypeHolder> for IDBRecord {
    /// <https://w3c.github.io/IndexedDB/#dom-idbrecord-key>
    fn Key(&self, _cx: &mut JSContext, mut retval: MutableHandleValue) {
        retval.set(self.key.get());
    }

    /// <https://w3c.github.io/IndexedDB/#dom-idbrecord-primarykey>
    fn PrimaryKey(&self, _cx: &mut JSContext, mut retval: MutableHandleValue) {
        retval.set(self.primary_key.get());
    }

    /// <https://w3c.github.io/IndexedDB/#dom-idbrecord-value>
    fn Value(&self, _cx: &mut JSContext, mut retval: MutableHandleValue) {
        retval.set(self.value.get());
    }
}
