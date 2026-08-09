use sampler_core::{DelaySettings, MasterMixSettings, ReverbSettings};

use crate::EngineError;

const PARAMETER_RAMP_FRAMES: u32 = 64;
const TAP_CROSSFADE_FRAMES: u32 = 128;
const MAX_DELAY_MS: usize = 2_000;
const REVERB_REFERENCE_RATE: u64 = 44_100;
const REVERB_INPUT_GAIN: f32 = 0.125;

const COMB_LENGTHS_LEFT: [u32; 8] = [1_116, 1_188, 1_277, 1_356, 1_422, 1_491, 1_557, 1_617];
const COMB_LENGTHS_RIGHT: [u32; 8] = [1_139, 1_211, 1_300, 1_379, 1_445, 1_514, 1_580, 1_640];
const ALL_PASS_LENGTHS_LEFT: [u32; 4] = [556, 441, 341, 225];
const ALL_PASS_LENGTHS_RIGHT: [u32; 4] = [579, 464, 364, 248];

pub(crate) struct LinearRamp {
    current: f32,
    target: f32,
    step: f32,
    remaining: u32,
}

impl LinearRamp {
    pub(crate) fn new(value: f32) -> Self {
        Self {
            current: value,
            target: value,
            step: 0.0,
            remaining: 0,
        }
    }

    pub(crate) fn set_target(&mut self, target: f32) {
        if self.target.to_bits() == target.to_bits() {
            return;
        }
        self.target = target;
        self.step = (target - self.current) / PARAMETER_RAMP_FRAMES as f32;
        self.remaining = PARAMETER_RAMP_FRAMES;
    }

    pub(crate) fn next(&mut self) -> f32 {
        if self.remaining == 0 {
            return self.current;
        }
        self.current += self.step;
        self.remaining -= 1;
        if self.remaining == 0 {
            self.current = self.target;
        }
        self.current
    }
}

pub(crate) struct DelayBus {
    sample_rate: u32,
    left: Vec<f32>,
    right: Vec<f32>,
    write_index: usize,
    old_tap_frames: usize,
    new_tap_frames: usize,
    tap_crossfade_remaining: u32,
    input_gain: LinearRamp,
    feedback: LinearRamp,
}

impl DelayBus {
    pub(crate) fn new(sample_rate: u32, settings: DelaySettings) -> Result<Self, EngineError> {
        let capacity = delay_capacity(sample_rate)?;
        let tap_frames = delay_frames(sample_rate, settings.time_ms, capacity);
        Ok(Self {
            sample_rate,
            left: zeroed_buffer(capacity)?,
            right: zeroed_buffer(capacity)?,
            write_index: 0,
            old_tap_frames: tap_frames,
            new_tap_frames: tap_frames,
            tap_crossfade_remaining: 0,
            input_gain: LinearRamp::new(if settings.enabled { 1.0 } else { 0.0 }),
            feedback: LinearRamp::new(settings.feedback),
        })
    }

    pub(crate) fn set_settings(&mut self, settings: DelaySettings) {
        let tap_frames = delay_frames(self.sample_rate, settings.time_ms, self.left.len());
        if tap_frames != self.new_tap_frames {
            self.old_tap_frames = self.new_tap_frames;
            self.new_tap_frames = tap_frames;
            self.tap_crossfade_remaining = TAP_CROSSFADE_FRAMES;
        }
        self.input_gain
            .set_target(if settings.enabled { 1.0 } else { 0.0 });
        self.feedback.set_target(settings.feedback);
    }

