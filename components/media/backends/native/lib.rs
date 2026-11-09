/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii's pure-Rust servo-media backend (no GStreamer): cpal for output, symphonia for decoding.
//!
//! - Web Audio: [`sink::NativeAudioSink`] feeds the shared output; decodeAudioData uses
//!   [`decoder::NativeAudioDecoder`].
//! - `<audio>`: [`player::NativePlayer`], audio only.
//! - With no sound device (or `PARTII_AUDIO=null`) the output is a real-time null sink, so render
//!   threads and players still run.
//! - Video, capture, media streams and WebRTC behave like the dummy backend.

mod codecs;
pub mod decoder;
mod device;
pub mod output;
pub mod player;
pub mod queue;
pub mod resample;
pub mod sink;
#[cfg(feature = "video")]
mod webm;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, Weak};
use std::thread;

use log::warn;
use servo_base::generic_channel::GenericCallback;
use servo_media::{Backend, BackendInit, MediaInstanceError, SupportsMediaType};
use servo_media_audio::context::{AudioContext, AudioContextOptions};
use servo_media_audio::decoder::AudioDecoder;
use servo_media_audio::sink::AudioSinkError;
use servo_media_audio::{AudioBackend, AudioStreamReader};
use servo_media_dummy::{DummyBackend, DummyPlayer, DummyStreamReader, DummyWebRtcController};
use servo_media_player::context::PlayerGLContext;
use servo_media_player::{Player, PlayerEvent, StreamType, audio, video};
use servo_media_streams::capture::MediaTrackConstraintSet;
use servo_media_streams::device_monitor::MediaDeviceMonitor;
use servo_media_streams::registry::MediaStreamId;
use servo_media_streams::{MediaOutput, MediaSocket, MediaStreamType};
use servo_media_traits::{BackendMsg, ClientContextId, MediaInstance};
use servo_media_webrtc::{WebRtcBackend, WebRtcController, WebRtcSignaller};

type WeakInstances = HashMap<ClientContextId, Vec<(usize, Weak<Mutex<dyn MediaInstance>>)>>;

pub struct NativeBackend {
    instances: Arc<Mutex<WeakInstances>>,
    next_instance_id: AtomicUsize,
    backend_chan: Arc<Mutex<Sender<BackendMsg>>>,
}

impl BackendInit for NativeBackend {
    fn init() -> Box<dyn Backend> {
        let instances: Arc<Mutex<WeakInstances>> = Default::default();
        let instances_ = instances.clone();
        let (backend_chan, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("PartiiMediaShutdown".to_owned())
            .spawn(move || {
                while let Ok(BackendMsg::Shutdown {
                    context,
                    id,
                    tx_ack,
                }) = receiver.recv()
                {
                    let mut instances = instances_.lock().unwrap();
                    if let Some(vec) = instances.get_mut(&context) {
                        vec.retain(|m| m.0 != id);
                        if vec.is_empty() {
                            instances.remove(&context);
                        }
                    }
                    let _ = tx_ack.send(());
                }
            })
            .expect("Failed to spawn the media shutdown thread");
        Box::new(NativeBackend {
            instances,
            next_instance_id: AtomicUsize::new(0),
            backend_chan: Arc::new(Mutex::new(backend_chan)),
        })
    }
}

impl NativeBackend {
    fn register(
        &self,
        context: &ClientContextId,
        id: usize,
        instance: Weak<Mutex<dyn MediaInstance>>,
    ) {
        self.instances
            .lock()
            .unwrap()
            .entry(*context)
            .or_default()
            .push((id, instance));
    }

