/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::f64::consts::{PI, SQRT_2};

use malloc_size_of_derive::MallocSizeOf;
use smallvec::SmallVec;

use crate::audio_node::{AudioNodeEngine, AudioNodeMessage, AudioNodeType, BlockInfo, ChannelInfo};
use crate::block::{Chunk, Tick};
use crate::param::{Param, ParamType};

#[derive(Copy, Clone, Debug, MallocSizeOf)]
pub struct BiquadFilterNodeOptions {
    pub filter: FilterType,
    pub frequency: f32,
    pub detune: f32,
    pub q: f32,
    pub gain: f32,
}

#[derive(Copy, Clone, Debug, MallocSizeOf)]
pub enum FilterType {
    LowPass,
    HighPass,
    BandPass,
    LowShelf,
    HighShelf,
    Peaking,
    Notch,
    AllPass,
}

impl Default for BiquadFilterNodeOptions {
    fn default() -> Self {
        BiquadFilterNodeOptions {
            filter: FilterType::LowPass,
            frequency: 350.,
            detune: 0.,
            q: 1.,
            gain: 0.,
        }
    }
}

#[derive(Copy, Clone, Debug, MallocSizeOf)]
pub enum BiquadFilterNodeMessage {
    SetFilterType(FilterType),
}

/// The last two input and output values, per-channel
// Default sets all fields to zero
#[derive(Default, Copy, Clone, PartialEq)]
struct BiquadState {
    /// The input value from last frame
    x1: f64,
    /// The input value from two frames ago
    x2: f64,
    /// The output value from last frame
    y1: f64,
    /// The output value from two frames ago
    y2: f64,
}

impl BiquadState {
    /// Update with new input/output values from this frame
    fn update(&mut self, x: f64, y: f64) {
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
    }
}

/// <https://webaudio.github.io/web-audio-api/#biquadfilternode>
#[derive(AudioNodeCommon)]
pub(crate) struct BiquadFilterNode {
    channel_info: ChannelInfo,
    filter: FilterType,
    frequency: Param,
    detune: Param,
    q: Param,
    gain: Param,
    /// The computed filter parameter b0
    /// This is actually b0 / a0, we pre-divide
    /// for efficiency
    b0: f64,
    /// The computed filter parameter b1
    /// This is actually b1 / a0, we pre-divide
    /// for efficiency
    b1: f64,
    /// The computed filter parameter b2
    /// This is actually b2 / a0, we pre-divide
    /// for efficiency
    b2: f64,
    /// The computed filter parameter a1
    /// This is actually a1 / a0, we pre-divide
    /// for efficiency
    a1: f64,
    /// The computed filter parameter a2
    /// This is actually a2 / a0, we pre-divide
    /// for efficiency
    a2: f64,
    /// Stored filter state, this contains the last two
    /// frames of input and output values for every
    /// channel
    state: SmallVec<[BiquadState; 2]>,
}

impl BiquadFilterNode {
    pub fn new(
        options: BiquadFilterNodeOptions,
        channel_info: ChannelInfo,
        sample_rate: f32,
    ) -> Self {
        let mut ret = Self {
            channel_info,
            filter: options.filter,
            frequency: Param::new(options.frequency),
            gain: Param::new(options.gain),
            q: Param::new(options.q),
            detune: Param::new(options.detune),
            b0: 0.,
            b1: 0.,
            b2: 0.,
            a1: 0.,
            a2: 0.,
            state: SmallVec::new(),
        };
        ret.update_coefficients(sample_rate);
        ret
    }

    pub fn update_parameters(&mut self, info: &BlockInfo, tick: Tick) -> bool {
        let mut changed = self.frequency.update(info, tick);
        changed |= self.detune.update(info, tick);
        changed |= self.q.update(info, tick);
        changed |= self.gain.update(info, tick);

        if changed {
            self.update_coefficients(info.sample_rate);
        }
        changed
    }

