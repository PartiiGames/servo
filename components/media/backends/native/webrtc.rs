/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! RTCPeerConnection on str0m (Partii, stream voice; runtime docs/handover/voice/STATUS.md).
//!
//! One thread per peer connection ("PartiiRtc") owns the str0m `Rtc`, its UDP sockets (one per
//! host candidate, each read by a small thread that forwards packets here) and the transceivers.
//! Everything else talks to it through one channel, so str0m's rule (mutate, then drain
//! `poll_output` to a timeout) holds by construction.
//!
//! Audio is Opus mono 48 kHz, 20 ms frames, through libopus: senders read their track from the
//! live registry (live.rs), receivers push decoded PCM into the transceiver's receiver source.
//! No trickle ICE: the local host candidates are in every description (jam voice sends no
//! candidates separately). Video transceivers are refused for now.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use log::{debug, warn};
use servo_media_streams::MediaStreamType;
use servo_media_streams::live::{self, AudioFormat, AudioReader, AudioSource};
use servo_media_streams::registry::MediaStreamId;
use servo_media_webrtc::thread::InternalEvent;
use servo_media_webrtc::{
    BundlePolicy, DataChannelId, DataChannelInit, DataChannelMessage, DescriptionType,
    GatheringState, IceCandidate, IceConnectionState, PeerConnectionState, ResultCallback,
    RtpDirection, RtpStats, SdpType, SessionDescription, SignalingState, TransceiverId,
    TransceiverState, WebRtcController, WebRtcControllerBackend, WebRtcDataChannelResult,
    WebRtcError, WebRtcResult, WebRtcSignaller,
};
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc, RtcConfig};

/// Opus at 48 kHz: 20 ms frames.
/// PARTII_WEBRTC_DEBUG=1: one stderr line per ICE/DTLS step, the candidates and the SDP's ICE lines,
/// and packet counters every 2 s (farm e2e diagnostics; nothing secret: no keys, no media).
fn rtc_debug() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PARTII_WEBRTC_DEBUG").is_ok_and(|v| v == "1"))
}

macro_rules! rtc_dbg {
    ($($t:tt)*) => {
        if rtc_debug() {
            eprintln!("partii-webrtc: {}", format_args!($($t)*));
        }
    };
}

