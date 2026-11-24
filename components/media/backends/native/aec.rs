/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Echo cancellation for microphones: sonora, the pure-Rust port of WebRTC's audio processing (AEC3, noise
//! suppression, AGC2, high-pass; BSD-3-Clause). On by default, as getUserMedia's echoCancellation is in Chrome and
//! WebKit; `PARTII_RUNTIME_AEC=0` (the remote config's kill switch) turns it off.
//!
//! The far end is what the output plays (`Output::render`, the device or the null output), downmixed to mono and
//! queued per microphone ("tap"). Each microphone, on its own thread, hands every whole 10 ms of queued far end to
//! its processor before each 10 ms capture frame, and pushes the processed mono frame.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use sonora::config::{AdaptiveDigital, EchoCanceller, GainController2, HighPassFilter, NoiseSuppression};
use sonora::{AudioProcessing, Config, StreamConfig};

/// The kill switch: anything but "0", "off" or "false" (or unset) keeps echo cancellation on.
pub(crate) fn enabled() -> bool {
    !matches!(
        std::env::var("PARTII_RUNTIME_AEC").as_deref(),
        Ok("0") | Ok("off") | Ok("false")
    )
}

/// At most this much far end waits in a tap (a stalled microphone must not grow it).
const FAR_MAX_SECS: usize = 1;
/// The render-to-capture delay hint; AEC3's own delay estimator finds the real one.
const DELAY_HINT_MS: i32 = 40;

/// What the output played, mono, at the output's rate.
pub(crate) struct Far {
    rate: u32,
    samples: VecDeque<f32>,
}

impl Far {
    /// Takes up to `n` samples (fewer when fewer wait).
    pub(crate) fn take(&mut self, n: usize, into: &mut Vec<f32>) -> u32 {
        let k = n.min(self.samples.len());
        into.extend(self.samples.drain(..k));
        self.rate
    }
}

static TAPS: Mutex<Vec<Weak<Mutex<Far>>>> = Mutex::new(Vec::new());
static ANY_TAP: AtomicBool = AtomicBool::new(false);

/// A new queue of everything the output plays from now on (dropped: the output stops filling it).
pub(crate) fn tap() -> Arc<Mutex<Far>> {
    let far = Arc::new(Mutex::new(Far {
        rate: 0,
        samples: VecDeque::new(),
    }));
    TAPS.lock().unwrap().push(Arc::downgrade(&far));
    ANY_TAP.store(true, Ordering::Release);
    far
}

/// From `Output::render`: the mixed output (interleaved, `channels`, at `rate`). Free when no microphone listens.
pub(crate) fn feed_render(out: &[f32], channels: usize, rate: u32) {
    if !ANY_TAP.load(Ordering::Acquire) {
        return;
    }
    // Never wait on the audio thread: a tap being added this instant just misses this period.
    let Ok(mut taps) = TAPS.try_lock() else {
        return;
    };
    taps.retain(|t| t.strong_count() > 0);
    if taps.is_empty() {
        ANY_TAP.store(false, Ordering::Release);
        return;
    }
    let ch = channels.max(1);
    let max = rate as usize * FAR_MAX_SECS;
    for tap in taps.iter().filter_map(Weak::upgrade) {
        let Ok(mut far) = tap.lock() else { continue };
        if far.rate != rate {
            far.rate = rate;
            far.samples.clear();
        }
        far.samples
            .extend(out.chunks(ch).map(|f| f.iter().sum::<f32>() / ch as f32));
        if far.samples.len() > max {
            let extra = far.samples.len() - max;
            far.samples.drain(..extra);
        }
    }
}

/// Which stages run (all on for a microphone; the test turns off the ones that would treat a tone as noise).
#[derive(Clone, Copy)]
pub(crate) struct Stages {
    pub noise_suppression: bool,
    pub gain_control: bool,
}

impl Default for Stages {
    fn default() -> Self {
        Stages {
            noise_suppression: true,
            gain_control: true,
        }
    }
}