    pub(crate) fn process(&mut self, input: [f32; 2], invalid_commands: &mut u64) -> [f32; 2] {
        let old = self.read_tap(self.old_tap_frames);
        let new = self.read_tap(self.new_tap_frames);
        let wet = if self.tap_crossfade_remaining == 0 {
            new
        } else {
            let elapsed = TAP_CROSSFADE_FRAMES - self.tap_crossfade_remaining + 1;
            let mix = elapsed as f32 / TAP_CROSSFADE_FRAMES as f32;
            self.tap_crossfade_remaining -= 1;
            if self.tap_crossfade_remaining == 0 {
                self.old_tap_frames = self.new_tap_frames;
            }
            [
                finite_or_zero(old[0] * (1.0 - mix) + new[0] * mix, invalid_commands),
                finite_or_zero(old[1] * (1.0 - mix) + new[1] * mix, invalid_commands),
            ]
        };

        let input_gain = self.input_gain.next();
        let feedback = self.feedback.next();
        self.left[self.write_index] =
            finite_or_zero(input[0] * input_gain + wet[1] * feedback, invalid_commands);
        self.right[self.write_index] =
            finite_or_zero(input[1] * input_gain + wet[0] * feedback, invalid_commands);
        self.write_index += 1;
        if self.write_index == self.left.len() {
            self.write_index = 0;
        }
        wet
    }

    fn read_tap(&self, tap_frames: usize) -> [f32; 2] {
        let index = (self.write_index + self.left.len() - tap_frames) % self.left.len();
        [self.left[index], self.right[index]]
    }
}

struct CombFilter {
    buffer: Vec<f32>,
    index: usize,
    low_pass: f32,
}

impl CombFilter {
    fn new(length: usize) -> Result<Self, EngineError> {
        Ok(Self {
            buffer: zeroed_buffer(length)?,
            index: 0,
            low_pass: 0.0,
        })
    }

    fn process(
        &mut self,
        input: f32,
        feedback: f32,
        damping: f32,
        invalid_commands: &mut u64,
    ) -> f32 {
        let output = self.buffer[self.index];
        self.low_pass = finite_or_zero(
            output * (1.0 - damping) + self.low_pass * damping,
            invalid_commands,
        );
        self.buffer[self.index] =
            finite_or_zero(input + self.low_pass * feedback, invalid_commands);
        self.index += 1;
        if self.index == self.buffer.len() {
            self.index = 0;
        }
        output
    }
}

struct AllPassFilter {
    buffer: Vec<f32>,
    index: usize,
}

impl AllPassFilter {
    fn new(length: usize) -> Result<Self, EngineError> {
        Ok(Self {
            buffer: zeroed_buffer(length)?,
            index: 0,
        })
    }

    fn process(&mut self, input: f32, invalid_commands: &mut u64) -> f32 {
        let buffered = self.buffer[self.index];
        let output = finite_or_zero(buffered - input, invalid_commands);
        self.buffer[self.index] = finite_or_zero(input + buffered * 0.5, invalid_commands);
        self.index += 1;
        if self.index == self.buffer.len() {
            self.index = 0;
        }
        output
    }
}

pub(crate) struct ReverbBus {
    combs_left: [CombFilter; 8],
    combs_right: [CombFilter; 8],
    all_passes_left: [AllPassFilter; 4],
    all_passes_right: [AllPassFilter; 4],
    input_gain: LinearRamp,
    room_size: LinearRamp,
    damping: LinearRamp,
}

impl ReverbBus {
    pub(crate) fn new(sample_rate: u32, settings: ReverbSettings) -> Result<Self, EngineError> {
        let comb_left = scaled_lengths(COMB_LENGTHS_LEFT, sample_rate)?;
        let comb_right = scaled_lengths(COMB_LENGTHS_RIGHT, sample_rate)?;
        let all_pass_left = scaled_lengths(ALL_PASS_LENGTHS_LEFT, sample_rate)?;
        let all_pass_right = scaled_lengths(ALL_PASS_LENGTHS_RIGHT, sample_rate)?;
        Ok(Self {
            combs_left: [
                CombFilter::new(comb_left[0])?,
                CombFilter::new(comb_left[1])?,
                CombFilter::new(comb_left[2])?,
                CombFilter::new(comb_left[3])?,
                CombFilter::new(comb_left[4])?,
                CombFilter::new(comb_left[5])?,
                CombFilter::new(comb_left[6])?,
                CombFilter::new(comb_left[7])?,
            ],
            combs_right: [
                CombFilter::new(comb_right[0])?,
                CombFilter::new(comb_right[1])?,
                CombFilter::new(comb_right[2])?,
                CombFilter::new(comb_right[3])?,
                CombFilter::new(comb_right[4])?,
                CombFilter::new(comb_right[5])?,
                CombFilter::new(comb_right[6])?,
                CombFilter::new(comb_right[7])?,
            ],
            all_passes_left: [
                AllPassFilter::new(all_pass_left[0])?,
                AllPassFilter::new(all_pass_left[1])?,
                AllPassFilter::new(all_pass_left[2])?,
                AllPassFilter::new(all_pass_left[3])?,
            ],
            all_passes_right: [
                AllPassFilter::new(all_pass_right[0])?,
                AllPassFilter::new(all_pass_right[1])?,
                AllPassFilter::new(all_pass_right[2])?,
                AllPassFilter::new(all_pass_right[3])?,
            ],
            input_gain: LinearRamp::new(if settings.enabled { 1.0 } else { 0.0 }),
            room_size: LinearRamp::new(settings.room_size),
            damping: LinearRamp::new(settings.damping),
        })
    }

