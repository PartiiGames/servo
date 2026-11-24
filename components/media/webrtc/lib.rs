/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

extern crate log;
extern crate servo_media_streams;
extern crate uuid;

use std::fmt::Display;
use std::str::FromStr;

use std::sync::atomic::{AtomicU64, Ordering};

use servo_media_streams::MediaStreamType;
use servo_media_streams::live::AudioSource;
use servo_media_streams::registry::MediaStreamId;

pub mod datachannel;
pub mod thread;

pub use datachannel::{
    DataChannelEvent, DataChannelId, DataChannelInit, DataChannelMessage, DataChannelState,
};
pub use thread::WebRtcController;

#[derive(Debug)]
pub enum WebRtcError {
    Backend(String),
}

pub type WebRtcResult = Result<(), WebRtcError>;
pub type WebRtcDataChannelResult = Result<DataChannelId, WebRtcError>;

impl<T: Display> From<T> for WebRtcError {
    fn from(x: T) -> Self {
        WebRtcError::Backend(x.to_string())
    }
}

/// This trait is implemented by backends and should never be used directly by
/// the client. Use WebRtcController instead
pub trait WebRtcControllerBackend: Send {
    fn configure(&mut self, stun_server: &str, policy: BundlePolicy) -> WebRtcResult;
    fn set_remote_description(
        &mut self,
        _: SessionDescription,
        cb: Box<dyn FnOnce() + Send + 'static>,
    ) -> WebRtcResult;
    fn set_local_description(
        &mut self,
        _: SessionDescription,
        cb: Box<dyn FnOnce() + Send + 'static>,
    ) -> WebRtcResult;
    fn add_ice_candidate(&mut self, candidate: IceCandidate) -> WebRtcResult;
    fn create_offer(
        &mut self,
        cb: Box<dyn FnOnce(SessionDescription) + Send + 'static>,
    ) -> WebRtcResult;
    fn create_answer(
        &mut self,
        cb: Box<dyn FnOnce(SessionDescription) + Send + 'static>,
    ) -> WebRtcResult;
    fn add_stream(&mut self, stream: &MediaStreamId) -> WebRtcResult;

    fn create_data_channel(&mut self, init: &DataChannelInit) -> WebRtcDataChannelResult;
    fn close_data_channel(&mut self, channel: &DataChannelId) -> WebRtcResult;
    fn send_data_channel_message(
        &mut self,
        channel: &DataChannelId,
        message: &DataChannelMessage,
    ) -> WebRtcResult;

    fn internal_event(&mut self, event: thread::InternalEvent) -> WebRtcResult;
    fn quit(&mut self);

    // Transceivers (Partii, stream voice): the unified-plan API. Backends without it keep the
    // defaults: errors, and descriptions through the callbacks above.

    /// `addTransceiver(trackOrKind, {direction})`. `receiver` is the source of the transceiver's
    /// `receiver.track` (`receiver_track`, made by the DOM, so the track exists at once): the
    /// backend pushes the decoded remote media into it.
    fn add_transceiver(
        &mut self,
        _id: TransceiverId,
        _kind: MediaStreamType,
        _direction: RtpDirection,
        _send: Option<MediaStreamId>,
        _receiver: AudioSource,
        _receiver_track: MediaStreamId,
    ) -> WebRtcResult {
        Err(WebRtcError::Backend("transceivers unsupported".into()))
    }
    fn set_direction(&mut self, _id: TransceiverId, _direction: RtpDirection) -> WebRtcResult {
        Err(WebRtcError::Backend("transceivers unsupported".into()))
    }
    /// `transceiver.stop()`: the next offer carries the m-line with port 0.
    fn stop_transceiver(&mut self, _id: TransceiverId) -> WebRtcResult {
        Err(WebRtcError::Backend("transceivers unsupported".into()))
    }
    /// `sender.replaceTrack(track)`: no renegotiation.
    fn replace_track(
        &mut self,
        _id: TransceiverId,
        _track: Option<MediaStreamId>,
        cb: ResultCallback<()>,
    ) -> WebRtcResult {
        cb(Err("replaceTrack unsupported".into()));
        Ok(())
    }
    fn get_stats(&mut self, cb: Box<dyn FnOnce(Vec<RtpStats>) + Send + 'static>) -> WebRtcResult {
        cb(vec![]);
        Ok(())
    }
    /// setLocalDescription / setRemoteDescription with an error path.
    fn set_description(
        &mut self,
        which: DescriptionType,
        desc: SessionDescription,
        cb: ResultCallback<()>,
    ) -> WebRtcResult {
        match which {
            DescriptionType::Local => self.set_local_description(desc, Box::new(move || cb(Ok(())))),
            DescriptionType::Remote => {
                self.set_remote_description(desc, Box::new(move || cb(Ok(()))))
            },
        }
    }
    /// createOffer / createAnswer with an error path.
    fn create_description(&mut self, ty: SdpType, cb: ResultCallback<SessionDescription>) -> WebRtcResult {
        if ty == SdpType::Offer {
            self.create_offer(Box::new(move |d| cb(Ok(d))))
        } else {
            self.create_answer(Box::new(move |d| cb(Ok(d))))
        }
    }
}

