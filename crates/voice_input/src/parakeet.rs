use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use async_channel::Sender;
use audio::AudioSettings;
use futures::{
    FutureExt as _,
    future::BoxFuture,
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    pin_mut, select_biased,
};
use gpui::{App, AppContext as _, Task, TaskExt as _};
use http_client::{
    AsyncBody, HttpClient, HttpRequestExt as _, Method, Request, StatusCode,
    http::header::{CONTENT_RANGE, RANGE},
};
use parakeet_rs::{ParakeetTDT, Transcriber};
use parking_lot::Mutex;
use settings::{Settings as _, SettingsStore, VoiceInputModel};
use sha2::{Digest, Sha256};

use super::{
    VoiceInputModelDownloadState, VoiceInputProvider, VoiceInputProviderId,
    VoiceTranscriptionBackend, VoiceTranscriptionConfig, VoiceTranscriptionEvent,
    VoiceTranscriptionInput, VoiceTranscriptionSession, voice_transcription_transport,
};

const DOWNLOAD_BUFFER_SIZE: usize = 64 * 1024;
const DOWNLOAD_PROGRESS_STEP: u64 = 1024 * 1024;
const OFFLINE_CHUNK_SECONDS: usize = 20;
const OFFLINE_SPLIT_SEARCH_SECONDS: usize = 3;
const OFFLINE_ENERGY_WINDOW_MILLISECONDS: usize = 100;
const MAX_CAPTURE_SECONDS: usize = 10 * 60;

#[derive(Clone, Copy)]
struct ModelFile {
    path: &'static str,
    name: &'static str,
    size: u64,
    sha256: &'static str,
}

struct ModelDefinition {
    model: VoiceInputModel,
    directory: &'static str,
    repository: &'static str,
    revision: &'static str,
    files: &'static [ModelFile],
}

const TDT_FILES: &[ModelFile] = &[
    ModelFile {
        path: "encoder-model.int8.onnx",
        name: "encoder-model.int8.onnx",
        size: 652_183_999,
        sha256: "6139d2fa7e1b086097b277c7149725edbab89cc7c7ae64b23c741be4055aff09",
    },
    ModelFile {
        path: "decoder_joint-model.int8.onnx",
        name: "decoder_joint-model.int8.onnx",
        size: 18_202_004,
        sha256: "eea7483ee3d1a30375daedc8ed83e3960c91b098812127a0d99d1c8977667a70",
    },
    ModelFile {
        path: "vocab.txt",
        name: "vocab.txt",
        size: 93_939,
        sha256: "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d",
    },
];

const TDT_MODEL: ModelDefinition = ModelDefinition {
    model: VoiceInputModel::ParakeetTdtV3,
    directory: "parakeet-tdt-v3",
    repository: "istupakov/parakeet-tdt-0.6b-v3-onnx",
    revision: "8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce",
    files: TDT_FILES,
};

const PARAKEET_SUPPORTED_MODELS: &[VoiceInputModel] = &[VoiceInputModel::ParakeetTdtV3];

impl ModelDefinition {
    fn for_model(model: VoiceInputModel) -> Option<&'static Self> {
        match model {
            VoiceInputModel::None => None,
            VoiceInputModel::ParakeetTdtV3 => Some(&TDT_MODEL),
        }
    }

    fn total_size(&self) -> u64 {
        self.files.iter().map(|file| file.size).sum()
    }

    fn model_dir(&self, data_dir: &Path) -> PathBuf {
        data_dir.join(self.directory).join(self.revision)
    }

    fn file_url(&self, file: &ModelFile) -> String {
        format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            self.repository, self.revision, file.path
        )
    }
}

#[derive(Clone)]
pub struct ParakeetProvider {
    state: Arc<Mutex<ProviderState>>,
    model_preparation_lock: Arc<async_lock::Mutex<()>>,
    http_client: Arc<dyn HttpClient>,
    data_dir: Arc<PathBuf>,
    model_state_sender: Sender<Option<VoiceInputModelDownloadState>>,
}

#[derive(Default)]
struct ProviderState {
    voice_input_enabled: bool,
    selected_model: Option<VoiceInputModel>,
    downloaded_model: Option<VoiceInputModel>,
    blocked_model: Option<VoiceInputModel>,
    operation: Option<ModelOperation>,
    next_generation: u64,
    loaded_model: Option<LoadedModel>,
    session_active: bool,
}

#[derive(Clone)]
struct ModelOperation {
    model: VoiceInputModel,
    generation: u64,
    cancelled: Arc<AtomicBool>,
    cancellation_sender: async_channel::Sender<()>,
    cancellation_receiver: async_channel::Receiver<()>,
}

impl ModelOperation {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        match self.cancellation_sender.try_send(()) {
            Ok(())
            | Err(async_channel::TrySendError::Closed(()))
            | Err(async_channel::TrySendError::Full(())) => {}
        }
    }
}

#[derive(Clone)]
struct LoadedModel {
    model: VoiceInputModel,
    runtime: Arc<OfflineModel>,
}

struct OfflineModel {
    transcriber: Mutex<ParakeetTDT>,
    is_decoding: AtomicBool,
}

struct OfflineDecodeGuard(Arc<OfflineModel>);

impl Drop for OfflineDecodeGuard {
    fn drop(&mut self) {
        self.0.is_decoding.store(false, Ordering::Release);
    }
}