    pub(crate) fn set_settings(&mut self, settings: ReverbSettings) {
        self.input_gain
            .set_target(if settings.enabled { 1.0 } else { 0.0 });
        self.room_size.set_target(settings.room_size);
        self.damping.set_target(settings.damping);
    }

    pub(crate) fn process(&mut self, input: [f32; 2], invalid_commands: &mut u64) -> [f32; 2] {
        let input_gain = self.input_gain.next();
        let mono_input = finite_or_zero(
            (input[0] + input[1]) * 0.5 * input_gain * REVERB_INPUT_GAIN,
            invalid_commands,
        );
        let feedback = 0.7 + self.room_size.next() * 0.28;
        let damping = self.damping.next();

        let mut left = 0.0;
        for comb in &mut self.combs_left {
            left = finite_or_zero(
                left + comb.process(mono_input, feedback, damping, invalid_commands),
                invalid_commands,
            );
        }
        let mut right = 0.0;
        for comb in &mut self.combs_right {
            right = finite_or_zero(
                right + comb.process(mono_input, feedback, damping, invalid_commands),
                invalid_commands,
            );
        }

        left = finite_or_zero(left * 0.125, invalid_commands);
        right = finite_or_zero(right * 0.125, invalid_commands);
        for all_pass in &mut self.all_passes_left {
            left = all_pass.process(left, invalid_commands);
        }
        for all_pass in &mut self.all_passes_right {
            right = all_pass.process(right, invalid_commands);
        }
        [left, right]
    }
}

pub(crate) struct FxRack {
    delay: DelayBus,
    reverb: ReverbBus,
    master_gain: LinearRamp,
    delay_return: LinearRamp,
    reverb_return: LinearRamp,
    settings: MasterMixSettings,
}

impl FxRack {
    pub(crate) fn new(sample_rate: u32, settings: MasterMixSettings) -> Result<Self, EngineError> {
        if sample_rate == 0 {
            return Err(EngineError::ZeroSampleRate);
        }
        Ok(Self {
            delay: DelayBus::new(sample_rate, settings.delay)?,
            reverb: ReverbBus::new(sample_rate, settings.reverb)?,
            master_gain: LinearRamp::new(db_to_gain(settings.gain_db)),
            delay_return: LinearRamp::new(effect_return_gain(
                settings.delay.enabled,
                settings.delay.return_db,
            )),
            reverb_return: LinearRamp::new(effect_return_gain(
                settings.reverb.enabled,
                settings.reverb.return_db,
            )),
            settings,
        })
    }

    pub(crate) fn set_settings(&mut self, settings: MasterMixSettings) {
        self.delay.set_settings(settings.delay);
        self.reverb.set_settings(settings.reverb);
        self.master_gain.set_target(db_to_gain(settings.gain_db));
        self.delay_return.set_target(effect_return_gain(
            settings.delay.enabled,
            settings.delay.return_db,
        ));
        self.reverb_return.set_target(effect_return_gain(
            settings.reverb.enabled,
            settings.reverb.return_db,
        ));
        self.settings = settings;
    }