    fn media_instance_action(
        &self,
        id: &ClientContextId,
        cb: &dyn Fn(&dyn MediaInstance) -> Result<(), MediaInstanceError>,
    ) {
        let mut instances = self.instances.lock().unwrap();
        if let Some(vec) = instances.get_mut(id) {
            vec.retain(|(_, weak)| match weak.upgrade() {
                Some(instance) => {
                    if cb(&*instance.lock().unwrap()).is_err() {
                        warn!("Error executing media instance action");
                    }
                    true
                },
                None => false,
            });
        }
    }
}

/// What the decoders (codecs.rs) take: symphonia as built here, and Opus with the `opus` feature.
/// AAC is not built (royalty-free codecs only): mp4a.40.34/6b/69 are MP3 in MP4.
fn codec_supported(codec: &str) -> bool {
    let codec = codec.trim().trim_matches('"').to_ascii_lowercase();
    if codec == "opus" {
        return cfg!(feature = "opus");
    }
    if video_codec_supported(&codec) {
        return true;
    }
    matches!(
        codec.as_str(),
        "vorbis" | "mp3" | "mp4a.40.34" | "mp4a.6b" | "mp4a.69" | "flac" | "1" | "pcm"
    )
}

#[cfg(feature = "video")]
fn video_codec_supported(codec: &str) -> bool {
    webm::codec_supported(codec)
}

#[cfg(not(feature = "video"))]
fn video_codec_supported(_: &str) -> bool {
    false
}

/// WebM/Matroska video is playable: a video codec is built in.
fn video_containers() -> bool {
    #[cfg(feature = "video")]
    return webm::any_decoder();
    #[cfg(not(feature = "video"))]
    false
}

pub fn can_play_type(media_type: &str) -> SupportsMediaType {
    // Partii: parse leniently ("audio/ogg;", "audio/ogg;codecs", "audio/ogg;codecs=" are the bare
    // type, as in Chromium and Gecko); the mime crate rejects those.
    let mut parts = media_type.split(';');
    let essence = parts.next().unwrap_or("").trim().to_ascii_lowercase();
    if essence.split('/').count() != 2 || essence.split('/').any(|part| part.is_empty()) {
        return SupportsMediaType::No;
    }
    let codecs = parts.find_map(|param| {
        let (name, value) = param.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("codecs")
            .then(|| value.trim().trim_matches('"').trim().to_owned())
    });
    let container_ok = matches!(
        essence.as_str(),
        "audio/ogg" |
            "application/ogg" |
            "audio/vorbis" |
            "audio/mpeg" |
            "audio/mp3" |
            "audio/mpeg3" |
            "audio/x-mpeg" |
            "audio/wav" |
            "audio/wave" |
            "audio/x-wav" |
            "audio/vnd.wave" |
            "audio/flac" |
            "audio/x-flac" |
            "audio/mp4" |
            "audio/x-m4a" |
            "audio/m4a" |
            "audio/webm" |
            "audio/x-matroska"
    ) || (video_containers() && matches!(essence.as_str(), "video/webm" | "video/x-matroska"));
    if !container_ok {
        return SupportsMediaType::No;
    }
    match codecs.filter(|codecs| !codecs.is_empty()) {
        None => SupportsMediaType::Maybe,
        Some(codecs) => {
            if codecs.split(',').all(codec_supported) {
                SupportsMediaType::Probably
            } else {
                SupportsMediaType::No
            }
        },
    }
}

impl Backend for NativeBackend {
    fn create_player(
        &self,
        context_id: &ClientContextId,
        stream_type: StreamType,
        sender: GenericCallback<PlayerEvent>,
        video_renderer: Option<Arc<Mutex<dyn video::VideoFrameRenderer>>>,
        audio_renderer: Option<Arc<Mutex<dyn audio::AudioRenderer>>>,
        _gl_context: Box<dyn PlayerGLContext>,
    ) -> Arc<Mutex<dyn Player>> {
        let id = self.next_instance_id.fetch_add(1, Ordering::Relaxed);
        if stream_type == StreamType::Stream {
            // MediaStream sources (srcObject) are unsupported, as in the dummy backend.
            return Arc::new(Mutex::new(DummyPlayer));
        }
        let player = Arc::new(Mutex::new(player::NativePlayer::new(
            id,
            sender,
            audio_renderer,
            video_renderer,
        )));
        let weak: Weak<Mutex<player::NativePlayer>> = Arc::downgrade(&player);
        self.register(context_id, id, weak);
        player
    }

    fn create_audio_context(
        &self,
        client_context_id: &ClientContextId,
        options: AudioContextOptions,
    ) -> Result<Arc<Mutex<AudioContext>>, AudioSinkError> {
        let id = self.next_instance_id.fetch_add(1, Ordering::Relaxed);
        let context =
            AudioContext::new::<Self>(id, client_context_id, self.backend_chan.clone(), options)?;
        let context = Arc::new(Mutex::new(context));
        let weak: Weak<Mutex<AudioContext>> = Arc::downgrade(&context);
        self.register(client_context_id, id, weak);
        Ok(context)
    }

    fn create_audiostream(&self) -> MediaStreamId {
        DummyBackend.create_audiostream()
    }

    fn create_videostream(&self) -> MediaStreamId {
        DummyBackend.create_videostream()
    }

    fn create_stream_output(&self) -> Box<dyn MediaOutput> {
        DummyBackend.create_stream_output()
    }

    fn create_stream_and_socket(
        &self,
        ty: MediaStreamType,
    ) -> (Box<dyn MediaSocket>, MediaStreamId) {
        DummyBackend.create_stream_and_socket(ty)
    }

    fn create_audioinput_stream(&self, set: MediaTrackConstraintSet) -> Option<MediaStreamId> {
        DummyBackend.create_audioinput_stream(set)
    }

    fn create_videoinput_stream(&self, set: MediaTrackConstraintSet) -> Option<MediaStreamId> {
        DummyBackend.create_videoinput_stream(set)
    }

    fn create_webrtc(&self, signaller: Box<dyn WebRtcSignaller>) -> WebRtcController {
        WebRtcController::new::<Self>(signaller)
    }

    fn can_play_type(&self, media_type: &str) -> SupportsMediaType {
        can_play_type(media_type)
    }