impl ParakeetProvider {
    pub fn new(
        http_client: Arc<dyn HttpClient>,
        data_dir: PathBuf,
        model_state_sender: Sender<Option<VoiceInputModelDownloadState>>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(ProviderState::default())),
            model_preparation_lock: Arc::new(async_lock::Mutex::new(())),
            http_client,
            data_dir: Arc::new(data_dir),
            model_state_sender,
        }
    }

    fn selected_model(&self, cx: &App) -> Option<VoiceInputModel> {
        voice_audio_settings(cx).and_then(|(_, model)| model)
    }

    fn start_model_operation(&self, model: VoiceInputModel, cx: &mut App) {
        let Some(definition) = ModelDefinition::for_model(model) else {
            return;
        };

        let operation = {
            let mut state = self.state.lock();
            if state.blocked_model == Some(model) {
                return;
            }
            if state
                .loaded_model
                .as_ref()
                .is_some_and(|loaded| loaded.model == model)
                || state
                    .operation
                    .as_ref()
                    .is_some_and(|operation| operation.model == model)
            {
                return;
            }
            if state.downloaded_model == Some(model) && !state.voice_input_enabled {
                drop(state);
                self.send_model_state(Some(VoiceInputModelDownloadState::Downloaded { model }));
                return;
            }

            if let Some(operation) = state.operation.take() {
                operation.cancel();
            }

            state.next_generation = state.next_generation.wrapping_add(1);
            let (cancellation_sender, cancellation_receiver) = async_channel::bounded(1);
            let operation = ModelOperation {
                model,
                generation: state.next_generation,
                cancelled: Arc::new(AtomicBool::new(false)),
                cancellation_sender,
                cancellation_receiver,
            };
            state.selected_model = Some(model);
            state.operation = Some(operation.clone());
            operation
        };

        self.send_operation_state(&operation, VoiceInputModelDownloadState::Loading { model });
        cx.background_spawn({
            let provider = self.clone();
            async move {
                if let Err(error) = provider.prepare_model(definition, &operation).await {
                    provider.fail_operation(&operation, error);
                }
            }
        })
        .detach();
    }

    async fn prepare_model(
        &self,
        definition: &'static ModelDefinition,
        operation: &ModelOperation,
    ) -> Result<()> {
        let _preparation_guard = self.model_preparation_lock.lock().await;
        if operation.cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        let model_dir = definition.model_dir(&self.data_dir);
        if !model_is_ready(definition, &model_dir, &operation.cancelled).await? {
            self.send_operation_state(
                operation,
                VoiceInputModelDownloadState::Downloading {
                    model: operation.model,
                    downloaded_bytes: 0,
                    total_bytes: definition.total_size(),
                },
            );
            download_model(self, definition, &model_dir, operation).await?;
        }
        if operation.cancelled.load(Ordering::Acquire) {
            return Ok(());
        }

        let (session_active, voice_input_enabled) = {
            let mut state = self.state.lock();
            if !self.operation_is_current(&state, operation) {
                return Ok(());
            }
            state.downloaded_model = Some(operation.model);
            (state.session_active, state.voice_input_enabled)
        };

        if !voice_input_enabled {
            self.clear_operation(operation);
            self.send_operation_state(
                operation,
                VoiceInputModelDownloadState::Downloaded {
                    model: operation.model,
                },
            );
            return Ok(());
        }

        if session_active {
            self.clear_operation(operation);
            self.send_operation_state(
                operation,
                VoiceInputModelDownloadState::Loading {
                    model: operation.model,
                },
            );
            return Ok(());
        }

        self.send_operation_state(
            operation,
            VoiceInputModelDownloadState::Loading {
                model: operation.model,
            },
        );
        let model_path = model_dir.clone();
        let runtime = match smol::unblock(move || load_model(definition, &model_path)).await {
            Ok(runtime) => runtime,
            Err(error) => {
                if let Err(invalidation_error) =
                    async_fs::write(model_dir.join(".complete"), "").await
                {
                    log::warn!(
                        "failed to invalidate local voice model marker after load failure: {invalidation_error}"
                    );
                }
                return Err(error)
                    .with_context(|| format!("loading {}", model_label(operation.model)));
            }
        };

        let loaded = LoadedModel {
            model: operation.model,
            runtime,
        };
        let (should_publish, downloaded_only) = {
            let mut state = self.state.lock();
            if self.operation_is_current(&state, operation)
                && !state.session_active
                && state.voice_input_enabled
            {
                state.loaded_model = Some(loaded);
                state.operation = None;
                (true, false)
            } else if self.operation_is_current(&state, operation)
                && !state.session_active
                && !state.voice_input_enabled
            {
                state.operation = None;
                (false, true)
            } else {
                (false, false)
            }
        };

        if should_publish {
            self.send_operation_state(
                operation,
                VoiceInputModelDownloadState::Ready {
                    model: operation.model,
                },
            );
        } else if downloaded_only {
            self.send_operation_state(
                operation,
                VoiceInputModelDownloadState::Downloaded {
                    model: operation.model,
                },
            );
        }
        Ok(())
    }

    fn fail_operation(&self, operation: &ModelOperation, error: anyhow::Error) {
        let should_publish = {
            let mut state = self.state.lock();
            if self.operation_is_current(&state, operation) {
                state.operation = None;
                state.blocked_model = Some(operation.model);
                true
            } else {
                false
            }
        };

        if should_publish {
            log::error!(
                "failed to prepare local voice model {}: {error:#}",
                model_label(operation.model)
            );
            self.send_operation_state(
                operation,
                VoiceInputModelDownloadState::Failed {
                    model: operation.model,
                    message: format!("{error:#}").into(),
                },
            );
        }
    }

    fn clear_operation(&self, operation: &ModelOperation) {
        let mut state = self.state.lock();
        if self.operation_is_current(&state, operation) {
            state.operation = None;
        }
    }

    fn operation_is_current(&self, state: &ProviderState, operation: &ModelOperation) -> bool {
        state.selected_model == Some(operation.model)
            && state
                .operation
                .as_ref()
                .is_some_and(|current| current.generation == operation.generation)
            && !operation.cancelled.load(Ordering::Acquire)
    }

    fn send_model_state(&self, model_state: Option<VoiceInputModelDownloadState>) {
        if let Err(error) = self.model_state_sender.try_send(model_state) {
            log::warn!("voice model status receiver is unavailable: {error}");
        }
    }

    fn send_operation_state(
        &self,
        operation: &ModelOperation,
        model_state: VoiceInputModelDownloadState,
    ) {
        let state = self.state.lock();
        let is_valid_state = match &model_state {
            VoiceInputModelDownloadState::Downloading { .. } => state
                .operation
                .as_ref()
                .is_some_and(|current| current.generation == operation.generation),
            VoiceInputModelDownloadState::Loading { .. } => {
                state
                    .operation
                    .as_ref()
                    .is_some_and(|current| current.generation == operation.generation)
                    || (state.session_active
                        && state.voice_input_enabled
                        && state.downloaded_model == Some(operation.model))
            }
            VoiceInputModelDownloadState::Downloaded { .. } => {
                !state.voice_input_enabled && state.downloaded_model == Some(operation.model)
            }
            VoiceInputModelDownloadState::Failed { .. } => {
                state.blocked_model == Some(operation.model)
            }
            VoiceInputModelDownloadState::Ready { .. } => {
                state.voice_input_enabled
                    && state
                        .loaded_model
                        .as_ref()
                        .is_some_and(|loaded| loaded.model == operation.model)
            }
        };
        if is_valid_state
            && state.selected_model == Some(operation.model)
            && state.next_generation == operation.generation
            && !operation.cancelled.load(Ordering::Acquire)
        {
            self.send_model_state(Some(model_state));
        }
    }
}