    pub(crate) fn process(
        &mut self,
        dry: [f32; 2],
        delay_input: [f32; 2],
        reverb_input: [f32; 2],
        invalid_commands: &mut u64,
    ) -> [f32; 2] {
        let dry = sanitize_frame(dry, invalid_commands);
        let delay_input = sanitize_frame(delay_input, invalid_commands);
        let reverb_input = sanitize_frame(reverb_input, invalid_commands);
        let delay_wet = self.delay.process(delay_input, invalid_commands);
        let reverb_wet = self.reverb.process(reverb_input, invalid_commands);
        let delay_gain = self.delay_return.next();
        let reverb_gain = self.reverb_return.next();
        let master_gain = self.master_gain.next();

        [
            finite_or_zero(
                finite_or_zero(
                    dry[0] + delay_wet[0] * delay_gain + reverb_wet[0] * reverb_gain,
                    invalid_commands,
                ) * master_gain,
                invalid_commands,
            ),
            finite_or_zero(
                finite_or_zero(
                    dry[1] + delay_wet[1] * delay_gain + reverb_wet[1] * reverb_gain,
                    invalid_commands,
                ) * master_gain,
                invalid_commands,
            ),
        ]
    }
}

fn effect_return_gain(enabled: bool, return_db: f32) -> f32 {
    if enabled { db_to_gain(return_db) } else { 0.0 }
}

