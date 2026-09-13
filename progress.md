# Voice Input Progress

## Status

V1 uses one local, offline Parakeet TDT v3 model. Voice input defaults off and
has no selected model on a fresh install. Selecting the model downloads it.
Intel macOS does not expose the voice settings or provider.

## Interaction

- The existing mic icon uses the Accent color while recording and Success while
  transcribing.
- The composer and attachment controls stay disabled from recording start until
  finalized text is inserted.
- Stopping the recording begins local transcription. It does not send the message.
- Sending while recording or transcribing waits for final text. Escape cancels
  the session and discards its transcript.
- The transcript is inserted at the editor anchor and persisted as a draft.

## Implementation

The microphone capture uses Zed's existing audio-device settings and conversion
helpers. Audio is buffered while recording, then split into bounded chunks near
quiet pauses and transcribed sequentially. Recordings are capped at ten minutes.
Model downloads are revision-pinned, resumable, and SHA-256 verified.

## Validation

Focused crate checks, tests, Clippy, Rust formatting, and `git diff --check` pass.
Model weights were not downloaded; real transcription accuracy, microphone
permissions, and runtime latency still require model-backed hardware tests.