/// The SDP lines that decide ICE and DTLS (candidates, ice-lite, setup, ufrag), for rtc_dbg.
fn sdp_ice_lines(sdp: &str) -> String {
    sdp.lines()
        .filter(|l| {
            l.starts_with("a=candidate") ||
                l.starts_with("a=ice-lite") ||
                l.starts_with("a=setup") ||
                l.starts_with("a=ice-ufrag") ||
                l.starts_with("m=")
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

const FRAME: usize = 960;
const RATE: u32 = 48_000;

// ---------- libopus ----------

struct OpusEncoder(*mut opusic_sys::OpusEncoder);
// SAFETY: the encoder state is only used from the thread that owns this value.
unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    fn new() -> Option<Self> {
        let mut err = 0;
        // SAFETY: plain libopus constructor; checked for null and the error code.
        let st = unsafe {
            opusic_sys::opus_encoder_create(
                RATE as i32,
                1,
                opusic_sys::OPUS_APPLICATION_VOIP,
                &mut err,
            )
        };
        if st.is_null() || err != opusic_sys::OPUS_OK as i32 {
            return None;
        }
        // SAFETY: valid encoder; the requests take one opus_int32 each.
        unsafe {
            opusic_sys::opus_encoder_ctl(st, opusic_sys::OPUS_SET_BITRATE_REQUEST, 32_000i32);
            opusic_sys::opus_encoder_ctl(st, opusic_sys::OPUS_SET_INBAND_FEC_REQUEST, 1i32);
            opusic_sys::opus_encoder_ctl(st, opusic_sys::OPUS_SET_PACKET_LOSS_PERC_REQUEST, 5i32);
        }
        Some(OpusEncoder(st))
    }

    fn encode(&mut self, pcm: &[f32; FRAME], out: &mut [u8]) -> Option<usize> {
        // SAFETY: pcm holds FRAME mono samples, out is writable for its length.
        let n = unsafe {
            opusic_sys::opus_encode_float(
                self.0,
                pcm.as_ptr(),
                FRAME as i32,
                out.as_mut_ptr(),
                out.len() as i32,
            )
        };
        (n > 0).then_some(n as usize)
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        // SAFETY: made by opus_encoder_create, destroyed once.
        unsafe { opusic_sys::opus_encoder_destroy(self.0) }
    }
}

struct OpusDecoder(*mut opusic_sys::OpusDecoder);
// SAFETY: as for OpusEncoder.
unsafe impl Send for OpusDecoder {}

impl OpusDecoder {
    fn new() -> Option<Self> {
        let mut err = 0;
        // SAFETY: plain libopus constructor; checked.
        let st = unsafe { opusic_sys::opus_decoder_create(RATE as i32, 1, &mut err) };
        if st.is_null() || err != opusic_sys::OPUS_OK as i32 {
            return None;
        }
        Some(OpusDecoder(st))
    }

    /// Decode one packet (or conceal a lost one with `None`) into `out`; returns samples.
    fn decode(&mut self, packet: Option<&[u8]>, out: &mut [f32]) -> usize {
        let (ptr, len) = match packet {
            Some(p) => (p.as_ptr(), p.len() as i32),
            None => (std::ptr::null(), 0),
        };
        let frames = if packet.is_some() {
            out.len()
        } else {
            FRAME.min(out.len())
        };
        // SAFETY: out is writable for `frames` mono samples; a null packet asks for concealment.
        let n = unsafe {
            opusic_sys::opus_decode_float(self.0, ptr, len, out.as_mut_ptr(), frames as i32, 0)
        };
        n.max(0) as usize
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        // SAFETY: made by opus_decoder_create, destroyed once.
        unsafe { opusic_sys::opus_decoder_destroy(self.0) }
    }
}

// ---------- the controller (on servo-media's WebRTC thread) ----------

enum Msg {
    Packet {
        data: Vec<u8>,
        source: SocketAddr,
        destination: SocketAddr,
        at: Instant,
    },
    SetDescription(DescriptionType, SessionDescription, ResultCallback<()>),
    Create(SdpType, ResultCallback<SessionDescription>),
    AddTransceiver(
        TransceiverId,
        MediaStreamType,
        RtpDirection,
        Option<MediaStreamId>,
        AudioSource,
        MediaStreamId,
    ),
    SetDirection(TransceiverId, RtpDirection),
    Stop(TransceiverId),
    ReplaceTrack(TransceiverId, Option<MediaStreamId>, ResultCallback<()>),
    Stats(Box<dyn FnOnce(Vec<RtpStats>) + Send>),
    AddStream(MediaStreamId),
    Quit,
}

/// servo-media's controller backend: forwards everything to the peer's own thread.
pub struct Str0mController {
    tx: Sender<Msg>,
    /// Ids for data channels, which never open here (see `create_data_channel`).
    next_dc: DataChannelId,
}

impl Str0mController {
    pub fn new(signaller: Box<dyn WebRtcSignaller>, controller: WebRtcController) -> Self {
        let (tx, rx) = mpsc::channel();
        let tx2 = tx.clone();
        thread::Builder::new()
            .name("PartiiRtc".to_owned())
            .spawn(move || Peer::new(signaller, controller, tx2).run(rx))
            .expect("spawn the WebRTC thread");
        Str0mController { tx, next_dc: 0 }
    }

    fn send(&self, m: Msg) -> WebRtcResult {
        self.tx
            .send(m)
            .map_err(|_| WebRtcError::Backend("peer connection closed".into()))
    }
}

impl WebRtcControllerBackend for Str0mController {
    fn configure(&mut self, stun_server: &str, _policy: BundlePolicy) -> WebRtcResult {
        // Host candidates only: jam voice talks to an ICE-lite SFU, which needs no reflexive
        // candidates from us. Every policy is max-bundle here.
        debug!("webrtc: STUN server {stun_server} noted (host candidates only)");
        Ok(())
    }
    fn set_remote_description(
        &mut self,
        desc: SessionDescription,
        cb: Box<dyn FnOnce() + Send + 'static>,
    ) -> WebRtcResult {
        self.set_description(DescriptionType::Remote, desc, Box::new(move |_| cb()))
    }
    fn set_local_description(
        &mut self,
        desc: SessionDescription,
        cb: Box<dyn FnOnce() + Send + 'static>,
    ) -> WebRtcResult {
        self.set_description(DescriptionType::Local, desc, Box::new(move |_| cb()))
    }
    fn add_ice_candidate(&mut self, candidate: IceCandidate) -> WebRtcResult {
        // Candidates in the remote description are used; trickled ones are not (yet).
        debug!(
            "webrtc: ignoring trickled candidate {}",
            candidate.candidate
        );
        Ok(())
    }
    fn create_offer(
        &mut self,
        cb: Box<dyn FnOnce(SessionDescription) + Send + 'static>,
    ) -> WebRtcResult {
        self.create_description(
            SdpType::Offer,
            Box::new(move |r| {
                if let Ok(d) = r {
                    cb(d)
                }
            }),
        )
    }
    fn create_answer(
        &mut self,
        cb: Box<dyn FnOnce(SessionDescription) + Send + 'static>,
    ) -> WebRtcResult {
        self.create_description(
            SdpType::Answer,
            Box::new(move |r| {
                if let Ok(d) = r {
                    cb(d)
                }
            }),
        )
    }
    fn add_stream(&mut self, stream: &MediaStreamId) -> WebRtcResult {
        self.send(Msg::AddStream(*stream))
    }
    // Data channels are not carried yet (str0m's SCTP is not wired in). A channel is created and simply never
    // opens: readyState stays "connecting" until the page or the peer connection closes it, sends are dropped,
    // and a remote m=application never fires ondatachannel. A page that needs one (partii-net's links) then
    // keeps to its other path (the ws relay) instead of the page dying on an error it never expects here.
    fn create_data_channel(&mut self, _: &DataChannelInit) -> WebRtcDataChannelResult {
        self.next_dc += 1;
        debug!("webrtc: data channel {} created (never opens: not supported yet)", self.next_dc);
        Ok(self.next_dc)
    }
    fn close_data_channel(&mut self, _: &DataChannelId) -> WebRtcResult {
        Ok(())
    }
    fn send_data_channel_message(
        &mut self,
        _: &DataChannelId,
        _: &DataChannelMessage,
    ) -> WebRtcResult {
        Ok(())
    }
    fn internal_event(&mut self, _: InternalEvent) -> WebRtcResult {
        Ok(())
    }
    fn quit(&mut self) {
        let _ = self.tx.send(Msg::Quit);
    }
    fn add_transceiver(
        &mut self,
        id: TransceiverId,
        kind: MediaStreamType,
        direction: RtpDirection,
        send: Option<MediaStreamId>,
        receiver: AudioSource,
        receiver_track: MediaStreamId,
    ) -> WebRtcResult {
        self.send(Msg::AddTransceiver(
            id,
            kind,
            direction,
            send,
            receiver,
            receiver_track,
        ))
    }
    fn set_direction(&mut self, id: TransceiverId, direction: RtpDirection) -> WebRtcResult {
        self.send(Msg::SetDirection(id, direction))
    }
    fn stop_transceiver(&mut self, id: TransceiverId) -> WebRtcResult {
        self.send(Msg::Stop(id))
    }
    fn replace_track(
        &mut self,
        id: TransceiverId,
        track: Option<MediaStreamId>,
        cb: ResultCallback<()>,
    ) -> WebRtcResult {
        self.send(Msg::ReplaceTrack(id, track, cb))
    }
    fn get_stats(&mut self, cb: Box<dyn FnOnce(Vec<RtpStats>) + Send + 'static>) -> WebRtcResult {
        self.send(Msg::Stats(cb))
    }
    fn set_description(
        &mut self,
        which: DescriptionType,
        desc: SessionDescription,
        cb: ResultCallback<()>,
    ) -> WebRtcResult {
        self.send(Msg::SetDescription(which, desc, cb))
    }
    fn create_description(
        &mut self,
        ty: SdpType,
        cb: ResultCallback<SessionDescription>,
    ) -> WebRtcResult {
        self.send(Msg::Create(ty, cb))
    }
}

// ---------- the peer (its own thread) ----------

/// Mono f32 at any rate in, 48 kHz out (linear; voice).
struct ToOpusRate {
    in_rate: u32,
    channels: usize,
    pos: f64,
    prev: f32,
    scratch: Vec<f32>,
}

impl ToOpusRate {
    fn new(format: AudioFormat) -> Self {
        ToOpusRate {
            in_rate: format.sample_rate.max(1),
            channels: format.channels.max(1) as usize,
            pos: 0.,
            prev: 0.,
            scratch: Vec::new(),
        }
    }

    /// Read what the reader has, append 48 kHz mono samples to `out`.
    fn pull(&mut self, reader: &mut AudioReader, out: &mut Vec<f32>) {
        let n = reader.buffered();
        if n == 0 {
            return;
        }
        self.scratch.resize(n, 0.);
        let got = reader.read(&mut self.scratch);
        let ch = self.channels;
        let mono = self.scratch[..got]
            .chunks_exact(ch)
            .map(|f| f.iter().sum::<f32>() / ch as f32);
        if self.in_rate == RATE {
            out.extend(mono);
            return;
        }
        let step = self.in_rate as f64 / RATE as f64;
        for s in mono {
            // Output samples between prev (at -1) and s (at 0).
            while self.pos < 1. {
                out.push(self.prev + (s - self.prev) * self.pos as f32);
                self.pos += step;
            }
            self.pos -= 1.;
            self.prev = s;
        }
    }
}

struct SendSide {
    track: MediaStreamId,
    reader: AudioReader,
    rate: ToOpusRate,
    pcm: Vec<f32>,
}

impl SendSide {
    fn new(track: MediaStreamId) -> Option<SendSide> {
        let reader = live::subscribe_audio(&track)?;
        let rate = ToOpusRate::new(reader.format());
        Some(SendSide {
            track,
            reader,
            rate,
            pcm: Vec::with_capacity(FRAME * 4),
        })
    }
}

struct Tr {
    id: TransceiverId,
    kind: MediaStreamType,
    direction: RtpDirection,
    /// The mid in the session (after the description that made it was applied), or the one in
    /// our pending offer.
    mid: Option<Mid>,
    negotiated: bool,
    current: Option<RtpDirection>,
    stop_requested: bool,
    stopped: bool,
    added_by_remote: bool,
    send: Option<SendSide>,
    encoder: Option<OpusEncoder>,
    samples_sent: u64,
    recv: AudioSource,
    recv_track: MediaStreamId,
    decoder: Option<OpusDecoder>,
    receiving: bool,
    last_packet: Option<Instant>,
    bytes_in: u64,
    packets_in: u64,
    lost_in: i64,
    level_in: f64,
    bytes_out: u64,
    packets_out: u64,
}

impl Tr {
    fn new(
        id: TransceiverId,
        kind: MediaStreamType,
        direction: RtpDirection,
        recv: AudioSource,
        recv_track: MediaStreamId,
    ) -> Tr {
        Tr {
            id,
            kind,
            direction,
            mid: None,
            negotiated: false,
            current: None,
            stop_requested: false,
            stopped: false,
            added_by_remote: false,
            send: None,
            encoder: None,
            samples_sent: 0,
            recv,
            recv_track,
            decoder: None,
            receiving: false,
            last_packet: None,
            bytes_in: 0,
            packets_in: 0,
            lost_in: 0,
            level_in: 0.,
            bytes_out: 0,
            packets_out: 0,
        }
    }

    fn state(&self, fire_track: bool) -> TransceiverState {
        TransceiverState {
            id: self.id,
            mid: self.mid.map(|m| m.to_string()),
            kind: self.kind,
            direction: if self.stopped {
                RtpDirection::Stopped
            } else {
                self.direction
            },
            current_direction: if self.stopped {
                Some(RtpDirection::Stopped)
            } else {
                self.current
            },
            receiver_track: self.recv_track,
            added_by_remote: self.added_by_remote,
            fire_track,
        }
    }
}

fn to_str0m(d: RtpDirection) -> Direction {
    match d {
        RtpDirection::SendRecv => Direction::SendRecv,
        RtpDirection::SendOnly => Direction::SendOnly,
        RtpDirection::RecvOnly => Direction::RecvOnly,
        RtpDirection::Inactive | RtpDirection::Stopped => Direction::Inactive,
    }
}

fn from_str0m(d: Direction) -> RtpDirection {
    match d {
        Direction::SendRecv => RtpDirection::SendRecv,
        Direction::SendOnly => RtpDirection::SendOnly,
        Direction::RecvOnly => RtpDirection::RecvOnly,
        Direction::Inactive => RtpDirection::Inactive,
    }
}

fn receives(d: Option<RtpDirection>) -> bool {
    matches!(d, Some(RtpDirection::SendRecv | RtpDirection::RecvOnly))
}

fn sends(d: Option<RtpDirection>) -> bool {
    matches!(d, Some(RtpDirection::SendRecv | RtpDirection::SendOnly))
}

/// A remote track's source: decoded Opus, 48 kHz mono.
fn remote_source() -> (MediaStreamId, AudioSource) {
    let (id, src) = live::create_audio_source(
        AudioFormat {
            sample_rate: RATE,
            channels: 1,
        },
        "remote audio",
    );
    // Remote tracks start muted until media arrives (webrtc-pc: receiver tracks).
    src.set_muted(true);
    (id, src)
}

/// The host addresses to gather: the default route's address (found without sending anything),
/// and loopback when there is none or PARTII_WEBRTC_LOOPBACK=1 (local tests).
/// PARTII_WEBRTC_BIND=<ip> gathers that address only (a farm test binds 127.0.0.1).
fn host_addresses() -> Vec<IpAddr> {
    if let Some(ip) = std::env::var("PARTII_WEBRTC_BIND")
        .ok()
        .and_then(|v| v.parse::<IpAddr>().ok())
    {
        return vec![ip];
    }
    let mut v = vec![];
    if let Ok(s) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) &&
        s.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).is_ok() &&
        let Ok(a) = s.local_addr() &&
        !a.ip().is_unspecified()
    {
        v.push(a.ip());
    }
    if v.is_empty() || std::env::var("PARTII_WEBRTC_LOOPBACK").is_ok_and(|x| x == "1") {
        v.push(IpAddr::V4(Ipv4Addr::LOCALHOST));
    }
    v
}