impl VoiceInputProvider for ParakeetProvider {
    fn id(&self) -> VoiceInputProviderId {
        VoiceInputProviderId("parakeet".into())
    }

    fn supported_models(&self) -> &[VoiceInputModel] {
        PARAKEET_SUPPORTED_MODELS
    }

    fn settings_changed(&self, session_active: bool, cx: &mut App) {
        let Some((voice_input_enabled, selected_model)) = voice_audio_settings(cx) else {
            let mut state = self.state.lock();
            state.session_active = session_active;
            state.voice_input_enabled = false;
            state.selected_model = None;
            state.blocked_model = None;
            state.loaded_model = None;
            if let Some(operation) = state.operation.take() {
                operation.cancel();
            }
            drop(state);
            self.send_model_state(None);
            return;
        };
        let model = selected_model.filter(|model| *model != VoiceInputModel::None);
        let (should_prepare, downloaded_after_disable) = {
            let mut state = self.state.lock();
            state.session_active = session_active;
            let selection_changed = state.selected_model != selected_model;
            let was_enabled = state.voice_input_enabled;
            state.voice_input_enabled = voice_input_enabled;

            if selection_changed {
                if let Some(operation) = state.operation.take() {
                    operation.cancel();
                }
                state.loaded_model = None;
                state.selected_model = selected_model;
                state.blocked_model = None;
            }
            if was_enabled && !voice_input_enabled {
                state.loaded_model = None;
            }

            let should_prepare = model.is_some_and(|model| {
                state.blocked_model != Some(model)
                    && state.operation.is_none()
                    && (selection_changed
                        || (!session_active
                            && voice_input_enabled
                            && !state
                                .loaded_model
                                .as_ref()
                                .is_some_and(|loaded| loaded.model == model)))
            });
            let downloaded_after_disable =
                if was_enabled && !voice_input_enabled && state.operation.is_none() {
                    state.downloaded_model.filter(|downloaded_model| {
                        selected_model == Some(*downloaded_model)
                            && state.blocked_model != Some(*downloaded_model)
                    })
                } else {
                    None
                };
            (should_prepare, downloaded_after_disable)
        };

        if let Some(model) = downloaded_after_disable {
            self.send_model_state(Some(VoiceInputModelDownloadState::Downloaded { model }));
        }
        match model {
            Some(model) if should_prepare => self.start_model_operation(model, cx),
            Some(_) => {}
            None => self.send_model_state(None),
        }
    }

    fn cancel_model_download(&self, _cx: &mut App) {
        let canceled_model = {
            let mut state = self.state.lock();
            let operation = state.operation.take();
            if let Some(operation) = &operation {
                operation.cancel();
                state.blocked_model = Some(operation.model);
                state.next_generation = state.next_generation.wrapping_add(1);
            }
            operation.map(|operation| operation.model)
        };

        if let Some(model) = canceled_model {
            self.send_model_state(Some(VoiceInputModelDownloadState::Failed {
                model,
                message: "Download canceled. Select Retry to resume.".into(),
            }));
        }
    }

    fn retry_model_download(&self, session_active: bool, cx: &mut App) {
        let Some((voice_input_enabled, selected_model)) = voice_audio_settings(cx) else {
            self.send_model_state(None);
            return;
        };
        {
            let mut state = self.state.lock();
            state.session_active = session_active;
            state.voice_input_enabled = voice_input_enabled;
            if let Some(operation) = state.operation.take() {
                operation.cancel();
            }
            state.blocked_model = None;
            state.loaded_model = None;
            state.selected_model = selected_model;
        }
        if let Some(model) = selected_model.filter(|model| *model != VoiceInputModel::None) {
            self.start_model_operation(model, cx);
        }
    }

