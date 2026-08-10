use sampler_audio::{AudioEngine, EngineError, audio_channels};
use sampler_core::MasterMixSettings;

#[test]
fn engine_bootstrap_revalidates_persisted_master_settings_before_constructing_fx() {
    let invalid_settings = [
        MasterMixSettings {
            gain_db: f32::NAN,
            ..MasterMixSettings::default()
        },
        MasterMixSettings {
            delay: sampler_core::DelaySettings {
                feedback: f32::INFINITY,
                ..sampler_core::DelaySettings::default()
            },
            ..MasterMixSettings::default()
        },
        MasterMixSettings {
            reverb: sampler_core::ReverbSettings {
                room_size: -0.1,
                ..sampler_core::ReverbSettings::default()
            },
            ..MasterMixSettings::default()
        },
    ];

    for invalid in invalid_settings {
        let (_controller, ports) = audio_channels();
        assert!(matches!(
            AudioEngine::new_with_master_mix(48_000, ports, invalid),
            Err(EngineError::InvalidSettings)
        ));
    }
}