    /// Update the coefficients a1, a2, b0, b1, b2, given the sample_rate
    ///
    /// See <https://webaudio.github.io/web-audio-api/#filters-characteristics>
    fn update_coefficients(&mut self, fs: f32) {
        [self.b0, self.b1, self.b2, self.a1, self.a2] = biquad_coefficients(
            self.filter,
            self.frequency.value().into(),
            self.detune.value().into(),
            self.q.value().into(),
            self.gain.value().into(),
            fs.into(),
        );
    }
}

/// <https://webaudio.github.io/web-audio-api/#dom-biquadfilternode-getfrequencyresponse>,
/// for the given current parameter values.
pub fn biquad_frequency_response(
    options: &BiquadFilterNodeOptions,
    sample_rate: f32,
    frequency_hz: &[f32],
    mag_response: &mut [f32],
    phase_response: &mut [f32],
) {
    let [b0, b1, b2, a1, a2] = biquad_coefficients(
        options.filter,
        options.frequency.into(),
        options.detune.into(),
        options.q.into(),
        options.gain.into(),
        sample_rate.into(),
    );
    crate::iir_filter_node::IIRFilterNode::get_frequency_response(
        &[b0, b1, b2],
        &[1., a1, a2],
        frequency_hz,
        mag_response,
        phase_response,
        sample_rate,
    );
}