fn config(stages: Stages) -> Config {
    Config {
        high_pass_filter: Some(HighPassFilter::default()),
        echo_canceller: Some(EchoCanceller::default()),
        noise_suppression: stages.noise_suppression.then(NoiseSuppression::default),
        gain_controller2: stages.gain_control.then(|| GainController2 {
            adaptive_digital: Some(AdaptiveDigital::default()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// One microphone's processor: interleaved capture in, processed mono 10 ms frames out.
pub(crate) struct Aec {
    far: Arc<Mutex<Far>>,
    stages: Stages,
    rate: u32,
    channels: usize,
    /// The processor and the far-end rate it was built for.
    apm: Option<(u32, AudioProcessing)>,
    pending: VecDeque<f32>,
    /// 10 ms far-end frames handed to the processor (the tests check the far end arrives).
    render_frames: u64,
    render: Vec<f32>,
    render_out: Vec<f32>,
    frame: Vec<f32>,
    out: Vec<f32>,
}

impl Aec {
    pub(crate) fn new(rate: u32, channels: u16) -> Aec {
        Aec::with_stages(rate, channels, Stages::default())
    }

    pub(crate) fn with_stages(rate: u32, channels: u16, stages: Stages) -> Aec {
        Aec {
            far: tap(),
            stages,
            rate,
            channels: channels.max(1) as usize,
            apm: None,
            pending: VecDeque::new(),
            render_frames: 0,
            render: Vec::new(),
            render_out: Vec::new(),
            frame: Vec::new(),
            out: Vec::new(),
        }
    }

    /// Feeds `interleaved` capture samples; `emit` gets each processed 10 ms mono frame.
    pub(crate) fn process(&mut self, interleaved: &[f32], mut emit: impl FnMut(&[f32])) {
        let ch = self.channels;
        self.pending
            .extend(interleaved.chunks(ch).map(|f| f.iter().sum::<f32>() / ch as f32));
        let n = (self.rate / 100) as usize;
        while n > 0 && self.pending.len() >= n {
            // Every whole 10 ms of far end that waits, first.
            self.render.clear();
            let far_rate = {
                let mut far = self.far.lock().unwrap();
                let rn = (far.rate / 100) as usize;
                let whole = if rn == 0 { 0 } else { far.samples.len() / rn * rn };
                far.take(whole, &mut self.render)
            };
            let far_rate = if far_rate == 0 { self.rate } else { far_rate };
            if self.apm.as_ref().is_none_or(|(r, _)| *r != far_rate) {
                let apm = AudioProcessing::builder()
                    .config(config(self.stages))
                    .capture_config(StreamConfig::new(self.rate, 1))
                    .render_config(StreamConfig::new(far_rate, 1))
                    .build();
                self.apm = Some((far_rate, apm));
            }
            let (_, apm) = self.apm.as_mut().unwrap();
            let rn = (far_rate / 100) as usize;
            self.render_out.resize(rn, 0.);
            for chunk in self.render.chunks_exact(rn) {
                let _ = apm.process_render_f32(&[chunk], &mut [&mut self.render_out[..]]);
                self.render_frames += 1;
            }
            let _ = apm.set_stream_delay_ms(DELAY_HINT_MS);
            self.frame.clear();
            self.frame.extend(self.pending.drain(..n));
            self.out.resize(n, 0.);
            if apm
                .process_capture_f32(&[&self.frame[..]], &mut [&mut self.out[..]])
                .is_err()
            {
                self.out.copy_from_slice(&self.frame);
            }
            emit(&self.out);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::f32::consts::PI;

    use super::*;

    /// Power of `hz` in `x` (Goertzel).
    fn power(x: &[f32], hz: f32, rate: f32) -> f32 {
        let w = 2. * PI * hz / rate;
        let c = 2. * w.cos();
        let (mut s1, mut s2) = (0f32, 0f32);
        for v in x {
            let s = v + c * s1 - s2;
            s2 = s1;
            s1 = s;
        }
        (s1 * s1 + s2 * s2 - c * s1 * s2) / x.len() as f32
    }

    struct Lcg(u32);
    impl Lcg {
        fn next(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(1664525).wrapping_add(1013904223);
            (self.0 >> 8) as f32 / (1u32 << 24) as f32 * 2. - 1.
        }
    }

    fn mean_sq(x: &[f32]) -> f32 {
        x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32
    }

    const RATE: u32 = 48000;
    /// The far end is process-wide (every tap gets it): one test at a time.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn aec() -> Aec {
        Aec::with_stages(
            RATE,
            1,
            Stages {
                noise_suppression: false,
                gain_control: false,
            },
        )
    }

    /// Single talk: a noise far end comes back into the microphone 30 ms later at half level. After 6 s the echo is
    /// at least 15 dB down (echo return loss enhancement over the last 2 s), and the far end reached the processor.
    #[test]
    fn cancels_a_far_end_echo() {
        let _one = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let n = (RATE / 100) as usize;
        let mut aec = aec();
        let mut rng = Lcg(7);
        let mut echo: VecDeque<f32> = std::iter::repeat_n(0., RATE as usize * 30 / 1000).collect();
        let (mut cap_all, mut out_all) = (Vec::new(), Vec::new());
        for _ in 0..600 {
            let far: Vec<f32> = (0..n).map(|_| 0.3 * rng.next()).collect();
            feed_render(&far, 1, RATE);
            let cap: Vec<f32> = far
                .iter()
                .map(|f| {
                    echo.push_back(*f);
                    0.5 * echo.pop_front().unwrap()
                })
                .collect();
            cap_all.extend_from_slice(&cap);
            aec.process(&cap, |f| out_all.extend_from_slice(f));
        }
        let last = 2 * RATE as usize;
        let erle = 10.
            * (mean_sq(&cap_all[cap_all.len() - last..]) / mean_sq(&out_all[out_all.len() - last..]).max(1e-20))
                .log10();
        let stats = aec.apm.as_ref().map(|(_, a)| format!("{:?}", a.statistics()));
        println!(
            "AEC-TEST echo: erle_db {erle:.1} render_frames {} stats {stats:?}",
            aec.render_frames
        );
        assert!(aec.render_frames >= 590, "the far end didn't reach the processor ({} frames)", aec.render_frames);
        assert!(erle >= 15., "echo only {erle:.1} dB down");
    }

    /// Near end only (the far end silent): a 440 Hz tone comes through within 3 dB.
    #[test]
    fn keeps_the_near_end() {
        let _one = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let n = (RATE / 100) as usize;
        let mut aec = aec();
        let (mut cap_all, mut out_all) = (Vec::new(), Vec::new());
        for i in 0..300 {
            feed_render(&vec![0.; n], 1, RATE);
            let cap: Vec<f32> = (0..n)
                .map(|k| 0.3 * (2. * PI * 440. * (i * n + k) as f32 / RATE as f32).sin())
                .collect();
            cap_all.extend_from_slice(&cap);
            aec.process(&cap, |f| out_all.extend_from_slice(f));
        }
        let last = RATE as usize;
        let loss = 10.
            * (power(&cap_all[cap_all.len() - last..], 440., RATE as f32)
                / power(&out_all[out_all.len() - last..], 440., RATE as f32).max(1e-20))
                .log10();
        println!("AEC-TEST near: loss_db {loss:.1}");
        assert!(loss.abs() <= 3., "near end changed {loss:.1} dB");
    }
}