fn db_to_gain(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

fn delay_capacity(sample_rate: u32) -> Result<usize, EngineError> {
    let rate = usize::try_from(sample_rate).map_err(|_| EngineError::EffectBufferSizeOverflow)?;
    rate.checked_mul(MAX_DELAY_MS)
        .and_then(|scaled| scaled.checked_add(999))
        .map(|scaled| (scaled / 1_000).max(1))
        .ok_or(EngineError::EffectBufferSizeOverflow)
}

fn delay_frames(sample_rate: u32, time_ms: u16, capacity: usize) -> usize {
    let scaled = u64::from(sample_rate) * u64::from(time_ms);
    usize::try_from(scaled / 1_000)
        .unwrap_or(capacity)
        .clamp(1, capacity)
}

fn scaled_lengths<const N: usize>(
    reference_lengths: [u32; N],
    sample_rate: u32,
) -> Result<[usize; N], EngineError> {
    let mut scaled = [1; N];
    for (index, reference) in reference_lengths.into_iter().enumerate() {
        let numerator = u64::from(reference)
            .checked_mul(u64::from(sample_rate))
            .and_then(|value| value.checked_add(REVERB_REFERENCE_RATE / 2))
            .ok_or(EngineError::EffectBufferSizeOverflow)?;
        scaled[index] = usize::try_from((numerator / REVERB_REFERENCE_RATE).max(1))
            .map_err(|_| EngineError::EffectBufferSizeOverflow)?;
    }
    Ok(scaled)
}

fn zeroed_buffer(length: usize) -> Result<Vec<f32>, EngineError> {
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(length)
        .map_err(|_| EngineError::EffectBufferAllocationFailed)?;
    buffer.resize(length, 0.0);
    Ok(buffer)
}

fn sanitize_frame(frame: [f32; 2], invalid_commands: &mut u64) -> [f32; 2] {
    [
        finite_or_zero(frame[0], invalid_commands),
        finite_or_zero(frame[1], invalid_commands),
    ]
}

fn finite_or_zero(value: f32, invalid_commands: &mut u64) -> f32 {
    if value.is_finite() {
        value
    } else {
        *invalid_commands = invalid_commands.saturating_add(1);
        0.0
    }
}

#[cfg(test)]
mod tests {
    use sampler_core::{DelaySettings, MasterMixSettings, ReverbSettings};

    use super::*;
    use crate::EngineError;

    #[derive(Clone, Copy)]
    enum Bus {
        Delay,
        Reverb,
    }

    fn enabled_delay(time_ms: u16, feedback: f32, return_db: f32) -> MasterMixSettings {
        MasterMixSettings::new(
            0.0,
            DelaySettings::new(true, time_ms, feedback, return_db).unwrap(),
            ReverbSettings::default(),
        )
        .unwrap()
    }

    fn enabled_reverb(room_size: f32, damping: f32, return_db: f32) -> MasterMixSettings {
        MasterMixSettings::new(
            0.0,
            DelaySettings::default(),
            ReverbSettings::new(true, room_size, damping, return_db).unwrap(),
        )
        .unwrap()
    }

    fn render_impulse(
        rack: &mut FxRack,
        frame_count: usize,
        impulse: [f32; 2],
        bus: Bus,
    ) -> Vec<[f32; 2]> {
        let mut invalid_commands = 0;
        (0..frame_count)
            .map(|frame| {
                let input = if frame == 0 { impulse } else { [0.0; 2] };
                let (delay_input, reverb_input) = match bus {
                    Bus::Delay => (input, [0.0; 2]),
                    Bus::Reverb => ([0.0; 2], input),
                };
                rack.process([0.0; 2], delay_input, reverb_input, &mut invalid_commands)
            })
            .collect()
    }

    fn render_reverb_fixture(sample_rate: u32) -> Vec<[f32; 2]> {
        let mut rack = FxRack::new(sample_rate, enabled_reverb(0.8, 0.4, 0.0)).unwrap();
        render_impulse(
            &mut rack,
            usize::try_from(sample_rate).unwrap() * 2,
            [1.0, 0.0],
            Bus::Reverb,
        )
    }

    fn peak(frames: &[[f32; 2]]) -> f32 {
        frames
            .iter()
            .flatten()
            .fold(0.0_f32, |peak, sample| peak.max(sample.abs()))
    }

    #[test]
    fn linear_ramp_reaches_the_target_in_exactly_64_frames() {
        let mut ramp = LinearRamp::new(0.0);
        ramp.set_target(1.0);

        for frame in 1..=64 {
            assert_eq!(ramp.next(), frame as f32 / 64.0);
        }
        assert_eq!(ramp.next(), 1.0);
    }

    #[test]
    fn delay_emits_ping_pong_taps_at_the_exact_integer_frame() {
        let mut rack = FxRack::new(1_000, enabled_delay(10, 0.5, 0.0)).unwrap();
        let frames = render_impulse(&mut rack, 31, [1.0, 0.0], Bus::Delay);
        assert_eq!(frames[10], [1.0, 0.0]);
        assert_eq!(frames[20], [0.0, 0.5]);
        assert_eq!(frames[30], [0.25, 0.0]);
    }

    #[test]
    fn delay_time_changes_crossfade_taps_for_128_frames() {
        let mut rack = FxRack::new(1_000, enabled_delay(10, 0.0, 0.0)).unwrap();
        let mut invalid_commands = 0;
        assert_eq!(
            rack.process([0.0; 2], [1.0, 0.0], [0.0; 2], &mut invalid_commands,),
            [0.0; 2]
        );
        rack.set_settings(enabled_delay(20, 0.0, 0.0));

        let frames: Vec<_> = (1..=128)
            .map(|_| rack.process([0.0; 2], [0.0; 2], [0.0; 2], &mut invalid_commands))
            .collect();
        assert_eq!(frames[9], [0.921_875, 0.0]);
        assert_eq!(frames[19], [0.156_25, 0.0]);
        assert!(frames[20..].iter().all(|frame| *frame == [0.0; 2]));
    }

    #[test]
    fn reverb_is_deterministic_decorrelated_finite_and_decays() {
        let left = render_reverb_fixture(48_000);
        let right = render_reverb_fixture(48_000);
        assert_eq!(left, right);
        assert!(left.iter().flatten().all(|sample| sample.is_finite()));
        assert!(
            left.iter()
                .any(|frame| frame[0].to_bits() != frame[1].to_bits())
        );
        assert!(peak(&left[left.len() - 4_800..]) < peak(&left[..24_000]));
    }

    #[test]
    fn disabled_effects_advance_and_decay_their_existing_tails() {
        let mut delay = FxRack::new(1_000, enabled_delay(10, 0.5, 0.0)).unwrap();
        let _ = render_impulse(&mut delay, 11, [1.0, 0.0], Bus::Delay);
        let disabled_delay = MasterMixSettings::new(
            0.0,
            DelaySettings::new(false, 10, 0.5, 0.0).unwrap(),
            ReverbSettings::default(),
        )
        .unwrap();
        delay.set_settings(disabled_delay);
        let _ = render_impulse(&mut delay, 500, [0.0; 2], Bus::Delay);
        delay.set_settings(enabled_delay(10, 0.5, 0.0));
        let resumed_delay = render_impulse(&mut delay, 128, [0.0; 2], Bus::Delay);
        assert!(peak(&resumed_delay) < 0.000_001);

        let mut reverb = FxRack::new(48_000, enabled_reverb(0.8, 0.4, 0.0)).unwrap();
        let mut unadvanced_reference = FxRack::new(48_000, enabled_reverb(0.8, 0.4, 0.0)).unwrap();
        let initial = render_impulse(&mut reverb, 4_000, [1.0, 0.0], Bus::Reverb);
        let reference_initial =
            render_impulse(&mut unadvanced_reference, 4_000, [1.0, 0.0], Bus::Reverb);
        assert_eq!(initial, reference_initial);
        let disabled_reverb = MasterMixSettings::new(
            0.0,
            DelaySettings::default(),
            ReverbSettings::new(false, 0.8, 0.4, 0.0).unwrap(),
        )
        .unwrap();
        reverb.set_settings(disabled_reverb);
        let _ = render_impulse(&mut reverb, 96_000, [0.0; 2], Bus::Reverb);
        reverb.set_settings(enabled_reverb(0.8, 0.4, 0.0));
        let resumed_reverb = render_impulse(&mut reverb, 4_000, [0.0; 2], Bus::Reverb);
        let unadvanced_reverb =
            render_impulse(&mut unadvanced_reference, 4_000, [0.0; 2], Bus::Reverb);
        assert!(peak(&resumed_reverb) < peak(&unadvanced_reverb));
    }

    #[test]
    fn maximum_feedback_and_room_settings_remain_finite_and_stable() {
        let mut delay = FxRack::new(1_000, enabled_delay(10, 0.95, 0.0)).unwrap();
        let delay_frames = render_impulse(&mut delay, 10_001, [1.0, 0.0], Bus::Delay);
        assert!(
            delay_frames
                .iter()
                .flatten()
                .all(|sample| sample.is_finite())
        );
        assert!(peak(&delay_frames) <= 1.0);

        let mut reverb = FxRack::new(48_000, enabled_reverb(1.0, 1.0, 0.0)).unwrap();
        let reverb_frames = render_impulse(&mut reverb, 96_000, [1.0, 0.0], Bus::Reverb);
        assert!(
            reverb_frames
                .iter()
                .flatten()
                .all(|sample| sample.is_finite())
        );
        assert!(peak(&reverb_frames) <= 2.0);
    }

    #[test]
    fn low_sample_rates_use_nonzero_delay_and_reverb_lines() {
        for sample_rate in [1, 2, 10] {
            let mut rack = FxRack::new(sample_rate, enabled_delay(10, 0.0, 0.0)).unwrap();
            let frames = render_impulse(&mut rack, 3, [1.0, 0.0], Bus::Delay);
            assert_eq!(frames[1], [1.0, 0.0]);

            let mut rack = FxRack::new(sample_rate, enabled_reverb(1.0, 1.0, 0.0)).unwrap();
            let frames = render_impulse(&mut rack, 32, [1.0, 0.0], Bus::Reverb);
            assert!(frames.iter().flatten().all(|sample| sample.is_finite()));
        }
        assert!(matches!(
            FxRack::new(0, MasterMixSettings::default()),
            Err(EngineError::ZeroSampleRate)
        ));
    }

    #[test]
    fn process_sanitizes_every_nonfinite_input_sample() {
        let mut rack = FxRack::new(48_000, MasterMixSettings::default()).unwrap();
        let mut invalid_commands = 0;
        let output = rack.process(
            [f32::NAN, f32::INFINITY],
            [f32::NEG_INFINITY, f32::NAN],
            [f32::INFINITY, f32::NEG_INFINITY],
            &mut invalid_commands,
        );
        assert_eq!(output, [0.0; 2]);
        assert_eq!(invalid_commands, 6);
    }
}