pub type ResultCallback<T> = Box<dyn FnOnce(Result<T, String>) + Send + 'static>;

/// A transceiver, the same id on both sides of the controller. Ids made by the DOM
/// ([`TransceiverId::new`]) and by the backend (for m-lines a remote offer adds) never collide.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct TransceiverId(pub u64);

impl TransceiverId {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        TransceiverId(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

impl Default for TransceiverId {
    fn default() -> Self {
        Self::new()
    }
}

/// <https://w3c.github.io/webrtc-pc/#dom-rtcrtptransceiverdirection>
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum RtpDirection {
    SendRecv,
    SendOnly,
    RecvOnly,
    Inactive,
    Stopped,
}

/// What the DOM shows of a transceiver after a negotiation step.
#[derive(Clone, Debug)]
pub struct TransceiverState {
    pub id: TransceiverId,
    pub mid: Option<String>,
    pub kind: MediaStreamType,
    pub direction: RtpDirection,
    pub current_direction: Option<RtpDirection>,
    /// The `receiver.track` (a live track; for transceivers the remote side added, the backend
    /// made its source).
    pub receiver_track: MediaStreamId,
    /// Made by a remote offer: the DOM makes its RTCRtpTransceiver.
    pub added_by_remote: bool,
    /// Fire `track` for it (it started receiving in this negotiation).
    pub fire_track: bool,
}

/// <https://w3c.github.io/webrtc-pc/#dom-rtcpeerconnectionstate>
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum PeerConnectionState {
    New,
    Connecting,
    Connected,
    Disconnected,
    Failed,
    Closed,
}

/// One RTP stream's counters, for getStats() (`inbound-rtp` / `outbound-rtp`).
#[derive(Clone, Debug)]
pub struct RtpStats {
    pub id: String,
    /// "inbound-rtp" or "outbound-rtp".
    pub type_: &'static str,
    /// "audio" or "video".
    pub kind: &'static str,
    pub ssrc: u32,
    pub mid: Option<String>,
    /// Payload bytes (bytesReceived / bytesSent).
    pub bytes: u64,
    pub packets: u64,
    /// Inbound: packets lost (from sequence gaps).
    pub packets_lost: i64,
    /// Inbound: jitter in seconds.
    pub jitter: f64,
    /// Inbound: audio level 0-1 of the last packets (`audioLevel`).
    pub audio_level: f64,
    /// Outbound: round trip time in seconds from RTCP, if known.
    pub round_trip_time: Option<f64>,
    /// Milliseconds since the Unix epoch.
    pub timestamp: f64,
}

pub trait WebRtcSignaller: Send {
    fn on_ice_candidate(&self, controller: &WebRtcController, candidate: IceCandidate);
    fn on_negotiation_needed(&self, controller: &WebRtcController);
    fn close(&self);
    fn on_add_stream(&self, stream: &MediaStreamId, ty: MediaStreamType);

    fn update_signaling_state(&self, _: SignalingState) {}
    fn update_gathering_state(&self, _: GatheringState) {}
    fn update_ice_connection_state(&self, _: IceConnectionState) {}

    fn on_data_channel_event(&self, _: DataChannelId, _: DataChannelEvent, _: &WebRtcController) {}

    /// Transceivers after a description was applied (before its callback runs): mids, current
    /// directions, transceivers a remote offer added, and which ones fire `track`.
    fn on_transceivers(&self, _: Vec<TransceiverState>) {}
    fn update_connection_state(&self, _: PeerConnectionState) {}
}

pub trait WebRtcBackend {
    type Controller: WebRtcControllerBackend + 'static;

    fn construct_webrtc_controller(
        signaller: Box<dyn WebRtcSignaller>,
        thread: WebRtcController,
    ) -> Self::Controller;
}

/// <https://www.w3.org/TR/webrtc/#rtcsdptype>
#[derive(Copy, Clone, Hash, Debug, PartialEq, Eq)]
pub enum SdpType {
    Answer,
    Offer,
    Pranswer,
    Rollback,
}

#[derive(Copy, Clone, Hash, Debug, PartialEq, Eq)]
pub enum DescriptionType {
    Local,
    Remote,
}

impl SdpType {
    pub fn as_str(self) -> &'static str {
        match self {
            SdpType::Answer => "answer",
            SdpType::Offer => "offer",
            SdpType::Pranswer => "pranswer",
            SdpType::Rollback => "rollback",
        }
    }
}

impl FromStr for SdpType {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        Ok(match s {
            "answer" => SdpType::Answer,
            "offer" => SdpType::Offer,
            "pranswer" => SdpType::Pranswer,
            "rollback" => SdpType::Rollback,
            _ => return Err(()),
        })
    }
}