/// Bind a UDP socket on `ip`: any port, or the first free one of PARTII_WEBRTC_PORTS=<lo>-<hi>
/// (a farm job's port block).
fn bind_udp(ip: IpAddr) -> Option<UdpSocket> {
    let range = std::env::var("PARTII_WEBRTC_PORTS").ok().and_then(|v| {
        let (a, b) = v.split_once('-')?;
        Some((a.parse::<u16>().ok()?, b.parse::<u16>().ok()?))
    });
    match range {
        // Every pinned port taken (another job sharing the block): an ephemeral one rather than none.
        // Start one past the last port used, so a new connection (a rejoin) doesn't take the 5-tuple
        // its predecessor just had (an SFU may still route that to the old session), as browsers'
        // random ports don't either.
        Some((lo, hi)) if lo <= hi => {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = u32::from(hi - lo) + 1;
            let start = NEXT.fetch_add(1, Ordering::Relaxed) % n;
            (0..n)
                .map(|k| lo + ((start + k) % n) as u16)
                .find_map(|p| UdpSocket::bind((ip, p)).ok())
                .or_else(|| {
                    rtc_dbg!("PARTII_WEBRTC_PORTS {lo}-{hi} all taken: an ephemeral port");
                    UdpSocket::bind((ip, 0)).ok()
                })
        },
        Some(_) => UdpSocket::bind((ip, 0)).ok(),
        None => UdpSocket::bind((ip, 0)).ok(),
    }
}