    fn session_started(&self) {
        self.state.lock().session_active = true;
    }

    fn session_ended(&self, cx: &mut App) {
        self.state.lock().session_active = false;
        self.settings_changed(false, cx);
    }

    fn configuration(&self, cx: &App) -> Result<VoiceTranscriptionConfig> {
        let (voice_input_enabled, selected_model) = voice_audio_settings(cx)
            .ok_or_else(|| anyhow!("Voice input settings are not available yet."))?;
        ensure!(
            voice_input_enabled,
            "Enable voice input in Audio Settings before starting a session."
        );
        let model = selected_model
            .filter(|model| *model != VoiceInputModel::None)
            .ok_or_else(|| anyhow!("Choose a local voice input model in Audio Settings."))?;
        let state = self.state.lock();
        ensure!(
            state
                .loaded_model
                .as_ref()
                .is_some_and(|loaded| loaded.model == model),
            "The selected voice input model is not ready."
        );
        ModelDefinition::for_model(model)
            .ok_or_else(|| anyhow!("The selected voice input model is unsupported."))?;
        Ok(VoiceTranscriptionConfig::default())
    }

    fn is_available(&self, cx: &App) -> bool {
        let Some((voice_input_enabled, selected_model)) = voice_audio_settings(cx) else {
            return false;
        };
        let Some(model) = selected_model.filter(|model| *model != VoiceInputModel::None) else {
            return false;
        };
        voice_input_enabled
            && self
                .state
                .lock()
                .loaded_model
                .as_ref()
                .is_some_and(|loaded| loaded.model == model)
    }

    fn start(
        &self,
        _config: VoiceTranscriptionConfig,
        cx: &mut App,
    ) -> Task<Result<VoiceTranscriptionSession>> {
        if !voice_audio_settings(cx).is_some_and(|(enabled, _)| enabled) {
            return Task::ready(Err(anyhow!(
                "Enable voice input in Audio Settings before starting a session."
            )));
        }
        let Some(model) = self
            .selected_model(cx)
            .filter(|model| *model != VoiceInputModel::None)
        else {
            return Task::ready(Err(anyhow!(
                "Choose a local voice input model in Audio Settings."
            )));
        };
        let loaded_model = self
            .state
            .lock()
            .loaded_model
            .as_ref()
            .filter(|loaded| loaded.model == model)
            .map(|loaded| loaded.runtime.clone());
        let Some(loaded_model) = loaded_model else {
            return Task::ready(Err(anyhow!("The selected voice input model is not ready.")));
        };
        if loaded_model.is_decoding.load(Ordering::Acquire) {
            return Task::ready(Err(anyhow!(
                "A previous local transcription is still finishing. Try again shortly."
            )));
        }

        let (session, backend) = voice_transcription_transport();
        cx.background_spawn(async move { run_offline_model(loaded_model, backend).await })
            .detach_and_log_err(cx);

        Task::ready(Ok(session))
    }
}

fn voice_audio_settings(cx: &App) -> Option<(bool, Option<VoiceInputModel>)> {
    if !cx.has_global::<SettingsStore>() {
        return None;
    }
    let settings = AudioSettings::get_global(cx);
    Some((settings.voice_input_enabled, settings.voice_input_model))
}

fn model_label(model: VoiceInputModel) -> &'static str {
    match model {
        VoiceInputModel::None => "no model",
        VoiceInputModel::ParakeetTdtV3 => "Parakeet TDT v3",
    }
}

fn load_model(definition: &'static ModelDefinition, model_dir: &Path) -> Result<Arc<OfflineModel>> {
    match definition.model {
        VoiceInputModel::ParakeetTdtV3 => Ok(Arc::new(OfflineModel {
            transcriber: Mutex::new(
                ParakeetTDT::from_pretrained(model_dir, None)
                    .map_err(|error| anyhow!("{error}"))?,
            ),
            is_decoding: AtomicBool::new(false),
        })),
        VoiceInputModel::None => bail!("No voice input model is selected."),
    }
}

async fn run_offline_model(
    model: Arc<OfflineModel>,
    backend: VoiceTranscriptionBackend,
) -> Result<()> {
    let sample_rate = 16_000;
    let maximum_samples = MAX_CAPTURE_SECONDS * sample_rate;
    let mut samples = Vec::new();
    loop {
        match backend.receive().await {
            VoiceTranscriptionInput::Audio(audio) => {
                let new_samples = audio.samples();
                if new_samples.len() > maximum_samples.saturating_sub(samples.len()) {
                    backend
                        .send_event(VoiceTranscriptionEvent::Failed {
                            message: "Voice input recordings are limited to 10 minutes.".into(),
                        })
                        .await?;
                    return Ok(());
                }
                samples.extend(samples_to_float(new_samples));
            }
            VoiceTranscriptionInput::Finish => break,
            VoiceTranscriptionInput::Cancel => return Ok(()),
        }
    }

    if samples.is_empty() {
        backend
            .send_event(VoiceTranscriptionEvent::Finished)
            .await?;
        return Ok(());
    }

    let mut transcript = String::new();
    for chunk_range in offline_audio_chunk_ranges(&samples, sample_rate) {
        let Some(chunk_samples) = samples.get(chunk_range) else {
            backend
                .send_event(VoiceTranscriptionEvent::Failed {
                    message: "Local voice input produced an invalid audio chunk.".into(),
                })
                .await?;
            return Ok(());
        };
        let transcription = transcribe_offline_chunk(model.clone(), chunk_samples.to_vec()).fuse();
        let cancellation = backend.receive_cancel().fuse();
        pin_mut!(transcription, cancellation);
        let transcription = select_biased! {
            _ = cancellation => return Ok(()),
            result = transcription => result,
        };
        let text = match transcription {
            Ok(text) => text,
            Err(message) => {
                backend
                    .send_event(VoiceTranscriptionEvent::Failed {
                        message: message.into(),
                    })
                    .await?;
                return Ok(());
            }
        };
        append_offline_chunk_text(&mut transcript, &text);
    }

    if !transcript.is_empty() {
        backend
            .send_event(VoiceTranscriptionEvent::Final(transcript.into()))
            .await?;
    }

    backend
        .send_event(VoiceTranscriptionEvent::Finished)
        .await?;
    Ok(())
}

