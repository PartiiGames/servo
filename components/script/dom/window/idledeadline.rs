/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::time::Instant;

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};

use crate::dom::bindings::codegen::Bindings::IdleDeadlineBinding::IdleDeadlineMethods;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::root::DomRoot;
use crate::dom::window::Window;
use crate::event_loop::script_thread::ScriptThread;

/// <https://w3c.github.io/requestidlecallback/#the-idledeadline-interface>
#[dom_struct]
pub(crate) struct IdleDeadline {
    reflector_: Reflector,
    /// The end of the idle period this callback runs in (now, for a timed-out callback).
    #[ignore_malloc_size_of = "Instant"]
    deadline: Instant,
    did_timeout: bool,
}

impl IdleDeadline {
    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        deadline: Instant,
        did_timeout: bool,
    ) -> DomRoot<IdleDeadline> {
        reflect_dom_object_with_cx(
            Box::new(IdleDeadline {
                reflector_: Reflector::new(),
                deadline,
                did_timeout,
            }),
            window,
            cx,
        )
    }
}

impl IdleDeadlineMethods<crate::DomTypeHolder> for IdleDeadline {
    /// <https://w3c.github.io/requestidlecallback/#dom-idledeadline-timeremaining>
    fn TimeRemaining(&self) -> Finite<f64> {
        // > Let deadline be the result of getting the deadline (timers and frames scheduled
        // > since the idle period began can only bring it closer).
        // > Let timeRemaining be deadline minus now; if negative, 0.
        let mut deadline = self.deadline;
        if !self.did_timeout &&
            let Some(limit) = ScriptThread::idle_deadline_limit()
        {
            deadline = deadline.min(limit);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        Finite::wrap(remaining.as_secs_f64() * 1000.0)
    }

    /// <https://w3c.github.io/requestidlecallback/#dom-idledeadline-didtimeout>
    fn DidTimeout(&self) -> bool {
        self.did_timeout
    }
}