struct Peer {
    rtc: Rtc,
    signaller: Box<dyn WebRtcSignaller>,
    controller: WebRtcController,
    sockets: Vec<(SocketAddr, UdpSocket)>,
    stop_sockets: Arc<AtomicBool>,
    trs: Vec<Tr>,
    pending: Option<(SdpPendingOffer, Vec<(TransceiverId, Mid)>)>,
    last_local: Option<SessionDescription>,
    answer: Option<String>,
    signaling: SignalingState,
    pc_state: PeerConnectionState,
    ice_connected: bool,
    dtls_connected: bool,
    disconnected_at: Option<Instant>,
    closed: bool,
    encoded: Vec<u8>,
    decoded: Vec<f32>,
    /// rtc_dbg counters: datagrams received, accepted by str0m, sent, send errors.
    n_in: u64,
    n_accepted: u64,
    n_out: u64,
    n_send_err: u64,
    last_dbg: Instant,
}

impl Peer {
    fn new(
        signaller: Box<dyn WebRtcSignaller>,
        controller: WebRtcController,
        tx: Sender<Msg>,
    ) -> Peer {
        let rtc = RtcConfig::new()
            .clear_codecs()
            .enable_opus(true, false)
            .build(Instant::now());
        let mut peer = Peer {
            rtc,
            signaller,
            controller,
            sockets: vec![],
            stop_sockets: Arc::new(AtomicBool::new(false)),
            trs: vec![],
            pending: None,
            last_local: None,
            answer: None,
            signaling: SignalingState::Stable,
            pc_state: PeerConnectionState::New,
            ice_connected: false,
            dtls_connected: false,
            disconnected_at: None,
            closed: false,
            encoded: vec![0; 1500],
            decoded: vec![0.; 5760],
            n_in: 0,
            n_accepted: 0,
            n_out: 0,
            n_send_err: 0,
            last_dbg: Instant::now(),
        };
        for ip in host_addresses() {
            let Some(sock) = bind_udp(ip) else {
                continue;
            };
            let Ok(addr) = sock.local_addr() else {
                continue;
            };
            let Ok(reader) = sock.try_clone() else {
                continue;
            };
            let Ok(c) = Candidate::host(addr, "udp") else {
                rtc_dbg!("local candidate {addr} refused by Candidate::host");
                continue;
            };
            let added = peer.rtc.add_local_candidate(c).is_some();
            rtc_dbg!("local candidate {addr} (added: {added})");
            peer.drain();
            let _ = reader.set_read_timeout(Some(Duration::from_millis(250)));
            let tx = tx.clone();
            let stop = peer.stop_sockets.clone();
            let _ = thread::Builder::new()
                .name("PartiiRtcUdp".to_owned())
                .spawn(move || {
                    let mut buf = vec![0u8; 2000];
                    while !stop.load(Ordering::Relaxed) {
                        match reader.recv_from(&mut buf) {
                            Ok((n, source)) => {
                                let m = Msg::Packet {
                                    data: buf[..n].to_vec(),
                                    source,
                                    destination: addr,
                                    at: Instant::now(),
                                };
                                if tx.send(m).is_err() {
                                    break;
                                }
                            },
                            Err(e)
                                if matches!(
                                    e.kind(),
                                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                                ) => {},
                            Err(_) => thread::sleep(Duration::from_millis(5)),
                        }
                    }
                });
            peer.sockets.push((addr, sock));
        }
        if peer.sockets.is_empty() {
            rtc_dbg!("no UDP socket could be bound (PARTII_WEBRTC_PORTS {:?})", std::env::var("PARTII_WEBRTC_PORTS").ok());
            warn!("webrtc: no UDP socket could be bound");
        }
        peer
    }