fn transcribe_offline_chunk(
    model: Arc<OfflineModel>,
    audio_chunk: Vec<f32>,
) -> BoxFuture<'static, std::result::Result<String, String>> {
    model.is_decoding.store(true, Ordering::Release);
    let decode_guard = OfflineDecodeGuard(model.clone());
    Box::pin(smol::unblock(move || {
        let _decode_guard = decode_guard;
        model
            .transcriber
            .lock()
            .transcribe_samples(audio_chunk, 16_000, 1, None)
            .map(|result| result.text)
            .map_err(|error| error.to_string())
    }))
}

fn append_offline_chunk_text(transcript: &mut String, chunk: &str) {
    let chunk = chunk.trim();
    let Some(first_character) = chunk.chars().next() else {
        return;
    };
    if !transcript.is_empty() && !matches!(first_character, '.' | ',' | '!' | '?' | ';' | ':' | ')')
    {
        transcript.push(' ');
    }
    transcript.push_str(chunk);
}

fn samples_to_float(samples: &[i16]) -> Vec<f32> {
    samples
        .iter()
        .map(|sample| *sample as f32 / 32768.0)
        .collect()
}

fn offline_audio_chunk_ranges(samples: &[f32], sample_rate: usize) -> Vec<std::ops::Range<usize>> {
    let chunk_sample_limit = (OFFLINE_CHUNK_SECONDS * sample_rate).max(1);
    if samples.len() <= chunk_sample_limit {
        return vec![0..samples.len()];
    }

    let mut chunk_ranges = Vec::new();
    let mut offset = 0;
    while samples.len() - offset > chunk_sample_limit {
        let audio_remaining = &samples[offset..];
        let split =
            offline_chunk_split_index(audio_remaining, sample_rate).unwrap_or(chunk_sample_limit);
        chunk_ranges.push(offset..offset + split);
        offset += split;
    }
    if offset < samples.len() {
        chunk_ranges.push(offset..samples.len());
    }
    chunk_ranges
}

fn offline_chunk_split_index(samples: &[f32], sample_rate: usize) -> Option<usize> {
    let chunk_sample_limit = (OFFLINE_CHUNK_SECONDS * sample_rate).max(1);
    if samples.len() <= chunk_sample_limit {
        return None;
    }

    let split_search_samples = OFFLINE_SPLIT_SEARCH_SECONDS * sample_rate;
    let energy_window_samples = (OFFLINE_ENERGY_WINDOW_MILLISECONDS * sample_rate / 1_000).max(1);
    let search_start = chunk_sample_limit.saturating_sub(split_search_samples);
    let mut best_split = chunk_sample_limit;
    let mut best_energy = f64::INFINITY;
    let mut window_start = search_start;

    while window_start + energy_window_samples <= chunk_sample_limit {
        let energy = samples[window_start..window_start + energy_window_samples]
            .iter()
            .map(|sample| f64::from(*sample) * f64::from(*sample))
            .sum::<f64>();
        if energy < best_energy {
            best_energy = energy;
            best_split = window_start + energy_window_samples / 2;
        }
        window_start += (energy_window_samples / 2).max(1);
    }

    Some(best_split.max(1))
}

async fn download_model(
    provider: &ParakeetProvider,
    definition: &'static ModelDefinition,
    model_dir: &Path,
    operation: &ModelOperation,
) -> Result<()> {
    async_fs::create_dir_all(model_dir)
        .await
        .with_context(|| format!("creating model directory {}", model_dir.display()))?;

    let total_bytes = definition.total_size();
    let mut completed_bytes = 0;
    for file in definition.files {
        if operation.cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        let destination = model_dir.join(file.name);
        if file_matches(&destination, file, &operation.cancelled).await? {
            completed_bytes += file.size;
            provider.send_operation_state(
                operation,
                VoiceInputModelDownloadState::Downloading {
                    model: definition.model,
                    downloaded_bytes: completed_bytes,
                    total_bytes,
                },
            );
            continue;
        }

        let file_url = definition.file_url(file);
        let downloaded = download_file(
            provider,
            operation,
            &file_url,
            file,
            &destination,
            completed_bytes,
            total_bytes,
        )
        .await?;
        if !downloaded {
            return Ok(());
        }
        completed_bytes += file.size;
    }

    if operation.cancelled.load(Ordering::Acquire) {
        return Ok(());
    }
    let marker_path = model_dir.join(".complete");
    async_fs::write(&marker_path, definition.revision)
        .await
        .with_context(|| format!("writing model marker {}", marker_path.display()))?;
    Ok(())
}