/// <https://www.w3.org/TR/webrtc/#rtcsessiondescription-class>
///
/// <https://developer.mozilla.org/en-US/docs/Web/API/RTCSessionDescription>
#[derive(Clone, Hash, Debug, PartialEq, Eq)]
pub struct SessionDescription {
    pub type_: SdpType,
    pub sdp: String,
}

/// <https://www.w3.org/TR/webrtc/#rtcicecandidate-interface>
///
/// <https://developer.mozilla.org/en-US/docs/Web/API/RTCIceCandidate>
#[derive(Clone, Hash, Debug, PartialEq, Eq)]
pub struct IceCandidate {
    pub sdp_mline_index: u32,
    pub candidate: String,
    // XXXManishearth this is missing a bunch
}

/// <https://developer.mozilla.org/en-US/docs/Web/API/RTCPeerConnection#RTCBundlePolicy_enum>
#[derive(Clone, Copy, Hash, Debug, PartialEq, Eq)]
pub enum BundlePolicy {
    Balanced,
    MaxCompat,
    MaxBundle,
}

impl BundlePolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            BundlePolicy::Balanced => "balanced",
            BundlePolicy::MaxCompat => "max-compat",
            BundlePolicy::MaxBundle => "max-bundle",
        }
    }
}

/// <https://www.w3.org/TR/webrtc/#rtcsignalingstate-enum>
#[derive(Clone, Copy, Hash, Debug, PartialEq, Eq)]
pub enum SignalingState {
    Stable,
    HaveLocalOffer,
    HaveRemoteOffer,
    HaveLocalPranswer,
    HaveRemotePranswer,
    Closed,
}

/// <https://www.w3.org/TR/webrtc/#rtcicegatheringstate-enum>
#[derive(Clone, Copy, Hash, Debug, PartialEq, Eq)]
pub enum GatheringState {
    New,
    Gathering,
    Complete,
}

/// <https://www.w3.org/TR/webrtc/#rtciceconnectionstate-enum>
#[derive(Clone, Copy, Hash, Debug, PartialEq, Eq)]
pub enum IceConnectionState {
    New,
    Checking,
    Connected,
    Completed,
    Disconnected,
    Failed,
    Closed,
}