    fn mute(&self, id: &ClientContextId, val: bool) {
        self.media_instance_action(id, &move |i: &dyn MediaInstance| i.mute(val));
    }

    fn suspend(&self, id: &ClientContextId) {
        self.media_instance_action(id, &|i: &dyn MediaInstance| i.suspend());
    }

    fn resume(&self, id: &ClientContextId) {
        self.media_instance_action(id, &|i: &dyn MediaInstance| i.resume());
    }

    fn get_device_monitor(&self) -> Box<dyn MediaDeviceMonitor> {
        DummyBackend.get_device_monitor()
    }

    fn preferred_sample_rate(&self) -> f32 {
        // The shared output's rate: the device's, or the null output's (PARTII_AUDIO_RATE,
        // default 48000).
        output::context_rate(output::output().format().0) as f32
    }

    fn base_latency(&self, sample_rate: f32) -> f64 {
        // The sink's minimum queue between the render thread and the output (1024 frames). A
        // fixed figure, so every context at one rate reports the same baseLatency (WPT
        // audiocontextoptions.html compares contexts); slow devices queue more, which
        // outputLatency's two periods account for.
        sink::MIN_TARGET_FRAMES as f64 / sample_rate.max(1.) as f64
    }

    fn output_latency(&self) -> f64 {
        // The device's own buffering: about two periods (none for the null output).
        let output = output::output();
        match output.format().2 {
            output::OutputKind::Device => 2. * output.period_secs(),
            output::OutputKind::Null => 0.,
        }
    }
}

impl AudioBackend for NativeBackend {
    type Sink = sink::NativeAudioSink;

    fn make_decoder() -> Box<dyn AudioDecoder> {
        Box::new(decoder::NativeAudioDecoder)
    }

    fn make_sink() -> Result<Self::Sink, AudioSinkError> {
        Ok(sink::NativeAudioSink::new())
    }

    fn make_streamreader(
        _id: MediaStreamId,
        _sample_rate: f32,
    ) -> Result<Box<dyn AudioStreamReader + Send>, AudioSinkError> {
        Ok(Box::new(DummyStreamReader))
    }
}

impl WebRtcBackend for NativeBackend {
    type Controller = DummyWebRtcController;

    fn construct_webrtc_controller(
        _: Box<dyn WebRtcSignaller>,
        _: WebRtcController,
    ) -> Self::Controller {
        DummyWebRtcController
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_play_types() {
        assert_eq!(can_play_type("audio/ogg"), SupportsMediaType::Maybe);
        assert_eq!(
            can_play_type("audio/ogg; codecs=\"vorbis\""),
            SupportsMediaType::Probably
        );
        let opus = if cfg!(feature = "opus") {
            SupportsMediaType::Probably
        } else {
            SupportsMediaType::No
        };
        assert_eq!(can_play_type("audio/ogg; codecs=\"opus\""), opus);
        assert_eq!(can_play_type("audio/webm; codecs=opus"), opus);
        // No AAC (royalty-free codecs only, 2026-11-09): mp4/m4a with AAC and ADTS .aac are "".
        assert_eq!(
            can_play_type("audio/mp4; codecs=\"mp4a.40.2\""),
            SupportsMediaType::No
        );
        assert_eq!(can_play_type("audio/aac"), SupportsMediaType::No);
        assert_eq!(
            can_play_type("audio/mp4; codecs=\"mp4a.40.34\""),
            SupportsMediaType::Probably
        );
        assert_eq!(can_play_type("video/mp4"), SupportsMediaType::No);
        let vpx = if cfg!(feature = "vpx") {
            SupportsMediaType::Probably
        } else {
            SupportsMediaType::No
        };
        let av1 = if cfg!(feature = "av1") {
            SupportsMediaType::Probably
        } else {
            SupportsMediaType::No
        };
        assert_eq!(can_play_type("video/webm; codecs=\"vp8, vorbis\""), vpx);
        assert_eq!(can_play_type("video/webm; codecs=\"vp09.00.10.08\""), vpx);
        assert_eq!(can_play_type("video/webm; codecs=av01.0.04M.08"), av1);
        assert_eq!(
            can_play_type("video/webm; codecs=\"avc1.42E01E\""),
            SupportsMediaType::No
        );
        assert_eq!(can_play_type("audio/mpeg"), SupportsMediaType::Maybe);
        assert_eq!(can_play_type("audio/ogg;"), SupportsMediaType::Maybe);
        assert_eq!(can_play_type("audio/ogg;codecs"), SupportsMediaType::Maybe);
        assert_eq!(can_play_type("audio/ogg;codecs="), SupportsMediaType::Maybe);
        assert_eq!(
            can_play_type("audio/webm; codecs=\"vorbis\""),
            SupportsMediaType::Probably
        );
        assert_eq!(
            can_play_type("application/octet-stream"),
            SupportsMediaType::No
        );
    }
}