async fn download_file(
    provider: &ParakeetProvider,
    operation: &ModelOperation,
    url: &str,
    model_file: &ModelFile,
    destination: &Path,
    completed_model_bytes: u64,
    total_model_bytes: u64,
) -> Result<bool> {
    let parent = destination.parent().ok_or_else(|| {
        anyhow!(
            "model file has no parent directory: {}",
            destination.display()
        )
    })?;
    async_fs::create_dir_all(parent)
        .await
        .with_context(|| format!("creating model directory {}", parent.display()))?;

    let partial_path = destination.with_extension(format!(
        "{}part",
        destination
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| format!("{extension}."))
            .unwrap_or_default()
    ));
    let mut partial_bytes = match async_fs::metadata(&partial_path).await {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading partial model file {}", partial_path.display()));
        }
    };
    if partial_bytes > model_file.size {
        async_fs::remove_file(&partial_path)
            .await
            .with_context(|| {
                format!("removing oversized partial file {}", partial_path.display())
            })?;
        partial_bytes = 0;
    }
    if operation.cancelled.load(Ordering::Acquire) {
        return Ok(false);
    }
    if partial_bytes == model_file.size {
        if file_matches(&partial_path, model_file, &operation.cancelled).await? {
            install_model_file(&partial_path, destination).await?;
            provider.send_operation_state(
                operation,
                VoiceInputModelDownloadState::Downloading {
                    model: operation.model,
                    downloaded_bytes: completed_model_bytes + model_file.size,
                    total_bytes: total_model_bytes,
                },
            );
            return Ok(true);
        }
        async_fs::remove_file(&partial_path)
            .await
            .with_context(|| {
                format!("discarding corrupt partial file {}", partial_path.display())
            })?;
        partial_bytes = 0;
    }

    let mut request = Request::builder()
        .method(Method::GET)
        .uri(url)
        .follow_redirects(http_client::RedirectPolicy::FollowAll);
    if partial_bytes > 0 {
        request = request.header(RANGE, format!("bytes={partial_bytes}-"));
    }
    let request = request
        .body(AsyncBody::empty())
        .with_context(|| format!("building model request for {url}"))?;
    let mut response = {
        let sending = provider.http_client.send(request).fuse();
        let cancellation = operation.cancellation_receiver.recv().fuse();
        pin_mut!(sending, cancellation);
        select_biased! {
            _ = cancellation => return Ok(false),
            response = sending => response.with_context(|| format!("downloading model file from {url}"))?,
        }
    };

    if partial_bytes > 0 && response.status() == StatusCode::OK {
        async_fs::remove_file(&partial_path)
            .await
            .with_context(|| format!("restarting partial model file {}", partial_path.display()))?;
        partial_bytes = 0;
    } else if partial_bytes > 0 && response.status() == StatusCode::PARTIAL_CONTENT {
        let content_range = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| anyhow!("server omitted Content-Range for resumed model download"))?;
        if !content_range.starts_with(&format!("bytes {partial_bytes}-")) {
            async_fs::remove_file(&partial_path)
                .await
                .with_context(|| {
                    format!("discarding invalid partial file {}", partial_path.display())
                })?;
            bail!("server resumed model download at an unexpected offset: {content_range}");
        }
    } else if !response.status().is_success() {
        bail!("model download returned HTTP {}", response.status());
    }

    let mut hasher = Sha256::new();
    if partial_bytes > 0 {
        hash_file(&partial_path, &mut hasher, &operation.cancelled).await?;
    }

    let mut file = if partial_bytes > 0 {
        async_fs::OpenOptions::new()
            .append(true)
            .open(&partial_path)
            .await
            .with_context(|| format!("opening partial model file {}", partial_path.display()))?
    } else {
        async_fs::File::create(&partial_path)
            .await
            .with_context(|| format!("creating model file {}", partial_path.display()))?
    };

    let mut buffer = vec![0; DOWNLOAD_BUFFER_SIZE];
    let mut downloaded_bytes = partial_bytes;
    let mut last_reported_bytes = partial_bytes;
    loop {
        if operation.cancelled.load(Ordering::Acquire) {
            file.flush().await.with_context(|| {
                format!("flushing partial model file {}", partial_path.display())
            })?;
            return Ok(false);
        }
        let bytes_read = {
            let reading = response.body_mut().read(&mut buffer).fuse();
            let cancellation = operation.cancellation_receiver.recv().fuse();
            pin_mut!(reading, cancellation);
            select_biased! {
                _ = cancellation => {
                    file.flush()
                        .await
                        .with_context(|| format!("flushing partial model file {}", partial_path.display()))?;
                    return Ok(false);
                },
                bytes_read = reading => {
                    bytes_read.with_context(|| format!("reading model response from {url}"))?
                },
            }
        };
        if bytes_read == 0 {
            break;
        }
        file.write_all(&buffer[..bytes_read])
            .await
            .with_context(|| format!("writing model file {}", partial_path.display()))?;
        hasher.update(&buffer[..bytes_read]);
        downloaded_bytes += bytes_read as u64;
        ensure!(
            downloaded_bytes <= model_file.size,
            "model file {} exceeds its expected size of {} bytes",
            model_file.name,
            model_file.size
        );
        if downloaded_bytes.saturating_sub(last_reported_bytes) >= DOWNLOAD_PROGRESS_STEP {
            provider.send_operation_state(
                operation,
                VoiceInputModelDownloadState::Downloading {
                    model: operation.model,
                    downloaded_bytes: completed_model_bytes + downloaded_bytes,
                    total_bytes: total_model_bytes,
                },
            );
            last_reported_bytes = downloaded_bytes;
        }
    }
    file.flush()
        .await
        .with_context(|| format!("flushing model file {}", partial_path.display()))?;

    ensure!(
        downloaded_bytes == model_file.size,
        "model file {} has size {downloaded_bytes}, expected {}",
        model_file.name,
        model_file.size
    );
    let actual_sha256 = format!("{:x}", hasher.finalize());
    if !actual_sha256.eq_ignore_ascii_case(model_file.sha256) {
        async_fs::remove_file(&partial_path)
            .await
            .with_context(|| format!("discarding corrupt model file {}", partial_path.display()))?;
        bail!("model file {} failed SHA-256 verification", model_file.name);
    }

    if operation.cancelled.load(Ordering::Acquire) {
        return Ok(false);
    }
    install_model_file(&partial_path, destination).await?;

    provider.send_operation_state(
        operation,
        VoiceInputModelDownloadState::Downloading {
            model: operation.model,
            downloaded_bytes: completed_model_bytes + downloaded_bytes,
            total_bytes: total_model_bytes,
        },
    );
    Ok(true)
}

