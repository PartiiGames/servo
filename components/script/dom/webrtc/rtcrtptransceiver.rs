/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use servo_media::webrtc::{RtpDirection, TransceiverId};

use crate::dom::bindings::codegen::Bindings::RTCRtpTransceiverBinding::{
    RTCRtpTransceiverDirection, RTCRtpTransceiverMethods,
};
use crate::dom::bindings::error::{Error, ErrorResult};
use crate::dom::bindings::root::{Dom, DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::globalscope::GlobalScope;
use crate::dom::rtcpeerconnection::RTCPeerConnection;
use crate::dom::rtcrtpreceiver::RTCRtpReceiver;
use crate::dom::rtcrtpsender::RTCRtpSender;

#[dom_struct]
pub(crate) struct RTCRtpTransceiver {
    reflector_: Reflector,
    #[ignore_malloc_size_of = "defined in servo-media"]
    #[no_trace]
    id: TransceiverId,
    sender: Dom<RTCRtpSender>,
    receiver: Dom<RTCRtpReceiver>,
    direction: Cell<RTCRtpTransceiverDirection>,
    current_direction: Cell<Option<RTCRtpTransceiverDirection>>,
    mid: DomRefCell<Option<DOMString>>,
    stopped: Cell<bool>,
    pc: MutNullableDom<RTCPeerConnection>,
}

impl RTCRtpTransceiver {
    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        pc: &RTCPeerConnection,
        id: TransceiverId,
        direction: RTCRtpTransceiverDirection,
        sender: &RTCRtpSender,
        receiver: &RTCRtpReceiver,
    ) -> DomRoot<Self> {
        sender.attach(pc, id);
        reflect_dom_object_with_cx(
            Box::new(Self {
                reflector_: Reflector::new(),
                id,
                sender: Dom::from_ref(sender),
                receiver: Dom::from_ref(receiver),
                direction: Cell::new(direction),
                current_direction: Cell::new(None),
                mid: DomRefCell::new(None),
                stopped: Cell::new(false),
                pc: MutNullableDom::new(Some(pc)),
            }),
            global,
            cx,
        )
    }

    pub(crate) fn id(&self) -> TransceiverId {
        self.id
    }

    pub(crate) fn receiver(&self) -> DomRoot<RTCRtpReceiver> {
        DomRoot::from_ref(&*self.receiver)
    }

    pub(crate) fn sender(&self) -> DomRoot<RTCRtpSender> {
        DomRoot::from_ref(&*self.sender)
    }

    /// The backend's view after a negotiation step.
    pub(crate) fn update(
        &self,
        mid: Option<String>,
        direction: RtpDirection,
        current: Option<RtpDirection>,
    ) {
        if mid.is_some() {
            *self.mid.borrow_mut() = mid.map(DOMString::from);
        }
        if direction == RtpDirection::Stopped {
            self.mark_stopped();
        }
        self.current_direction.set(current.map(to_dom));
    }

    pub(crate) fn mark_stopped(&self) {
        self.stopped.set(true);
        self.direction.set(RTCRtpTransceiverDirection::Stopped);
        self.current_direction
            .set(Some(RTCRtpTransceiverDirection::Stopped));
        self.sender.detach();
    }
}

pub(crate) fn to_dom(d: RtpDirection) -> RTCRtpTransceiverDirection {
    match d {
        RtpDirection::SendRecv => RTCRtpTransceiverDirection::Sendrecv,
        RtpDirection::SendOnly => RTCRtpTransceiverDirection::Sendonly,
        RtpDirection::RecvOnly => RTCRtpTransceiverDirection::Recvonly,
        RtpDirection::Inactive => RTCRtpTransceiverDirection::Inactive,
        RtpDirection::Stopped => RTCRtpTransceiverDirection::Stopped,
    }
}

pub(crate) fn from_dom(d: RTCRtpTransceiverDirection) -> RtpDirection {
    match d {
        RTCRtpTransceiverDirection::Sendrecv => RtpDirection::SendRecv,
        RTCRtpTransceiverDirection::Sendonly => RtpDirection::SendOnly,
        RTCRtpTransceiverDirection::Recvonly => RtpDirection::RecvOnly,
        RTCRtpTransceiverDirection::Inactive => RtpDirection::Inactive,
        RTCRtpTransceiverDirection::Stopped => RtpDirection::Stopped,
    }
}

impl RTCRtpTransceiverMethods<crate::DomTypeHolder> for RTCRtpTransceiver {
    /// <https://w3c.github.io/webrtc-pc/#dom-rtcrtptransceiver-mid>
    fn GetMid(&self) -> Option<DOMString> {
        self.mid.borrow().clone()
    }

    fn Direction(&self) -> RTCRtpTransceiverDirection {
        self.direction.get()
    }

    /// <https://w3c.github.io/webrtc-pc/#dom-rtcrtptransceiver-direction>
    fn SetDirection(&self, direction: RTCRtpTransceiverDirection) -> ErrorResult {
        if self.stopped.get() {
            return Err(Error::InvalidState(None));
        }
        if direction == RTCRtpTransceiverDirection::Stopped {
            return Err(Error::Type(c"use stop() to stop a transceiver".to_owned()));
        }
        if direction != self.direction.get() {
            self.direction.set(direction);
            if let Some(pc) = self.pc.get() &&
                let Some(c) = pc.controller()
            {
                c.set_direction(self.id, from_dom(direction));
            }
        }
        Ok(())
    }

    fn GetCurrentDirection(&self) -> Option<RTCRtpTransceiverDirection> {
        self.current_direction.get()
    }

    fn Sender(&self) -> DomRoot<RTCRtpSender> {
        DomRoot::from_ref(&*self.sender)
    }

    fn Receiver(&self) -> DomRoot<RTCRtpReceiver> {
        DomRoot::from_ref(&*self.receiver)
    }

    /// <https://w3c.github.io/webrtc-pc/#dom-rtcrtptransceiver-stop>: the next offer carries the
    /// m-line with port 0.
    fn Stop(&self) {
        if self.stopped.get() {
            return;
        }
        if let Some(pc) = self.pc.get() &&
            let Some(c) = pc.controller()
        {
            c.stop_transceiver(self.id);
        }
        self.stopped.set(true);
        self.direction.set(RTCRtpTransceiverDirection::Stopped);
        self.sender.detach();
    }
}
