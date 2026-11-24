/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::realm::CurrentRealm;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use servo_media::webrtc::TransceiverId;

use crate::dom::bindings::codegen::Bindings::RTCRtpSenderBinding::{
    RTCRtcpParameters, RTCRtpParameters, RTCRtpSendParameters, RTCRtpSenderMethods,
};
use crate::dom::bindings::error::Error;
use crate::dom::bindings::refcounted::{Trusted, TrustedPromise};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::globalscope::GlobalScope;
use crate::dom::mediastreamtrack::MediaStreamTrack;
use crate::dom::rtcpeerconnection::RTCPeerConnection;
use crate::dom::{Promise, RootedPromise};

#[dom_struct]
pub(crate) struct RTCRtpSender {
    reflector_: Reflector,
    track: MutNullableDom<MediaStreamTrack>,
    /// The transceiver it belongs to, and its peer connection (weakly: the pc owns transceivers).
    #[ignore_malloc_size_of = "defined in servo-media"]
    #[no_trace]
    id: Cell<Option<TransceiverId>>,
    pc: MutNullableDom<RTCPeerConnection>,
}

impl RTCRtpSender {
    fn new_inherited(track: Option<&MediaStreamTrack>) -> Self {
        Self {
            reflector_: Reflector::new(),
            track: MutNullableDom::new(track),
            id: Cell::new(None),
            pc: MutNullableDom::new(None),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        track: Option<&MediaStreamTrack>,
    ) -> DomRoot<Self> {
        reflect_dom_object_with_cx(Box::new(Self::new_inherited(track)), global, cx)
    }

    pub(crate) fn attach(&self, pc: &RTCPeerConnection, id: TransceiverId) {
        self.pc.set(Some(pc));
        self.id.set(Some(id));
    }

    pub(crate) fn detach(&self) {
        self.pc.set(None);
    }
}

impl RTCRtpSenderMethods<crate::DomTypeHolder> for RTCRtpSender {
    fn GetParameters(&self) -> RTCRtpSendParameters {
        RTCRtpSendParameters {
            parent: RTCRtpParameters {
                headerExtensions: vec![],
                rtcp: RTCRtcpParameters {
                    cname: None,
                    reducedSize: None,
                },
                codecs: vec![],
            },
            transactionId: DOMString::new(),
            encodings: vec![],
        }
    }

    fn SetParameters(
        &self,
        cx: &mut JSContext,
        _parameters: &RTCRtpSendParameters,
    ) -> RootedPromise {
        Promise::new_resolved(cx, &self.global(), ())
    }

    /// <https://w3c.github.io/webrtc-pc/#dom-rtcrtpsender-track>
    fn GetTrack(&self) -> Option<DomRoot<MediaStreamTrack>> {
        self.track.get()
    }

    /// <https://w3c.github.io/webrtc-pc/#dom-rtcrtpsender-replacetrack>: no renegotiation; the
    /// backend swaps the source under the same RTP stream.
    fn ReplaceTrack(
        &self,
        current_realm: &mut CurrentRealm,
        with_track: Option<&MediaStreamTrack>,
    ) -> RootedPromise {
        let p = Promise::new_in_realm(current_realm);
        let (Some(pc), Some(id)) = (self.pc.get(), self.id.get()) else {
            p.reject_error(current_realm, Error::InvalidState(None));
            return p;
        };
        if pc.is_closed() {
            p.reject_error(current_realm, Error::InvalidState(None));
            return p;
        }
        let task_source = self
            .global()
            .task_manager()
            .networking_task_source()
            .to_sendable();
        let this = Trusted::new(self);
        let track = with_track.map(Trusted::new);
        let promise = TrustedPromise::from(&p);
        let Some(controller) = pc.controller() else {
            p.reject_error(current_realm, Error::InvalidState(None));
            return p;
        };
        controller.replace_track(
            id,
            with_track.map(|t| t.id()),
            Box::new(move |r| {
                task_source.queue(task!(track_replaced: move |cx| {
                    let promise = promise.root(cx);
                    match r {
                        Ok(()) => {
                            this.root().track.set(track.map(|t| t.root()).as_deref());
                            promise.resolve_native(cx, &());
                        },
                        Err(e) => promise.reject_error(cx, Error::InvalidModification(Some(e))),
                    }
                }));
            }),
        );
        p
    }
}