async fn install_model_file(partial_path: &Path, destination: &Path) -> Result<()> {
    match async_fs::metadata(destination).await {
        Ok(_) => {
            async_fs::remove_file(destination)
                .await
                .with_context(|| format!("replacing model file {}", destination.display()))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("checking model file {}", destination.display()));
        }
    }
    async_fs::rename(partial_path, destination)
        .await
        .with_context(|| format!("installing model file {}", destination.display()))?;
    Ok(())
}

async fn model_is_ready(
    definition: &ModelDefinition,
    model_dir: &Path,
    cancelled: &AtomicBool,
) -> Result<bool> {
    let marker_path = model_dir.join(".complete");
    let marker_matches = match async_fs::read_to_string(&marker_path).await {
        Ok(marker) => marker == definition.revision,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading model marker {}", marker_path.display()));
        }
    };
    if marker_matches {
        for file in definition.files {
            ensure!(
                !cancelled.load(Ordering::Acquire),
                "model operation was cancelled"
            );
            let metadata = match async_fs::metadata(model_dir.join(file.name)).await {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error.into()),
            };
            if metadata.len() != file.size {
                return Ok(false);
            }
        }
        return Ok(true);
    }

    for file in definition.files {
        if !file_matches(&model_dir.join(file.name), file, cancelled).await? {
            return Ok(false);
        }
    }
    ensure!(
        !cancelled.load(Ordering::Acquire),
        "model operation was cancelled"
    );
    async_fs::write(&marker_path, definition.revision)
        .await
        .with_context(|| format!("writing model marker {}", marker_path.display()))?;
    Ok(true)
}

async fn file_matches(path: &Path, expected: &ModelFile, cancelled: &AtomicBool) -> Result<bool> {
    let metadata = match async_fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if metadata.len() != expected.size {
        return Ok(false);
    }
    let mut hasher = Sha256::new();
    hash_file(path, &mut hasher, cancelled).await?;
    Ok(format!("{:x}", hasher.finalize()).eq_ignore_ascii_case(expected.sha256))
}