/// The filter's coefficients [b0, b1, b2, a1, a2], each divided by a0.
///
/// See <https://webaudio.github.io/web-audio-api/#filters-characteristics>
pub fn biquad_coefficients(
    filter: FilterType,
    freq: f64,
    detune: f64,
    q: f64,
    g: f64,
    fs: f64,
) -> [f64; 5] {
    // The constant z-transform y[n] = b0 * x[n]
    let constant = |b0: f64| [b0, 0., 0., 0., 0.];
    let f0: f64 = freq * (2.0_f64).powf(detune / 1200.);
    // clamp to nominal range
    // https://webaudio.github.io/web-audio-api/#biquadfilternode
    let f0 = if f0 > fs / 2. || !f0.is_finite() {
        fs / 2.
    } else if f0 < 0. {
        0.
    } else {
        f0
    };

    // Normalized to Nyquist (as the spec's limits are): 1 is fs / 2. Normalizing to fs
    // left the `== 1.` limits unreachable, so a filter at Nyquist got poles on the unit
    // circle (a band-pass at 0.5 * fs rang and grew instead of outputting 0).
    let normalized = f0 / (fs / 2.);
    let a = 10.0_f64.powf(g / 40.);

    // the boundary values sometimes need limits to
    // be taken
    match filter {
        FilterType::LowPass => {
            if normalized == 1. {
                return constant(1.);
            } else if normalized == 0. {
                return constant(0.);
            }
        },
        FilterType::HighPass => {
            if normalized == 1. {
                return constant(0.);
            } else if normalized == 0. {
                return constant(1.);
            }
        },
        FilterType::LowShelf => {
            if normalized == 1. {
                return constant(a * a);
            } else if normalized == 0. {
                return constant(1.);
            }
        },
        FilterType::HighShelf => {
            if normalized == 1. {
                return constant(1.);
            } else if normalized == 0. {
                return constant(a * a);
            }
        },
        FilterType::Peaking => {
            if normalized == 0. || normalized == 1. {
                return constant(1.);
            } else if q <= 0. {
                return constant(a * a);
            }
        },
        FilterType::AllPass => {
            if normalized == 0. || normalized == 1. {
                return constant(1.);
            } else if q <= 0. {
                return constant(-1.);
            }
        },
        FilterType::Notch => {
            if normalized == 0. || normalized == 1. {
                return constant(1.);
            } else if q <= 0. {
                return constant(0.);
            }
        },
        FilterType::BandPass => {
            if normalized == 0. || normalized == 1. {
                return constant(0.);
            } else if q <= 0. {
                return constant(1.);
            }
        },
    }

    let omega0 = PI * normalized;
    let sin_omega = omega0.sin();
    let cos_omega = omega0.cos();
    let alpha_q = sin_omega / (2. * q);
    let alpha_q_db = sin_omega / (2. * 10.0_f64.powf(q / 20.));
    let alpha_s = sin_omega / SQRT_2;

    let (b0, b1, b2, a0, a1, a2) = match filter {
        FilterType::LowPass => {
            let b1 = 1. - cos_omega;
            (
                (1. - cos_omega) / 2.,
                b1,
                b1 / 2.,
                1. + alpha_q_db,
                -2. * cos_omega,
                1. - alpha_q_db,
            )
        },
        FilterType::HighPass => {
            let b1 = -(1. + cos_omega);
            (
                (1. + cos_omega) / 2.,
                b1,
                -b1 / 2.,
                1. + alpha_q_db,
                -2. * cos_omega,
                1. - alpha_q_db,
            )
        },
        FilterType::BandPass => (
            alpha_q,
            0.,
            -alpha_q,
            1. + alpha_q,
            -2. * cos_omega,
            1. - alpha_q,
        ),
        FilterType::Notch => (
            1.,
            -2. * cos_omega,
            1.,
            1. + alpha_q,
            -2. * cos_omega,
            1. - alpha_q,
        ),
        FilterType::AllPass => (
            1. - alpha_q,
            -2. * cos_omega,
            1. + alpha_q,
            1. + alpha_q,
            -2. * cos_omega,
            1. - alpha_q,
        ),
        FilterType::Peaking => (
            1. + alpha_q * a,
            -2. * cos_omega,
            1. - alpha_q * a,
            1. + alpha_q / a,
            -2. * cos_omega,
            1. - alpha_q / a,
        ),
        FilterType::LowShelf => {
            let alpha_rt_a = 2. * alpha_s * a.sqrt();
            (
                a * ((a + 1.) - (a - 1.) * cos_omega + alpha_rt_a),
                2. * a * ((a - 1.) - (a + 1.) * cos_omega),
                a * ((a + 1.) - (a - 1.) * cos_omega - alpha_rt_a),
                (a + 1.) + (a - 1.) * cos_omega + alpha_rt_a,
                -2. * ((a - 1.) + (a + 1.) * cos_omega),
                (a + 1.) + (a - 1.) * cos_omega - alpha_rt_a,
            )
        },
        FilterType::HighShelf => {
            let alpha_rt_a = 2. * alpha_s * a.sqrt();
            (
                a * ((a + 1.) + (a - 1.) * cos_omega + alpha_rt_a),
                -2. * a * ((a - 1.) + (a + 1.) * cos_omega),
                a * ((a + 1.) + (a - 1.) * cos_omega - alpha_rt_a),
                (a + 1.) - (a - 1.) * cos_omega + alpha_rt_a,
                2. * ((a - 1.) - (a + 1.) * cos_omega),
                (a + 1.) - (a - 1.) * cos_omega - alpha_rt_a,
            )
        },
    };
    [b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0]
}

impl AudioNodeEngine for BiquadFilterNode {
    fn node_type(&self) -> AudioNodeType {
        AudioNodeType::BiquadFilterNode
    }

    /// Tail: the filter rings after its input stops; removable once its memory has decayed
    /// below audibility.
    fn removable(&self, _now: Tick) -> bool {
        const QUIET: f64 = 1e-6;
        self.state
            .iter()
            .all(|s| s.x1.abs() < QUIET && s.x2.abs() < QUIET && s.y1.abs() < QUIET && s.y2.abs() < QUIET)
    }