    fn run(mut self, rx: mpsc::Receiver<Msg>) {
        let mut next = Instant::now();
        loop {
            // Wake at str0m's deadline, or every 10 ms to move audio.
            let wait = next
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(10));
            match rx.recv_timeout(wait) {
                Ok(Msg::Quit) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(m) => self.handle(m),
                Err(RecvTimeoutError::Timeout) => {},
            }
            if self.closed {
                break;
            }
            let now = Instant::now();
            if now >= next {
                if let Err(e) = self.rtc.handle_input(Input::Timeout(now)) {
                    warn!("webrtc: timeout input: {e:?}");
                }
            }
            next = self.drain();
            self.send_audio();
            self.check_liveness();
            if rtc_debug() && now.duration_since(self.last_dbg) >= Duration::from_secs(2) {
                self.last_dbg = now;
                rtc_dbg!(
                    "packets in {} (accepted {}) out {} send errors {}; ice {} dtls {} pc {:?} alive {}",
                    self.n_in,
                    self.n_accepted,
                    self.n_out,
                    self.n_send_err,
                    self.ice_connected,
                    self.dtls_connected,
                    self.pc_state,
                    self.rtc.is_alive()
                );
            }
        }
        self.close();
    }

    /// Drain str0m's output to its next timeout: send packets, handle events.
    fn drain(&mut self) -> Instant {
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(t)) => return t,
                Ok(Output::Transmit(t)) => {
                    let sock = self
                        .sockets
                        .iter()
                        .find(|(a, _)| *a == t.source)
                        .or(self.sockets.first());
                    if let Some((_, s)) = sock {
                        match s.send_to(&t.contents, t.destination) {
                            Ok(_) => self.n_out += 1,
                            Err(e) => {
                                if self.n_send_err == 0 {
                                    rtc_dbg!("send {} -> {}: {e}", t.source, t.destination);
                                }
                                self.n_send_err += 1;
                            },
                        }
                    } else {
                        self.n_send_err += 1;
                    }
                },
                Ok(Output::Event(e)) => self.event(e),
                Err(e) => {
                    rtc_dbg!("poll_output error: {e:?} (alive {})", self.rtc.is_alive());
                    warn!("webrtc: {e:?}");
                    return Instant::now() + Duration::from_millis(50);
                },
            }
        }
    }

    fn event(&mut self, e: Event) {
        match e {
            Event::IceConnectionStateChange(s) => {
                use str0m::IceConnectionState as S;
                let ice = match s {
                    S::New => IceConnectionState::New,
                    S::Checking => IceConnectionState::Checking,
                    S::Connected => IceConnectionState::Connected,
                    S::Completed => IceConnectionState::Completed,
                    S::Disconnected => IceConnectionState::Disconnected,
                };
                rtc_dbg!("ice state {s:?}");
                self.ice_connected = matches!(s, S::Connected | S::Completed);
                self.disconnected_at = (s == S::Disconnected).then(Instant::now);
                self.signaller.update_ice_connection_state(ice);
                self.update_pc_state();
            },
            Event::Connected => {
                rtc_dbg!("dtls connected");
                self.dtls_connected = true;
                self.update_pc_state();
            },
            Event::MediaAdded(m) => {
                if self.trs.iter().any(|t| t.mid == Some(m.mid)) {
                    return;
                }
                if m.kind != MediaKind::Audio {
                    warn!("webrtc: remote {:?} m-line {} not supported", m.kind, m.mid);
                }
                let (track, src) = remote_source();
                let mut t = Tr::new(
                    TransceiverId::new(),
                    if m.kind == MediaKind::Audio {
                        MediaStreamType::Audio
                    } else {
                        MediaStreamType::Video
                    },
                    RtpDirection::RecvOnly,
                    src,
                    track,
                );
                t.mid = Some(m.mid);
                t.negotiated = true;
                t.added_by_remote = true;
                self.trs.push(t);
            },
            Event::MediaData(d) => {
                let decoded = &mut self.decoded;
                let Some(t) = self.trs.iter_mut().find(|t| t.mid == Some(d.mid)) else {
                    return;
                };
                if t.kind != MediaStreamType::Audio {
                    return;
                }
                if t.decoder.is_none() {
                    t.decoder = OpusDecoder::new();
                }
                let Some(dec) = t.decoder.as_mut() else {
                    return;
                };
                t.bytes_in += d.data.len() as u64;
                t.packets_in += 1;
                t.last_packet = Some(Instant::now());
                if !d.contiguous && t.packets_in > 1 {
                    t.lost_in += 1;
                    let n = dec.decode(None, decoded);
                    t.recv.push(&decoded[..n], Instant::now());
                }
                let n = dec.decode(Some(&d.data[..]), decoded);
                if n > 0 {
                    let pcm = &decoded[..n];
                    let rms = (pcm.iter().map(|v| v * v).sum::<f32>() / n as f32).sqrt();
                    t.level_in = rms as f64;
                    t.recv.push(pcm, d.network_time);
                }
            },
            Event::MediaChanged(c) => {
                if let Some(t) = self.trs.iter_mut().find(|t| t.mid == Some(c.mid)) {
                    t.current = Some(from_str0m(c.direction));
                }
            },
            _ => {},
        }
    }

    fn update_pc_state(&mut self) {
        let s = if self.closed {
            PeerConnectionState::Closed
        } else if self.ice_connected && self.dtls_connected {
            PeerConnectionState::Connected
        } else if self.disconnected_at.is_some() && self.dtls_connected {
            PeerConnectionState::Disconnected
        } else if self.pc_state == PeerConnectionState::New && !self.ice_connected {
            if self.trs.iter().any(|t| t.negotiated) {
                PeerConnectionState::Connecting
            } else {
                PeerConnectionState::New
            }
        } else if !self.ice_connected || !self.dtls_connected {
            PeerConnectionState::Connecting
        } else {
            self.pc_state
        };
        if s != self.pc_state {
            self.pc_state = s;
            self.signaller.update_connection_state(s);
        }
    }

    /// Disconnected for 15 s, or str0m gave up: failed. Receivers with no packets for 1 s mute.
    fn check_liveness(&mut self) {
        let now = Instant::now();
        if self.pc_state != PeerConnectionState::Failed
            && (self
                .disconnected_at
                .is_some_and(|t| now - t > Duration::from_secs(15))
                || !self.rtc.is_alive())
        {
            self.pc_state = PeerConnectionState::Failed;
            self.signaller
                .update_connection_state(PeerConnectionState::Failed);
            self.signaller
                .update_ice_connection_state(IceConnectionState::Failed);
        }
        for t in &mut self.trs {
            let live = t
                .last_packet
                .is_some_and(|p| now - p < Duration::from_secs(1));
            if live != t.receiving {
                t.receiving = live;
                t.recv.set_muted(!live);
            }
        }
    }

    /// Move audio: read every sender's track, then send whole 20 ms frames (one per sender per
    /// pass, each write drained before the next, as str0m requires).
    fn send_audio(&mut self) {
        let connected = self.pc_state == PeerConnectionState::Connected;
        for t in &mut self.trs {
            let Some(side) = t.send.as_mut() else {
                continue;
            };
            side.rate.pull(&mut side.reader, &mut side.pcm);
            if !(connected && t.negotiated && !t.stopped && sends(t.current)) {
                side.pcm.clear();
                continue;
            }
            // Keep at most 100 ms queued: a late thread drops the oldest, it doesn't drift.
            if side.pcm.len() > FRAME * 5 {
                let n = side.pcm.len() - FRAME * 2;
                side.pcm.drain(..n);
            }
            if t.encoder.is_none() {
                t.encoder = OpusEncoder::new();
            }
        }
        for _ in 0..5 {
            if !self
                .trs
                .iter()
                .any(|t| t.send.as_ref().is_some_and(|s| s.pcm.len() >= FRAME))
            {
                break;
            }
            self.send_audio_once();
        }
    }

    fn send_audio_once(&mut self) {
        let connected = self.pc_state == PeerConnectionState::Connected;
        let mut frame = [0f32; FRAME];
        for i in 0..self.trs.len() {
            let t = &mut self.trs[i];
            let Some(side) = t.send.as_mut() else {
                continue;
            };
            if !(connected && t.negotiated && !t.stopped && sends(t.current))
                || side.pcm.len() < FRAME
            {
                continue;
            }
            let Some(mid) = t.mid else { continue };
            frame.copy_from_slice(&side.pcm[..FRAME]);
            side.pcm.drain(..FRAME);
            let Some(enc) = t.encoder.as_mut() else {
                continue;
            };
            let Some(n) = enc.encode(&frame, &mut self.encoded) else {
                continue;
            };
            let time = MediaTime::new(t.samples_sent, Frequency::FORTY_EIGHT_KHZ);
            t.samples_sent += FRAME as u64;
            let Some(writer) = self.rtc.writer(mid) else {
                continue;
            };
            let Some(pt) = writer.payload_params().next().map(|p| p.pt()) else {
                continue;
            };
            if writer
                .write(pt, Instant::now(), time, self.encoded[..n].to_vec())
                .is_ok()
            {
                t.bytes_out += n as u64;
                t.packets_out += 1;
            }
            self.drain();
        }
    }

    fn report(&self, fire: &[TransceiverId]) {
        let v = self
            .trs
            .iter()
            .map(|t| t.state(fire.contains(&t.id)))
            .collect();
        self.signaller.on_transceivers(v);
    }

    fn set_signaling(&mut self, s: SignalingState) {
        if s != self.signaling {
            self.signaling = s;
            self.signaller.update_signaling_state(s);
        }
    }

    /// Current directions from the session, and the transceivers that start receiving now.
    fn refresh_directions(&mut self) -> Vec<TransceiverId> {
        let mut fire = vec![];
        for t in &mut self.trs {
            let Some(mid) = t.mid else { continue };
            if !t.negotiated {
                continue;
            }
            let Some(m) = self.rtc.media(mid) else {
                continue;
            };
            let was = t.current;
            if m.stopped() {
                t.stopped = true;
                t.current = Some(RtpDirection::Stopped);
                t.recv.end();
                continue;
            }
            t.current = Some(from_str0m(m.direction()));
            if receives(t.current) && !receives(was) {
                fire.push(t.id);
            }
        }
        fire
    }

    fn handle(&mut self, m: Msg) {
        match m {
            Msg::Packet {
                data,
                source,
                destination,
                at,
            } => {
                let Ok(contents) = data.as_slice().try_into() else {
                    return;
                };
                let input = Input::Receive(
                    at,
                    Receive {
                        proto: Protocol::Udp,
                        source,
                        destination,
                        contents,
                    },
                );
                self.n_in += 1;
                if self.rtc.accepts(&input) {
                    self.n_accepted += 1;
                    if let Err(e) = self.rtc.handle_input(input) {
                        rtc_dbg!("input from {source}: {e:?}");
                        debug!("webrtc: input: {e:?}");
                    }
                } else if self.n_in - self.n_accepted <= 3 {
                    rtc_dbg!("not accepted: {} bytes from {source} to {destination}", data.len());
                }
            },
            Msg::Create(ty, cb) => {
                let r = if ty == SdpType::Offer {
                    self.create_offer()
                } else {
                    self.answer
                        .clone()
                        .map(|sdp| SessionDescription {
                            type_: SdpType::Answer,
                            sdp,
                        })
                        .ok_or_else(|| "createAnswer needs a remote offer".to_owned())
                };
                cb(r);
            },
            Msg::SetDescription(which, desc, cb) => {
                let r = self.set_description(which, desc);
                self.update_pc_state();
                cb(r);
            },
            Msg::AddTransceiver(id, kind, direction, send, receiver, recv_track) => {
                receiver.set_muted(true);
                let mut t = Tr::new(id, kind, direction, receiver, recv_track);
                if kind == MediaStreamType::Audio {
                    t.send = send.and_then(SendSide::new);
                }
                self.trs.push(t);
            },
            Msg::SetDirection(id, d) => {
                if let Some(t) = self.trs.iter_mut().find(|t| t.id == id) {
                    t.direction = d;
                }
            },
            Msg::Stop(id) => {
                if let Some(t) = self.trs.iter_mut().find(|t| t.id == id) {
                    t.stop_requested = true;
                    t.send = None;
                    t.recv.end();
                    if !t.negotiated {
                        t.stopped = true;
                    }
                }
            },
            Msg::ReplaceTrack(id, track, cb) => {
                let Some(t) = self.trs.iter_mut().find(|t| t.id == id) else {
                    return cb(Err("unknown transceiver".into()));
                };
                if t.stopped {
                    return cb(Err("InvalidStateError: the transceiver is stopped".into()));
                }
                match track {
                    None => {
                        t.send = None;
                        cb(Ok(()))
                    },
                    Some(track) => match SendSide::new(track) {
                        Some(mut side) => {
                            // Keep the stream continuous: the timestamps go on from the old track.
                            if let Some(old) = t.send.take() {
                                side.pcm = old.pcm;
                            }
                            t.send = Some(side);
                            cb(Ok(()))
                        },
                        None => cb(Err("not a live audio track".into())),
                    },
                }
            },
            Msg::Stats(cb) => cb(self.stats()),
            Msg::AddStream(id) => {
                // Legacy addStream: a sendrecv transceiver for the track.
                let (track, src) = remote_source();
                let mut t = Tr::new(
                    TransceiverId::new(),
                    MediaStreamType::Audio,
                    RtpDirection::SendRecv,
                    src,
                    track,
                );
                t.send = SendSide::new(id);
                self.trs.push(t);
                self.signaller.on_negotiation_needed(&self.controller);
            },
            Msg::Quit => self.closed = true,
        }
        self.drain();
    }

    fn create_offer(&mut self) -> Result<SessionDescription, String> {
        // A new offer replaces an unanswered one: its new m-lines are made again.
        if let Some((_, added)) = self.pending.take() {
            for t in &mut self.trs {
                if !t.negotiated && added.iter().any(|(id, _)| *id == t.id) {
                    t.mid = None;
                }
            }
        }
        let mut api = self.rtc.sdp_api();
        let mut added = vec![];
        for t in &mut self.trs {
            if t.stopped || t.kind != MediaStreamType::Audio {
                continue;
            }
            if !t.negotiated {
                if t.stop_requested {
                    t.stopped = true;
                    continue;
                }
                let mid = api.add_media(
                    MediaKind::Audio,
                    to_str0m(t.direction),
                    None,
                    t.send.as_ref().map(|s| s.track.id().to_string()),
                    None,
                );
                t.mid = Some(mid);
                added.push((t.id, mid));
            } else if let Some(mid) = t.mid {
                if t.stop_requested {
                    api.stop_media(mid);
                } else {
                    api.set_direction(mid, to_str0m(t.direction));
                }
            }
        }
        let applied = api.apply();
        self.drain();
        match applied {
            Some((offer, pending)) => {
                self.pending = Some((pending, added));
                Ok(SessionDescription {
                    type_: SdpType::Offer,
                    sdp: offer.to_sdp_string(),
                })
            },
            None => self
                .last_local
                .clone()
                .map(|d| SessionDescription {
                    type_: SdpType::Offer,
                    sdp: d.sdp,
                })
                .ok_or_else(|| "nothing to offer: add a transceiver first".to_owned()),
        }
    }

    fn set_description(
        &mut self,
        which: DescriptionType,
        desc: SessionDescription,
    ) -> Result<(), String> {
        if self.closed {
            return Err("InvalidStateError: closed".into());
        }
        match (which, desc.type_) {
            (DescriptionType::Local, SdpType::Offer) => {
                rtc_dbg!("local offer: {}", sdp_ice_lines(&desc.sdp));
                // Our own offer (from createOffer): its mids are now the transceivers'.
                if self.pending.is_none() {
                    return Err("InvalidModificationError: no offer was created".into());
                }
                self.last_local = Some(desc);
                self.set_signaling(SignalingState::HaveLocalOffer);
                self.signaller
                    .update_gathering_state(GatheringState::Complete);
                self.report(&[]);
                Ok(())
            },
            (DescriptionType::Local, SdpType::Answer) => {
                if self.answer.is_none() {
                    return Err("InvalidStateError: no remote offer".into());
                }
                self.last_local = Some(desc);
                self.answer = None;
                let fire = self.refresh_directions();
                self.set_signaling(SignalingState::Stable);
                self.signaller
                    .update_gathering_state(GatheringState::Complete);
                self.report(&fire);
                Ok(())
            },
            (DescriptionType::Remote, SdpType::Answer) => {
                rtc_dbg!("remote answer: {}", sdp_ice_lines(&desc.sdp));
                let Some((pending, added)) = self.pending.take() else {
                    return Err("InvalidStateError: no local offer".into());
                };
                let answer = SdpAnswer::from_sdp_string(&desc.sdp)
                    .map_err(|e| format!("OperationError: {e:?}"))?;
                let r = self.rtc.sdp_api().accept_answer(pending, answer);
                if let Err(e) = r {
                    rtc_dbg!("accept_answer: {e:?}");
                    self.drain();
                    return Err(format!("OperationError: {e:?}"));
                }
                for t in &mut self.trs {
                    if added.iter().any(|(id, _)| *id == t.id) {
                        t.negotiated = true;
                    }
                    if t.stop_requested {
                        t.stopped = true;
                    }
                }
                self.drain();
                let fire = self.refresh_directions();
                self.set_signaling(SignalingState::Stable);
                self.report(&fire);
                Ok(())
            },
            (DescriptionType::Remote, SdpType::Offer) => {
                rtc_dbg!("remote offer: {}", sdp_ice_lines(&desc.sdp));
                let offer = SdpOffer::from_sdp_string(&desc.sdp)
                    .map_err(|e| format!("OperationError: {e:?}"))?;
                // A remote offer rolls back an unanswered local one.
                if let Some((_, added)) = self.pending.take() {
                    for t in &mut self.trs {
                        if !t.negotiated && added.iter().any(|(id, _)| *id == t.id) {
                            t.mid = None;
                        }
                    }
                }
                let answer = self.rtc.sdp_api().accept_offer(offer);
                self.drain();
                let answer = answer.map_err(|e| format!("OperationError: {e:?}"))?;
                self.answer = Some(answer.to_sdp_string());
                self.set_signaling(SignalingState::HaveRemoteOffer);
                // New m-lines are known now (MediaAdded): ontrack fires for them before the
                // promise resolves, as in browsers.
                let fire = self.refresh_directions();
                self.report(&fire);
                Ok(())
            },
            (_, SdpType::Rollback) => {
                self.pending = None;
                self.answer = None;
                self.set_signaling(SignalingState::Stable);
                Ok(())
            },
            (_, SdpType::Pranswer) => Err("pranswer is not supported".into()),
        }
    }

    fn stats(&self) -> Vec<RtpStats> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64() * 1000.)
            .unwrap_or(0.);
        let mut v = vec![];
        for t in &self.trs {
            let kind = if t.kind == MediaStreamType::Audio {
                "audio"
            } else {
                "video"
            };
            let mid = t.mid.map(|m| m.to_string());
            if receives(t.current) || t.packets_in > 0 {
                v.push(RtpStats {
                    id: format!("IT{}", t.id.0),
                    type_: "inbound-rtp",
                    kind,
                    ssrc: 0,
                    mid: mid.clone(),
                    bytes: t.bytes_in,
                    packets: t.packets_in,
                    packets_lost: t.lost_in,
                    jitter: 0.,
                    audio_level: t.level_in,
                    round_trip_time: None,
                    timestamp: now,
                });
            }
            if sends(t.current) || t.packets_out > 0 {
                v.push(RtpStats {
                    id: format!("OT{}", t.id.0),
                    type_: "outbound-rtp",
                    kind,
                    ssrc: 0,
                    mid,
                    bytes: t.bytes_out,
                    packets: t.packets_out,
                    packets_lost: 0,
                    jitter: 0.,
                    audio_level: 0.,
                    round_trip_time: None,
                    timestamp: now,
                });
            }
        }
        v
    }

    fn close(&mut self) {
        self.closed = true;
        self.stop_sockets.store(true, Ordering::Relaxed);
        self.rtc.disconnect();
        for t in &mut self.trs {
            t.send = None;
            t.recv.end();
        }
        self.set_signaling(SignalingState::Closed);
        if self.pc_state != PeerConnectionState::Closed {
            self.pc_state = PeerConnectionState::Closed;
            self.signaller
                .update_connection_state(PeerConnectionState::Closed);
        }
        self.signaller.close();
    }
}