async fn hash_file(path: &Path, hasher: &mut Sha256, cancelled: &AtomicBool) -> Result<()> {
    let mut file = async_fs::File::open(path)
        .await
        .with_context(|| format!("opening file for verification {}", path.display()))?;
    let mut buffer = vec![0; DOWNLOAD_BUFFER_SIZE];
    loop {
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "model operation was cancelled"
        );
        let bytes_read = file
            .read(&mut buffer)
            .await
            .with_context(|| format!("reading file for verification {}", path.display()))?;
        if bytes_read == 0 {
            return Ok(());
        }
        hasher.update(&buffer[..bytes_read]);
    }
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;
    use http_client::FakeHttpClient;

    use super::*;

    #[test]
    fn long_offline_dictation_splits_near_a_pause_without_reordering_audio() {
        let mut samples = vec![0.25; 260];
        samples[170..190].fill(0.0);
        let chunk_ranges = offline_audio_chunk_ranges(&samples, 10);

        assert_eq!(chunk_ranges.len(), 2);
        assert!((170..=190).contains(&chunk_ranges[0].len()));
        assert_eq!(chunk_ranges[0].start, 0);
        assert_eq!(chunk_ranges[1].start, chunk_ranges[0].end);
        assert_eq!(chunk_ranges[1].end, samples.len());
        let reconstructed = chunk_ranges
            .iter()
            .flat_map(|range| samples[range.clone()].iter().copied())
            .collect::<Vec<_>>();
        assert_eq!(reconstructed, samples);
    }

    #[test]
    fn microphone_samples_are_normalized_for_parakeet() {
        let samples = samples_to_float(&[i16::MIN, 0, i16::MAX]);

        assert_eq!(samples, vec![-1.0, 0.0, i16::MAX as f32 / 32768.0]);
    }

    #[test]
    fn offline_chunk_transcripts_are_joined_at_word_boundaries() {
        let mut transcript = String::new();
        append_offline_chunk_text(&mut transcript, "Hello");
        append_offline_chunk_text(&mut transcript, " world ");
        append_offline_chunk_text(&mut transcript, ", next");

        assert_eq!(transcript, "Hello world, next");
    }

    fn test_model_file() -> ModelFile {
        ModelFile {
            path: "model.onnx",
            name: "model.onnx",
            size: 3,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        }
    }

    static READY_MODEL_FILES: &[ModelFile] = &[ModelFile {
        path: "model.onnx",
        name: "model.onnx",
        size: 3,
        sha256: "",
    }];
    static READY_MODEL_DEFINITION: ModelDefinition = ModelDefinition {
        model: VoiceInputModel::ParakeetTdtV3,
        directory: "test-model",
        repository: "test",
        revision: "revision",
        files: READY_MODEL_FILES,
    };

    fn test_model_operation(model: VoiceInputModel, generation: u64) -> ModelOperation {
        let (cancellation_sender, cancellation_receiver) = async_channel::bounded(1);
        ModelOperation {
            model,
            generation,
            cancelled: Arc::new(AtomicBool::new(false)),
            cancellation_sender,
            cancellation_receiver,
        }
    }

    fn test_provider(
        client: Arc<dyn HttpClient>,
        data_directory: &Path,
    ) -> (ParakeetProvider, ModelOperation) {
        let (status_sender, _status_receiver) = async_channel::unbounded();
        let provider = ParakeetProvider::new(client, data_directory.to_path_buf(), status_sender);
        let operation = test_model_operation(VoiceInputModel::ParakeetTdtV3, 1);
        {
            let mut state = provider.state.lock();
            state.selected_model = Some(operation.model);
            state.next_generation = operation.generation;
            state.operation = Some(operation.clone());
        }
        (provider, operation)
    }

    #[test]
    fn model_download_finishing_while_disabled_publishes_downloaded() -> Result<()> {
        let client = FakeHttpClient::create(|_| async move {
            bail!("a complete model must not trigger a download")
        });
        let temporary_directory = tempfile::tempdir()?;
        let model_directory = temporary_directory.path().join("test-model/revision");
        std::fs::create_dir_all(&model_directory)?;
        std::fs::write(model_directory.join(".complete"), "revision")?;
        std::fs::write(model_directory.join("model.onnx"), "abc")?;
        let (status_sender, status_receiver) = async_channel::unbounded();
        let provider = ParakeetProvider::new(
            client,
            temporary_directory.path().to_path_buf(),
            status_sender,
        );
        let operation = test_model_operation(VoiceInputModel::ParakeetTdtV3, 1);
        {
            let mut state = provider.state.lock();
            state.voice_input_enabled = false;
            state.selected_model = Some(operation.model);
            state.session_active = true;
            state.next_generation = operation.generation;
            state.operation = Some(operation.clone());
        }
        block_on(provider.prepare_model(&READY_MODEL_DEFINITION, &operation))?;

        assert_eq!(
            status_receiver.try_recv()?,
            Some(VoiceInputModelDownloadState::Downloaded {
                model: operation.model,
            })
        );
        Ok(())
    }

    #[test]
    fn superseded_model_operations_cannot_publish_status() -> Result<()> {
        let client = FakeHttpClient::create(|_| async move {
            bail!("status publication should not make a request")
        });
        let temporary_directory = tempfile::tempdir()?;
        let (status_sender, status_receiver) = async_channel::unbounded();
        let provider = ParakeetProvider::new(
            client,
            temporary_directory.path().to_path_buf(),
            status_sender,
        );
        let stale_operation = test_model_operation(VoiceInputModel::ParakeetTdtV3, 1);
        let current_operation = test_model_operation(VoiceInputModel::ParakeetTdtV3, 2);
        {
            let mut state = provider.state.lock();
            state.selected_model = Some(current_operation.model);
            state.next_generation = current_operation.generation;
            state.operation = Some(current_operation);
        }

        provider.send_operation_state(
            &stale_operation,
            VoiceInputModelDownloadState::Downloading {
                model: VoiceInputModel::ParakeetTdtV3,
                downloaded_bytes: 1,
                total_bytes: 3,
            },
        );

        assert!(matches!(
            status_receiver.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
        Ok(())
    }

    #[test]
    fn model_download_resumes_a_partial_file_and_checks_its_hash() -> Result<()> {
        let client = FakeHttpClient::create(|request| async move {
            ensure!(
                request
                    .headers()
                    .get(RANGE)
                    .and_then(|value| value.to_str().ok())
                    == Some("bytes=1-"),
                "resumed requests must ask for the remaining bytes"
            );
            let response = http_client::Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(CONTENT_RANGE, "bytes 1-2/3")
                .body(AsyncBody::from("bc"))
                .map_err(anyhow::Error::from)?;
            Ok(response)
        });
        let temporary_directory = tempfile::tempdir()?;
        let destination = temporary_directory.path().join("model.onnx");
        let partial_path = destination.with_extension("onnx.part");
        block_on(async_fs::write(&partial_path, b"a"))?;
        let model_file = test_model_file();
        let (provider, operation) = test_provider(client, temporary_directory.path());

        let downloaded = block_on(download_file(
            &provider,
            &operation,
            "https://models.example/model.onnx",
            &model_file,
            &destination,
            0,
            3,
        ))?;

        assert!(downloaded);
        assert_eq!(block_on(async_fs::read(destination))?, b"abc");
        Ok(())
    }

    #[test]
    fn completed_partial_model_file_is_promoted_without_a_range_request() -> Result<()> {
        let client = FakeHttpClient::create(|_| async move {
            bail!("a complete partial file should not trigger a request")
        });
        let temporary_directory = tempfile::tempdir()?;
        let destination = temporary_directory.path().join("model.onnx");
        let partial_path = destination.with_extension("onnx.part");
        block_on(async_fs::write(&partial_path, b"abc"))?;
        let (provider, operation) = test_provider(client, temporary_directory.path());

        let downloaded = block_on(download_file(
            &provider,
            &operation,
            "https://models.example/model.onnx",
            &test_model_file(),
            &destination,
            0,
            3,
        ))?;

        assert!(downloaded);
        assert_eq!(block_on(async_fs::read(destination))?, b"abc");
        assert!(!partial_path.exists());
        Ok(())
    }
}