    fn process(&mut self, mut inputs: Chunk, info: &BlockInfo) -> Chunk {
        debug_assert!(inputs.len() == 1);
        self.state
            .resize(inputs.blocks[0].chan_count() as usize, Default::default());
        self.update_parameters(info, Tick(0));

        // XXXManishearth this node has tail time, so even if the block is silence
        // we must still compute things on it. However, it is possible to become
        // a dumb passthrough as long as we reach a quiescent state
        //
        // see https://dxr.mozilla.org/mozilla-central/rev/87a95e1b7ec691bef7b938e722fe1b01cce68664/dom/media/webaudio/blink/Biquad.cpp#81-91

        let repeat_or_silence = inputs.blocks[0].is_silence() || inputs.blocks[0].is_repeat();

        if repeat_or_silence && !self.state.iter().all(|s| *s == self.state[0]) {
            // In case our input is repeat/silence but our states are not identical, we must
            // explicitly duplicate, since mutate_with will otherwise only operate
            // on the first channel, ignoring the states of the later ones
            inputs.blocks[0].explicit_repeat();
        } else {
            // In case the states are identical, just make any silence explicit,
            // since mutate_with can't handle silent blocks
            inputs.blocks[0].explicit_silence();
        }

        {
            let mut iter = inputs.blocks[0].iter();
            while let Some(mut frame) = iter.next() {
                self.update_parameters(info, frame.tick());
                frame.mutate_with(|sample, chan| {
                    let state = &mut self.state[chan as usize];
                    let x0 = *sample as f64;
                    let y0 = self.b0 * x0 + self.b1 * state.x1 + self.b2 * state.x2 -
                        self.a1 * state.y1 -
                        self.a2 * state.y2;
                    *sample = y0 as f32;
                    state.update(x0, y0);
                });
            }
        }

        if inputs.blocks[0].is_repeat() {
            let state = self.state[0];
            self.state.iter_mut().for_each(|s| *s = state);
        }

        inputs
    }

    fn get_param(&mut self, id: ParamType) -> &mut Param {
        match id {
            ParamType::Frequency => &mut self.frequency,
            ParamType::Detune => &mut self.detune,
            ParamType::Q => &mut self.q,
            ParamType::Gain => &mut self.gain,
            _ => panic!("Unknown param {:?} for BiquadFilterNode", id),
        }
    }

    fn message_specific(&mut self, message: AudioNodeMessage, sample_rate: f32) {
        if let AudioNodeMessage::BiquadFilterNode(m) = message {
            match m {
                BiquadFilterNodeMessage::SetFilterType(f) => {
                    self.filter = f;
                    self.update_coefficients(sample_rate);
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowpass_response_passes_dc_and_is_nan_past_nyquist() {
        let options = BiquadFilterNodeOptions {
            filter: FilterType::LowPass,
            frequency: 1000.,
            detune: 0.,
            q: 1.,
            gain: 0.,
        };
        let freqs = [0., 1000., 24000., 24001., -1.];
        let mut mag = [0.; 5];
        let mut phase = [0.; 5];
        biquad_frequency_response(&options, 48000., &freqs, &mut mag, &mut phase);
        assert!((mag[0] - 1.).abs() < 1e-6, "{}", mag[0]);
        assert!(phase[0].abs() < 1e-6, "{}", phase[0]);
        // Q = 1 dB at the cutoff: |H| = 10^(1/20).
        assert!((mag[1] - 10f32.powf(1. / 20.)).abs() < 1e-3, "{}", mag[1]);
        assert!(mag[2] < 1e-6, "{}", mag[2]);
        assert!(mag[3].is_nan() && mag[4].is_nan());
    }

    #[test]
    fn filters_at_nyquist_take_the_limits() {
        let fs = 44100.;
        // biquad-automation.html: a band-pass detuned past Nyquist outputs nothing.
        let c = biquad_coefficients(FilterType::BandPass, 4400., 12000., 1., 0., fs);
        assert_eq!(c, [0., 0., 0., 0., 0.]);
        let c = biquad_coefficients(FilterType::LowPass, fs / 2., 0., 1., 0., fs);
        assert_eq!(c, [1., 0., 0., 0., 0.]);
        // Below Nyquist the coefficients are unchanged: w0 = 2 pi f0 / fs.
        let c = biquad_coefficients(FilterType::BandPass, 1000., 0., 1., 0., fs);
        let w0 = 2. * PI * 1000. / fs;
        let alpha = w0.sin() / 2.;
        assert!((c[0] - alpha / (1. + alpha)).abs() < 1e-12, "{:?}", c);
    }
}
