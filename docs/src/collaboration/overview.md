---
title: Collaboration
description: "Real-time collaboration in Zed: share projects, edit code together, and communicate with voice chat."
---

# Collaboration {#collaboration}

Zed supports real-time multiplayer editing. Multiple people can work in the same project simultaneously, seeing each other's cursors and edits as they happen.

Open the Collaboration Panel with {#kb collab_panel::ToggleFocus}. You'll need to [sign in](../authentication.md#signing-in) to access collaboration features.

## Collaboration Panel {#collaboration-panel}

The Collaboration Panel has two sections:

1. [Channels](./channels.md): Persistent project rooms for team collaboration, with shared projects and voice chat.
2. [Contacts and Private Calls](./contacts-and-private-calls.md): Your contacts list for ad-hoc private sessions.

> **Warning:** Sharing a project gives collaborators access to your local file system within that project. Only collaborate with people you trust.

See the [Data and Privacy FAQs](https://zed.dev/faq#data-and-privacy) for more details.

## Audio Settings {#audio-settings}

### Selecting Audio Devices

You can select specific input and output audio devices instead of using system defaults. To configure audio devices:

1. Open {#kb zed::OpenSettings}
2. Navigate to **Collaboration** > **Experimental**
3. Use the **Output Audio Device** and **Input Audio Device** dropdowns to select your preferred devices

Changes take effect immediately. If you select a device that becomes unavailable, Zed falls back to system defaults.

To test your audio configuration, click **Test Audio** in the same section. This opens a window where you can verify your microphone and speaker work correctly with the selected devices.

**JSON configuration:**

```json [settings]
{
  "audio": {
    "experimental.output_audio_device": "Device Name (device-id)",
    "experimental.input_audio_device": "Device Name (device-id)"
  }
}
```

Set either value to `null` to use system defaults.

### Voice Input {#voice-input}

Voice input adds microphone dictation to the Agent Panel. It is disabled by
default, and no model is selected or downloaded on a fresh install.

To enable it:

1. Open {#kb zed::OpenSettings} and select **AI**.
2. Turn on **Voice Input**.
3. Select a **Voice Input Model**. Zed downloads the selected model and shows
   its progress in Settings. The microphone stays disabled until the model is
   ready.

Parakeet TDT v3 runs locally. Zed downloads its weights only after you select
the model. Voice input is unavailable on Intel macOS.

The model weights have a separate license from the `parakeet-rs` code:

- [Parakeet TDT v3 ONNX weights](https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx)
  are converted from [NVIDIA's model](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3)
  and use CC BY 4.0.

Recorded audio is not sent to a transcription service.

The Agent Panel mic icon uses the Accent theme color while listening and the
Success theme color while transcribing. Zed locks the editor and attachment
controls while recording. After you stop, it transcribes the recording and
inserts the finalized text before unlocking the composer. Sending while
recording waits for transcription to finish before sending the message.
Recordings are limited to ten minutes.

**JSON configuration:**

```json [settings]
{
  "audio": {
    "voice_input_enabled": true,
    "voice_input_model": "parakeet_tdt_v3"
  }
}
```
