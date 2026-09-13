use std::str::FromStr;

use cpal::DeviceId;
use settings::{RegisterSetting, Settings, VoiceInputModel};

#[derive(Clone, Debug, RegisterSetting)]
pub struct AudioSettings {
    /// Select specific output audio device.
    pub output_audio_device: Option<DeviceId>,
    /// Select specific input audio device.
    pub input_audio_device: Option<DeviceId>,
    /// Whether voice input is enabled in the Agent Panel.
    pub voice_input_enabled: bool,
    /// Local speech model to use for voice input.
    pub voice_input_model: Option<VoiceInputModel>,
}

/// Configuration of audio in Zed
impl Settings for AudioSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let audio = &content.audio.as_ref().unwrap();
        AudioSettings {
            output_audio_device: audio
                .output_audio_device
                .as_ref()
                .and_then(|x| x.0.as_ref().and_then(|id| DeviceId::from_str(&id).ok())),
            input_audio_device: audio
                .input_audio_device
                .as_ref()
                .and_then(|x| x.0.as_ref().and_then(|id| DeviceId::from_str(&id).ok())),
            voice_input_enabled: audio.voice_input_enabled.unwrap_or_default(),
            voice_input_model: audio
                .voice_input_model
                .filter(|model| *model != VoiceInputModel::None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_input_is_disabled_by_default() {
        let content = settings::SettingsContent {
            audio: Some(settings::AudioSettingsContent::default()),
            ..settings::SettingsContent::default()
        };
        let settings = AudioSettings::from_settings(&content);

        assert!(!settings.voice_input_enabled);
        assert!(settings.voice_input_model.is_none());
    }
}
