/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii: <https://w3c.github.io/battery/>. The embedder reports the battery
//! (`WebViewDelegate::battery_status`); `navigator.getBattery()` asks it each time and the
//! manager fires change events for what changed since the last report.

use std::cell::Cell;

use dom_struct::dom_struct;
use embedder_traits::{BatteryStatus, EmbedderMsg};
use js::context::JSContext;
use js::realm::CurrentRealm;
use script_bindings::reflector::reflect_dom_object_with_cx;
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::BatteryManagerBinding::BatteryManagerMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::promise::RootedPromise;
use crate::routed_promise::{RoutedPromiseListener, callback_promise};

/// <https://w3c.github.io/battery/#the-batterymanager-interface>
#[dom_struct]
pub(crate) struct BatteryManager {
    eventtarget: EventTarget,
    charging: Cell<bool>,
    charging_time: Cell<f64>,
    discharging_time: Cell<f64>,
    level: Cell<f64>,
}

impl BatteryManager {
    fn new_inherited() -> Self {
        // <https://w3c.github.io/battery/#internal-slots>: until the first report, as if fully
        // charged on mains.
        Self {
            eventtarget: EventTarget::new_inherited(),
            charging: Cell::new(true),
            charging_time: Cell::new(0.0),
            discharging_time: Cell::new(f64::INFINITY),
            level: Cell::new(1.0),
        }
    }

    pub(crate) fn new(cx: &mut JSContext, global: &GlobalScope) -> DomRoot<Self> {
        reflect_dom_object_with_cx(Box::new(Self::new_inherited()), global, cx)
    }

    /// Ask the embedder for the battery state; `promise` resolves with this manager once it
    /// answers (<https://w3c.github.io/battery/#the-getbattery-method>).
    pub(crate) fn update(&self, cx: &mut CurrentRealm, promise: &RootedPromise) {
        let global = self.global();
        let Some(webview_id) = global.webview_id() else {
            promise.resolve_native(cx, self);
            return;
        };
        let task_manager = global.task_manager();
        let task_source = task_manager.dom_manipulation_task_source();
        let callback = callback_promise(promise, self, task_source);
        global.send_to_embedder(EmbedderMsg::GetBatteryStatus(webview_id, callback));
    }

    fn set_status(&self, cx: &mut JSContext, status: BatteryStatus) {
        let level = status.level.clamp(0.0, 1.0);
        let changes = [
            (self.charging.replace(status.charging) != status.charging, "chargingchange"),
            (
                self.charging_time.replace(status.charging_time) != status.charging_time,
                "chargingtimechange",
            ),
            (
                self.discharging_time.replace(status.discharging_time) !=
                    status.discharging_time,
                "dischargingtimechange",
            ),
            (self.level.replace(level) != level, "levelchange"),
        ];
        for (changed, event) in changes {
            if changed {
                self.upcast::<EventTarget>()
                    .fire_event(cx, Atom::from(event));
            }
        }
    }
}

impl RoutedPromiseListener<crate::DomTypeHolder, BatteryStatus> for BatteryManager {
    fn handle_response(&self, cx: &mut JSContext, status: BatteryStatus, promise: &RootedPromise) {
        self.set_status(cx, status);
        promise.resolve_native(cx, self);
    }
}

impl BatteryManagerMethods<crate::DomTypeHolder> for BatteryManager {
    /// <https://w3c.github.io/battery/#dom-batterymanager-charging>
    fn Charging(&self) -> bool {
        self.charging.get()
    }
    /// <https://w3c.github.io/battery/#dom-batterymanager-chargingtime>
    fn ChargingTime(&self) -> f64 {
        self.charging_time.get()
    }
    /// <https://w3c.github.io/battery/#dom-batterymanager-dischargingtime>
    fn DischargingTime(&self) -> f64 {
        self.discharging_time.get()
    }
    /// <https://w3c.github.io/battery/#dom-batterymanager-level>
    fn Level(&self) -> Finite<f64> {
        Finite::wrap(self.level.get())
    }

    event_handler!(chargingchange, GetOnchargingchange, SetOnchargingchange);
    event_handler!(chargingtimechange, GetOnchargingtimechange, SetOnchargingtimechange);
    event_handler!(
        dischargingtimechange,
        GetOndischargingtimechange,
        SetOndischargingtimechange
    );
    event_handler!(levelchange, GetOnlevelchange, SetOnlevelchange);
}
