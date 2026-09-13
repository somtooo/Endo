use std::{
    num::{NonZeroU16, NonZeroU32},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    thread,
    time::Duration,
};

#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
mod parakeet;

use anyhow::{Context as _, Result, anyhow};
use async_channel::{Receiver, Sender, TrySendError};
use async_trait::async_trait;
use audio::{AudioSettings, RodioExt as _};
use collections::HashMap;
use futures::{FutureExt as _, pin_mut, select_biased};
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, Global, SharedString, Task};
#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
use parakeet::ParakeetProvider;
#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
use paths::data_dir;
use rodio::{
    Source,
    cpal::{
        self, FromSample, I24, Sample as _, SizedSample, U24,
        traits::{DeviceTrait as _, StreamTrait as _},
    },
};
use settings::{Settings as _, SettingsStore, VoiceInputModel};

pub const TRANSCRIPTION_CHANNEL_COUNT: u16 = 1;
pub const TRANSCRIPTION_LANGUAGE: &str = "en";
pub const TRANSCRIPTION_SAMPLE_RATE: u32 = 16_000;

const AUDIO_CHANNEL_CAPACITY: usize = 10;
const RAW_AUDIO_BUFFER_CAPACITY: usize = 10;
const EVENT_CHANNEL_CAPACITY: usize = 32;
const CAPTURE_STATUS_RUNNING: u8 = 0;
const CAPTURE_STATUS_STOPPING: u8 = 1;
const CAPTURE_STATUS_STOPPED: u8 = 2;
const CAPTURE_STATUS_ENDED: u8 = 3;
const CAPTURE_STATUS_OVERFLOWED: u8 = 4;
const CAPTURE_FLUSH_TIMEOUT: Duration = Duration::from_millis(250);

fn transition_capture_status(status: &AtomicU8, current: u8, next: u8) {
    match status.compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) | Err(_) => {}
    }
}

pub fn init(cx: &mut App) {
    if cx.has_global::<GlobalVoiceInputRegistry>() {
        return;
    }

    let registry = cx.new(|_| VoiceInputRegistry::default());
    cx.set_global(GlobalVoiceInputRegistry(registry.clone()));
    #[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
    {
        let (model_state_sender, model_state_receiver) = async_channel::unbounded();
        let provider = ParakeetProvider::new(
            cx.http_client(),
            data_dir().join("voice_input"),
            model_state_sender,
        );
        registry.update(cx, |registry, cx| registry.register_provider(provider, cx));

        let weak_registry = registry.downgrade();
        cx.spawn(async move |cx| {
            while let Ok(model_state) = model_state_receiver.recv().await {
                let Some(registry) = weak_registry.upgrade() else {
                    break;
                };
                registry.update(cx, |registry, cx| {
                    registry.set_model_download_state(model_state, cx);
                });
            }
        })
        .detach();
    }

    let weak_registry = registry.downgrade();
    cx.observe_global::<SettingsStore>(move |cx| {
        if let Some(registry) = weak_registry.upgrade() {
            registry.update(cx, |registry, cx| registry.update_provider_settings(cx));
        }
    })
    .detach();
    registry.update(cx, |registry, cx| registry.update_provider_settings(cx));
}

pub fn is_enabled(cx: &App) -> bool {
    cx.has_global::<SettingsStore>() && AudioSettings::get_global(cx).voice_input_enabled
}

pub const fn is_supported_on_this_platform() -> bool {
    !cfg!(all(target_os = "macos", target_arch = "x86_64"))
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct VoiceInputProviderId(pub SharedString);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoiceTranscriptionConfig {
    pub channel_count: u16,
    pub language: SharedString,
    pub sample_rate: u32,
}

impl Default for VoiceTranscriptionConfig {
    fn default() -> Self {
        Self {
            channel_count: TRANSCRIPTION_CHANNEL_COUNT,
            language: TRANSCRIPTION_LANGUAGE.into(),
            sample_rate: TRANSCRIPTION_SAMPLE_RATE,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioChunk {
    samples: Arc<[i16]>,
}

impl AudioChunk {
    pub fn new(samples: impl Into<Arc<[i16]>>) -> Self {
        Self {
            samples: samples.into(),
        }
    }

    pub fn samples(&self) -> &[i16] {
        &self.samples
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

struct InterruptibleInputSource {
    _stream: cpal::Stream,
    buffer_receiver: Receiver<Vec<f32>>,
    current_buffer: std::vec::IntoIter<f32>,
    channel_count: NonZeroU16,
    sample_rate: NonZeroU32,
    stop_requested: Arc<AtomicBool>,
    status: Arc<AtomicU8>,
}

impl Iterator for InterruptibleInputSource {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(sample) = self.current_buffer.next() {
                return Some(sample);
            }
            match self.buffer_receiver.try_recv() {
                Ok(buffer) => self.current_buffer = buffer.into_iter(),
                Err(async_channel::TryRecvError::Closed) => return None,
                Err(async_channel::TryRecvError::Empty) => {
                    if self.stop_requested.load(Ordering::Acquire)
                        || matches!(
                            self.status.load(Ordering::Acquire),
                            CAPTURE_STATUS_ENDED | CAPTURE_STATUS_OVERFLOWED
                        )
                    {
                        return None;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
}

impl Source for InterruptibleInputSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> rodio::ChannelCount {
        self.channel_count
    }

    fn sample_rate(&self) -> rodio::SampleRate {
        self.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

fn open_interruptible_input_source(
    stop_requested: Arc<AtomicBool>,
    status: Arc<AtomicU8>,
    cx: &App,
) -> Result<InterruptibleInputSource> {
    let input_device = AudioSettings::get_global(cx).input_audio_device.as_ref();
    let device = audio::resolve_device(input_device, true)?;
    let supported_config = device
        .default_input_config()
        .context("failed to read the microphone input configuration")?;
    let sample_format = supported_config.sample_format();
    let stream_config = supported_config.config();
    let channel_count = NonZeroU16::new(stream_config.channels)
        .context("microphone input channel count is zero")?;
    let sample_rate = NonZeroU32::new(stream_config.sample_rate)
        .context("microphone input sample rate is zero")?;
    let (buffer_sender, buffer_receiver) = async_channel::bounded(RAW_AUDIO_BUFFER_CAPACITY);

    macro_rules! build_stream {
        ($sample:ty) => {
            build_input_stream::<$sample>(
                &device,
                &stream_config,
                buffer_sender,
                stop_requested.clone(),
                status.clone(),
            )?
        };
    }
    let stream = match sample_format {
        cpal::SampleFormat::I8 => build_stream!(i8),
        cpal::SampleFormat::I16 => build_stream!(i16),
        cpal::SampleFormat::I24 => build_stream!(I24),
        cpal::SampleFormat::I32 => build_stream!(i32),
        cpal::SampleFormat::I64 => build_stream!(i64),
        cpal::SampleFormat::U8 => build_stream!(u8),
        cpal::SampleFormat::U16 => build_stream!(u16),
        cpal::SampleFormat::U24 => build_stream!(U24),
        cpal::SampleFormat::U32 => build_stream!(u32),
        cpal::SampleFormat::U64 => build_stream!(u64),
        cpal::SampleFormat::F32 => build_stream!(f32),
        cpal::SampleFormat::F64 => build_stream!(f64),
        _ => return Err(anyhow!("unsupported microphone sample format")),
    };
    stream
        .play()
        .context("failed to start the microphone input stream")?;

    Ok(InterruptibleInputSource {
        _stream: stream,
        buffer_receiver,
        current_buffer: Vec::new().into_iter(),
        channel_count,
        sample_rate,
        stop_requested,
        status,
    })
}

fn build_input_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    buffer_sender: Sender<Vec<f32>>,
    stop_requested: Arc<AtomicBool>,
    status: Arc<AtomicU8>,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let error_status = status.clone();
    let error_stop_requested = stop_requested.clone();
    device
        .build_input_stream(
            *config,
            move |samples: &[T], _| {
                if stop_requested.load(Ordering::Acquire) {
                    return;
                }
                let samples = samples
                    .iter()
                    .map(|sample| sample.to_sample::<f32>())
                    .collect();
                match buffer_sender.try_send(samples) {
                    Ok(()) | Err(TrySendError::Closed(_)) => {}
                    Err(TrySendError::Full(_)) => {
                        status.store(CAPTURE_STATUS_OVERFLOWED, Ordering::Release);
                        stop_requested.store(true, Ordering::Release);
                    }
                }
            },
            move |_error| {
                error_status.store(CAPTURE_STATUS_ENDED, Ordering::Release);
                error_stop_requested.store(true, Ordering::Release);
            },
            None,
        )
        .context("failed to open the microphone input stream")
}

struct MicrophoneCapture {
    chunk_receiver: Receiver<AudioChunk>,
    stopped: Arc<AtomicBool>,
    status: Arc<AtomicU8>,
    worker: Option<thread::JoinHandle<()>>,
}

impl MicrophoneCapture {
    fn start(config: &VoiceTranscriptionConfig, cx: &App) -> Result<Self> {
        let channel_count =
            NonZeroU16::new(config.channel_count).context("voice input channel count is zero")?;
        let sample_rate =
            NonZeroU32::new(config.sample_rate).context("voice input sample rate is zero")?;
        let chunk_sample_count = config.sample_rate as usize * config.channel_count as usize / 10;
        if chunk_sample_count == 0 {
            return Err(anyhow!("voice input audio chunk size is zero"));
        }

        let stopped = Arc::new(AtomicBool::new(false));
        let status = Arc::new(AtomicU8::new(CAPTURE_STATUS_RUNNING));
        let microphone = open_interruptible_input_source(stopped.clone(), status.clone(), cx)?;
        let mut microphone = microphone
            .possibly_disconnected_channels_to_mono()
            .constant_samplerate(sample_rate)
            .constant_params(channel_count, sample_rate);
        let (chunk_sender, chunk_receiver) = async_channel::bounded(AUDIO_CHANNEL_CAPACITY);

        let worker = thread::Builder::new()
            .name("VoiceInputCapture".to_string())
            .spawn({
                let status = status.clone();
                move || {
                    let mut samples = Vec::with_capacity(chunk_sample_count);
                    while let Some(sample) = microphone.next() {
                        samples.push(sample.to_sample::<i16>());
                        if samples.len() < chunk_sample_count {
                            continue;
                        }

                        let chunk_samples =
                            std::mem::replace(&mut samples, Vec::with_capacity(chunk_sample_count));
                        match chunk_sender.try_send(AudioChunk::new(chunk_samples)) {
                            Ok(()) => {}
                            Err(TrySendError::Closed(_)) => break,
                            Err(TrySendError::Full(_)) => {
                                status.store(CAPTURE_STATUS_OVERFLOWED, Ordering::Release);
                                break;
                            }
                        }
                    }
                    if !samples.is_empty()
                        && status.load(Ordering::Acquire) != CAPTURE_STATUS_OVERFLOWED
                    {
                        match chunk_sender.try_send(AudioChunk::new(samples)) {
                            Ok(()) | Err(TrySendError::Closed(_)) => {}
                            Err(TrySendError::Full(_)) => {
                                status.store(CAPTURE_STATUS_OVERFLOWED, Ordering::Release);
                            }
                        }
                    }
                    drop(microphone);
                    transition_capture_status(
                        &status,
                        CAPTURE_STATUS_STOPPING,
                        CAPTURE_STATUS_STOPPED,
                    );
                    drop(chunk_sender);
                }
            })?;

        Ok(Self {
            chunk_receiver,
            stopped,
            status,
            worker: Some(worker),
        })
    }
}

#[async_trait]
trait VoiceInputAudioCapture: Send {
    fn stop(&self);
    async fn receive(&self) -> Result<Option<AudioChunk>>;
}

#[async_trait]
impl VoiceInputAudioCapture for MicrophoneCapture {
    fn stop(&self) {
        transition_capture_status(
            &self.status,
            CAPTURE_STATUS_RUNNING,
            CAPTURE_STATUS_STOPPING,
        );
        self.stopped.store(true, Ordering::Release);
    }

    async fn receive(&self) -> Result<Option<AudioChunk>> {
        match self.chunk_receiver.recv().await {
            Ok(chunk) => Ok(Some(chunk)),
            Err(_) => match self.status.load(Ordering::Acquire) {
                CAPTURE_STATUS_STOPPED => Ok(None),
                CAPTURE_STATUS_OVERFLOWED => {
                    Err(anyhow!("microphone audio buffer exceeded capacity"))
                }
                CAPTURE_STATUS_ENDED => Err(anyhow!("microphone input stream ended")),
                status => Err(anyhow!(
                    "microphone capture channel closed in unexpected state {status}"
                )),
            },
        }
    }
}

impl Drop for MicrophoneCapture {
    fn drop(&mut self) {
        self.stop();
        self.chunk_receiver.close();
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            self.status.store(CAPTURE_STATUS_ENDED, Ordering::Release);
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VoiceTranscriptionEvent {
    Final(SharedString),
    Finished,
    Failed { message: SharedString },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VoiceTranscriptionInput {
    Audio(AudioChunk),
    Finish,
    Cancel,
}

pub struct VoiceTranscriptionSession {
    audio_sender: Sender<AudioChunk>,
    cancel_sender: Sender<()>,
    event_receiver: Receiver<VoiceTranscriptionEvent>,
}

impl VoiceTranscriptionSession {
    pub fn try_send_audio(&self, chunk: AudioChunk) -> Result<(), TrySendError<AudioChunk>> {
        self.audio_sender.try_send(chunk)
    }

    pub fn finish(&self) {
        self.audio_sender.close();
    }

    pub fn cancel(&self) -> bool {
        let sent = match self.cancel_sender.try_send(()) {
            Ok(()) => true,
            Err(TrySendError::Closed(_)) | Err(TrySendError::Full(_)) => false,
        };
        self.audio_sender.close();
        sent
    }

    pub async fn receive_event(&self) -> Result<VoiceTranscriptionEvent, async_channel::RecvError> {
        self.event_receiver.recv().await
    }
}

impl Drop for VoiceTranscriptionSession {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub struct VoiceTranscriptionBackend {
    audio_receiver: Receiver<AudioChunk>,
    cancel_receiver: Receiver<()>,
    event_sender: Sender<VoiceTranscriptionEvent>,
}

impl VoiceTranscriptionBackend {
    pub async fn receive(&self) -> VoiceTranscriptionInput {
        let cancellation = self.cancel_receiver.recv().fuse();
        let audio = self.audio_receiver.recv().fuse();
        pin_mut!(cancellation, audio);
        select_biased! {
            _ = cancellation => VoiceTranscriptionInput::Cancel,
            audio = audio => match audio {
                Ok(chunk) => VoiceTranscriptionInput::Audio(chunk),
                Err(_) => VoiceTranscriptionInput::Finish,
            },
        }
    }

    pub async fn receive_cancel(&self) -> Result<(), async_channel::RecvError> {
        self.cancel_receiver.recv().await
    }

    pub async fn send_event(
        &self,
        event: VoiceTranscriptionEvent,
    ) -> Result<(), async_channel::SendError<VoiceTranscriptionEvent>> {
        self.event_sender.send(event).await
    }
}

pub fn voice_transcription_transport() -> (VoiceTranscriptionSession, VoiceTranscriptionBackend) {
    let (audio_sender, audio_receiver) = async_channel::bounded(AUDIO_CHANNEL_CAPACITY);
    let (cancel_sender, cancel_receiver) = async_channel::unbounded();
    let (event_sender, event_receiver) = async_channel::bounded(EVENT_CHANNEL_CAPACITY);

    (
        VoiceTranscriptionSession {
            audio_sender,
            cancel_sender,
            event_receiver,
        },
        VoiceTranscriptionBackend {
            audio_receiver,
            cancel_receiver,
            event_sender,
        },
    )
}

pub trait VoiceInputProvider: Send + Sync {
    fn id(&self) -> VoiceInputProviderId;

    fn supported_models(&self) -> &[VoiceInputModel] {
        &[]
    }

    fn settings_changed(&self, _session_active: bool, _cx: &mut App) {}

    fn cancel_model_download(&self, _cx: &mut App) {}

    fn retry_model_download(&self, session_active: bool, cx: &mut App) {
        self.settings_changed(session_active, cx);
    }

    fn session_started(&self) {}

    fn session_ended(&self, _cx: &mut App) {}

    fn configuration(&self, _cx: &App) -> Result<VoiceTranscriptionConfig> {
        Ok(VoiceTranscriptionConfig::default())
    }

    fn is_available(&self, _cx: &App) -> bool {
        true
    }

    fn start(
        &self,
        config: VoiceTranscriptionConfig,
        cx: &mut App,
    ) -> Task<Result<VoiceTranscriptionSession>>;
}

struct GlobalVoiceInputRegistry(Entity<VoiceInputRegistry>);

impl Global for GlobalVoiceInputRegistry {}

type VoiceInputCaptureFactory = Arc<
    dyn Fn(&VoiceTranscriptionConfig, &App) -> Result<Box<dyn VoiceInputAudioCapture>>
        + Send
        + Sync,
>;

pub struct VoiceInputRegistry {
    providers: HashMap<VoiceInputProviderId, Arc<dyn VoiceInputProvider>>,
    active_provider: Option<Arc<dyn VoiceInputProvider>>,
    active_session: Option<VoiceInputSessionId>,
    active_session_provider: Option<Arc<dyn VoiceInputProvider>>,
    model_download_state: Option<VoiceInputModelDownloadState>,
    next_session_id: u64,
    capture_factory: VoiceInputCaptureFactory,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VoiceInputModelDownloadState {
    Downloading {
        model: VoiceInputModel,
        downloaded_bytes: u64,
        total_bytes: u64,
    },
    Loading {
        model: VoiceInputModel,
    },
    Downloaded {
        model: VoiceInputModel,
    },
    Failed {
        model: VoiceInputModel,
        message: SharedString,
    },
    Ready {
        model: VoiceInputModel,
    },
}

impl Default for VoiceInputRegistry {
    fn default() -> Self {
        Self {
            providers: HashMap::default(),
            active_provider: None,
            active_session: None,
            active_session_provider: None,
            model_download_state: None,
            next_session_id: 0,
            capture_factory: Arc::new(|config, cx| {
                Ok(Box::new(MicrophoneCapture::start(config, cx)?))
            }),
        }
    }
}

impl VoiceInputRegistry {
    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalVoiceInputRegistry>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.has_global::<GlobalVoiceInputRegistry>()
            .then(|| Self::global(cx))
    }

    pub fn read_global(cx: &App) -> &Self {
        cx.global::<GlobalVoiceInputRegistry>().0.read(cx)
    }

    pub fn try_read_global(cx: &App) -> Option<&Self> {
        cx.has_global::<GlobalVoiceInputRegistry>()
            .then(|| Self::read_global(cx))
    }

    pub fn active_provider(&self) -> Option<Arc<dyn VoiceInputProvider>> {
        self.active_provider.clone()
    }

    pub fn model_download_state(&self) -> Option<&VoiceInputModelDownloadState> {
        self.model_download_state.as_ref()
    }

    pub fn has_available_provider(&self, cx: &App) -> bool {
        self.active_provider
            .as_ref()
            .is_some_and(|provider| provider.is_available(cx))
    }

    pub fn active_session(&self) -> Option<VoiceInputSessionId> {
        self.active_session
    }

    fn all_providers(&self) -> Vec<Arc<dyn VoiceInputProvider>> {
        let mut providers = self.providers.values().cloned().collect::<Vec<_>>();
        if let Some(session_provider) = &self.active_session_provider
            && !providers
                .iter()
                .any(|provider| Arc::ptr_eq(provider, session_provider))
        {
            providers.push(session_provider.clone());
        }
        providers
    }

    fn claim_session(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<(
        VoiceInputSessionId,
        Arc<dyn VoiceInputProvider>,
        VoiceInputCaptureFactory,
    )> {
        if self.active_session.is_some() {
            return None;
        }
        let provider = self.active_provider.clone()?;
        self.next_session_id = self.next_session_id.wrapping_add(1);
        let session_id = VoiceInputSessionId(self.next_session_id);
        self.active_session = Some(session_id);
        self.active_session_provider = Some(provider.clone());
        provider.session_started();
        cx.notify();
        Some((session_id, provider, self.capture_factory.clone()))
    }

    fn release_session(&mut self, session_id: VoiceInputSessionId, cx: &mut Context<Self>) -> bool {
        if self.active_session != Some(session_id) {
            return false;
        }
        self.active_session = None;
        if let Some(provider) = self.active_session_provider.take() {
            provider.session_ended(cx);
        }
        cx.notify();
        true
    }

    fn configured_model_provider(&self, cx: &App) -> Option<Arc<dyn VoiceInputProvider>> {
        let model = self.configured_model(cx)?;
        self.providers
            .values()
            .find(|provider| provider.supported_models().contains(&model))
            .cloned()
    }

    fn configured_model(&self, cx: &App) -> Option<VoiceInputModel> {
        cx.has_global::<SettingsStore>()
            .then(|| AudioSettings::get_global(cx).voice_input_model)
            .flatten()
    }

    pub fn register_provider<T: VoiceInputProvider + 'static>(
        &mut self,
        provider: T,
        cx: &mut Context<Self>,
    ) {
        let provider = Arc::new(provider);
        let id = provider.id();
        let duplicate_model = provider.supported_models().iter().find(|model| {
            self.providers.values().any(|registered_provider| {
                registered_provider.id() != id
                    && registered_provider.supported_models().contains(*model)
            })
        });
        if let Some(model) = duplicate_model {
            log::error!(
                "ignoring voice input provider {id:?}: {model:?} is already owned by another provider"
            );
            return;
        }
        self.providers.insert(id, provider.clone());
        if let Some(provider_for_model) = self.configured_model_provider(cx) {
            self.active_provider = Some(provider_for_model);
        } else if self
            .active_provider
            .as_ref()
            .is_none_or(|active_provider| !active_provider.is_available(cx))
        {
            self.active_provider = Some(provider);
        }
        cx.notify();
    }

    pub fn set_model_download_state(
        &mut self,
        state: Option<VoiceInputModelDownloadState>,
        cx: &mut Context<Self>,
    ) {
        if self.model_download_state != state {
            self.model_download_state = state;
            cx.notify();
        }
    }

    pub fn set_active_provider(
        &mut self,
        id: &VoiceInputProviderId,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(provider) = self.providers.get(id) else {
            return false;
        };
        self.active_provider = Some(provider.clone());
        cx.notify();
        true
    }

    pub fn unregister_provider(&mut self, id: &VoiceInputProviderId, cx: &mut Context<Self>) {
        self.providers.remove(id);
        if self
            .active_provider
            .as_ref()
            .is_some_and(|provider| provider.id() == *id)
        {
            self.active_provider = self.providers.values().next().cloned();
        }
        cx.notify();
    }

    pub fn update_provider_settings(&mut self, cx: &mut Context<Self>) {
        for provider in self.all_providers() {
            let session_active = self
                .active_session_provider
                .as_ref()
                .is_some_and(|session_provider| Arc::ptr_eq(session_provider, &provider));
            provider.settings_changed(session_active, cx);
        }

        let configured_provider = self.configured_model_provider(cx);
        let next_provider = configured_provider.or_else(|| {
            self.active_provider
                .as_ref()
                .filter(|provider| provider.is_available(cx))
                .cloned()
                .or_else(|| {
                    self.providers
                        .values()
                        .find(|provider| provider.is_available(cx))
                        .cloned()
                })
        });
        let active_provider_changed = match (self.active_provider.as_ref(), next_provider.as_ref())
        {
            (Some(current), Some(next)) => !Arc::ptr_eq(current, next),
            (None, None) => false,
            _ => true,
        };
        if active_provider_changed {
            self.active_provider = next_provider;
            cx.notify();
        }
    }

    pub fn cancel_model_download(&self, model: VoiceInputModel, cx: &mut App) {
        if self.configured_model(cx) == Some(model)
            && let Some(provider) = self.configured_model_provider(cx)
        {
            provider.cancel_model_download(cx);
        }
    }

    pub fn retry_model_download(&self, model: VoiceInputModel, cx: &mut App) {
        if self.configured_model(cx) == Some(model)
            && let Some(provider) = self.configured_model_provider(cx)
        {
            let session_active = self
                .active_session_provider
                .as_ref()
                .is_some_and(|session_provider| Arc::ptr_eq(session_provider, &provider));
            provider.retry_model_download(session_active, cx);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct VoiceInputSessionId(u64);

#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum VoiceInputState {
    #[default]
    Idle,
    Starting,
    Listening,
    Transcribing,
    Stopping,
}

impl VoiceInputState {
    fn from_u8(value: u8) -> Self {
        match value {
            value if value == Self::Starting as u8 => Self::Starting,
            value if value == Self::Listening as u8 => Self::Listening,
            value if value == Self::Transcribing as u8 => Self::Transcribing,
            value if value == Self::Stopping as u8 => Self::Stopping,
            _ => Self::Idle,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VoiceInputEvent {
    StateChanged(VoiceInputState),
    Final(SharedString),
    Finished,
    Cancelled,
    Failed(SharedString),
}

#[derive(Clone, Copy)]
enum VoiceInputControl {
    Finish,
    Cancel,
}

enum VoiceInputDriverEvent {
    Control(std::result::Result<VoiceInputControl, async_channel::RecvError>),
    Provider(std::result::Result<VoiceTranscriptionEvent, async_channel::RecvError>),
    Audio(Result<Option<AudioChunk>>),
}

pub struct VoiceInputSession {
    id: VoiceInputSessionId,
    state: Arc<AtomicU8>,
    control_sender: Sender<VoiceInputControl>,
    event_receiver: Receiver<VoiceInputEvent>,
    task: Option<Task<()>>,
}

impl VoiceInputSession {
    pub fn start(cx: &mut App) -> Result<Self> {
        let registry = VoiceInputRegistry::global(cx);
        let registry_state = registry.read(cx);
        if registry_state.active_session().is_some() {
            return Err(anyhow!(
                "Voice input is already active in another conversation."
            ));
        }
        let provider = registry_state
            .active_provider()
            .ok_or_else(|| anyhow!("No voice input provider is available."))?;
        let config = provider.configuration(cx)?;

        let claimed_session = registry.update(cx, |registry, cx| registry.claim_session(cx));
        let Some((id, provider, capture_factory)) = claimed_session else {
            return Err(anyhow!(
                "Voice input is already active in another conversation."
            ));
        };

        let (control_sender, control_receiver) = async_channel::unbounded();
        let (event_sender, event_receiver) = async_channel::unbounded();
        let state = Arc::new(AtomicU8::new(VoiceInputState::Starting as u8));
        let task = cx.spawn({
            let state = state.clone();
            async move |cx: &mut AsyncApp| {
                drive_voice_input_session(
                    provider,
                    capture_factory,
                    config.clone(),
                    state.clone(),
                    control_receiver,
                    event_sender,
                    cx,
                )
                .await;
                state.store(VoiceInputState::Idle as u8, Ordering::Release);
                registry.update(cx, |registry, cx| {
                    registry.release_session(id, cx);
                });
            }
        });

        Ok(Self {
            id,
            state,
            control_sender,
            event_receiver,
            task: Some(task),
        })
    }

    pub fn id(&self) -> VoiceInputSessionId {
        self.id
    }

    pub fn state(&self) -> VoiceInputState {
        VoiceInputState::from_u8(self.state.load(Ordering::Acquire))
    }

    pub fn finish(&mut self) -> bool {
        if !matches!(
            self.state(),
            VoiceInputState::Starting | VoiceInputState::Listening
        ) {
            return false;
        }
        self.state
            .store(VoiceInputState::Transcribing as u8, Ordering::Release);
        send_control(&self.control_sender, VoiceInputControl::Finish)
    }

    pub fn cancel(&mut self) -> bool {
        if self.state() == VoiceInputState::Idle {
            return false;
        }
        self.state
            .store(VoiceInputState::Stopping as u8, Ordering::Release);
        send_control(&self.control_sender, VoiceInputControl::Cancel)
    }

    pub fn events(&self) -> Receiver<VoiceInputEvent> {
        self.event_receiver.clone()
    }
}

impl Drop for VoiceInputSession {
    fn drop(&mut self) {
        self.cancel();
        if let Some(task) = self.task.take() {
            task.detach();
        }
    }
}

fn send_control(sender: &Sender<VoiceInputControl>, control: VoiceInputControl) -> bool {
    match sender.try_send(control) {
        Ok(()) => true,
        Err(TrySendError::Closed(_)) | Err(TrySendError::Full(_)) => false,
    }
}

async fn drive_voice_input_session(
    provider: Arc<dyn VoiceInputProvider>,
    capture_factory: VoiceInputCaptureFactory,
    config: VoiceTranscriptionConfig,
    state: Arc<AtomicU8>,
    control_receiver: Receiver<VoiceInputControl>,
    event_sender: Sender<VoiceInputEvent>,
    cx: &mut AsyncApp,
) {
    let provider_task = cx.update(|cx| provider.start(config.clone(), cx)).fuse();
    pin_mut!(provider_task);
    let mut finish_requested = false;
    let provider_result = loop {
        let startup_control = control_receiver.recv().fuse();
        pin_mut!(startup_control);
        select_biased! {
            control = startup_control => match control {
                Ok(VoiceInputControl::Finish) => {
                    finish_requested = true;
                    continue;
                }
                Ok(VoiceInputControl::Cancel) | Err(_) => {
                    send_event(&event_sender, VoiceInputEvent::Cancelled);
                    return;
                }
            },
            result = provider_task => break result,
        }
    };
    let session = match provider_result {
        Ok(session) => session,
        Err(error) => {
            send_event(
                &event_sender,
                VoiceInputEvent::Failed(error.to_string().into()),
            );
            return;
        }
    };

    finish_requested |= state.load(Ordering::Acquire) == VoiceInputState::Transcribing as u8;
    if finish_requested {
        session.finish();
        state.store(VoiceInputState::Transcribing as u8, Ordering::Release);
        send_event(
            &event_sender,
            VoiceInputEvent::StateChanged(VoiceInputState::Transcribing),
        );
        await_provider_finalization(&session, &control_receiver, &event_sender).await;
        return;
    }

    let capture = match cx.update(|cx| capture_factory(&config, cx)) {
        Ok(capture) => capture,
        Err(error) => {
            session.cancel();
            send_event(
                &event_sender,
                VoiceInputEvent::Failed(error.to_string().into()),
            );
            return;
        }
    };
    if state
        .compare_exchange(
            VoiceInputState::Starting as u8,
            VoiceInputState::Listening as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
    {
        send_event(
            &event_sender,
            VoiceInputEvent::StateChanged(VoiceInputState::Listening),
        );
    }

    loop {
        let event = {
            let control = control_receiver.recv().fuse();
            let provider_event = session.receive_event().fuse();
            let audio = capture.receive().fuse();
            pin_mut!(control, provider_event, audio);
            select_biased! {
                control = control => VoiceInputDriverEvent::Control(control),
                provider_event = provider_event => VoiceInputDriverEvent::Provider(provider_event),
                audio = audio => VoiceInputDriverEvent::Audio(audio),
            }
        };
        match event {
            VoiceInputDriverEvent::Control(control) => match control {
                Ok(VoiceInputControl::Finish) => break,
                Ok(VoiceInputControl::Cancel) | Err(_) => {
                    session.cancel();
                    send_event(&event_sender, VoiceInputEvent::Cancelled);
                    drain_capture_until_stopped(capture, cx).await;
                    return;
                }
            },
            VoiceInputDriverEvent::Provider(provider_event) => {
                let message = match provider_event {
                    Ok(VoiceTranscriptionEvent::Failed { message }) => message,
                    Ok(_) => "The offline provider produced a transcript before recording stopped."
                        .into(),
                    Err(error) => {
                        format!("Voice transcription stopped unexpectedly: {error}").into()
                    }
                };
                send_event(&event_sender, VoiceInputEvent::Failed(message));
                session.cancel();
                drain_capture_until_stopped(capture, cx).await;
                return;
            }
            VoiceInputDriverEvent::Audio(audio) => match audio {
                Ok(Some(chunk)) => {
                    if let Err(error) = session.try_send_audio(chunk) {
                        send_event(
                            &event_sender,
                            VoiceInputEvent::Failed(
                                format!(
                                    "Voice transcription could not deliver microphone input: {error}"
                                )
                                .into(),
                            ),
                        );
                        session.cancel();
                        drain_capture_until_stopped(capture, cx).await;
                        return;
                    }
                }
                Ok(None) => {
                    send_event(
                        &event_sender,
                        VoiceInputEvent::Failed("Microphone capture stopped unexpectedly.".into()),
                    );
                    session.cancel();
                    return;
                }
                Err(error) => {
                    send_event(
                        &event_sender,
                        VoiceInputEvent::Failed(error.to_string().into()),
                    );
                    session.cancel();
                    return;
                }
            },
        }
    }

    state.store(VoiceInputState::Transcribing as u8, Ordering::Release);
    send_event(
        &event_sender,
        VoiceInputEvent::StateChanged(VoiceInputState::Transcribing),
    );
    match flush_capture(capture, &session, cx).await {
        Ok(()) => session.finish(),
        Err(message) => {
            send_event(&event_sender, VoiceInputEvent::Failed(message));
            session.cancel();
            return;
        }
    }

    await_provider_finalization(&session, &control_receiver, &event_sender).await;
}

async fn flush_capture(
    capture: Box<dyn VoiceInputAudioCapture>,
    session: &VoiceTranscriptionSession,
    cx: &mut AsyncApp,
) -> std::result::Result<(), SharedString> {
    capture.stop();
    let timeout = cx.background_executor().timer(CAPTURE_FLUSH_TIMEOUT).fuse();
    pin_mut!(timeout);
    loop {
        let received = {
            let audio = capture.receive().fuse();
            pin_mut!(audio);
            select_biased! {
                _ = timeout => None,
                audio = audio => Some(audio),
            }
        };
        match received {
            None => {
                return Err(
                    "Timed out while stopping microphone capture; the message was not sent.".into(),
                );
            }
            Some(Ok(Some(chunk))) => {
                if let Err(error) = session.try_send_audio(chunk) {
                    return Err(format!(
                        "Voice transcription could not flush microphone input: {error}"
                    )
                    .into());
                }
            }
            Some(Ok(None)) => return Ok(()),
            Some(Err(error)) => return Err(error.to_string().into()),
        }
    }
}

async fn drain_capture_until_stopped(capture: Box<dyn VoiceInputAudioCapture>, cx: &mut AsyncApp) {
    capture.stop();
    let timeout = cx.background_executor().timer(CAPTURE_FLUSH_TIMEOUT).fuse();
    pin_mut!(timeout);
    loop {
        let received = {
            let audio = capture.receive().fuse();
            pin_mut!(audio);
            select_biased! {
                _ = timeout => None,
                audio = audio => Some(audio),
            }
        };
        match received {
            Some(Ok(Some(_))) => {}
            None | Some(Ok(None)) | Some(Err(_)) => return,
        }
    }
}

async fn await_provider_finalization(
    session: &VoiceTranscriptionSession,
    control_receiver: &Receiver<VoiceInputControl>,
    event_sender: &Sender<VoiceInputEvent>,
) {
    loop {
        let control = control_receiver.recv().fuse();
        let provider_event = session.receive_event().fuse();
        pin_mut!(control, provider_event);
        select_biased! {
            control = control => match control {
                Ok(VoiceInputControl::Cancel) | Err(_) => {
                    session.cancel();
                    send_event(event_sender, VoiceInputEvent::Cancelled);
                    return;
                }
                Ok(VoiceInputControl::Finish) => {}
            },
            provider_event = provider_event => match provider_event {
                Ok(event) => {
                    if handle_provider_event(event, event_sender) {
                        return;
                    }
                }
                Err(error) => {
                    send_event(
                        event_sender,
                        VoiceInputEvent::Failed(
                            format!("Voice transcription stopped before finalizing: {error}").into(),
                        ),
                    );
                    return;
                }
            },
        }
    }
}

fn handle_provider_event(
    event: VoiceTranscriptionEvent,
    event_sender: &Sender<VoiceInputEvent>,
) -> bool {
    match event {
        VoiceTranscriptionEvent::Final(text) => {
            send_event(event_sender, VoiceInputEvent::Final(text));
            false
        }
        VoiceTranscriptionEvent::Finished => {
            send_event(event_sender, VoiceInputEvent::Finished);
            true
        }
        VoiceTranscriptionEvent::Failed { message } => {
            send_event(event_sender, VoiceInputEvent::Failed(message));
            true
        }
    }
}

fn send_event(sender: &Sender<VoiceInputEvent>, event: VoiceInputEvent) -> bool {
    match sender.try_send(event) {
        Ok(()) => true,
        Err(TrySendError::Closed(_)) | Err(TrySendError::Full(_)) => false,
    }
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;
    use gpui::TestAppContext;

    use super::*;

    struct TestProvider {
        id: VoiceInputProviderId,
        settings_updates: Option<Arc<AtomicU8>>,
        supported_models: &'static [VoiceInputModel],
    }

    impl VoiceInputProvider for TestProvider {
        fn id(&self) -> VoiceInputProviderId {
            self.id.clone()
        }

        fn supported_models(&self) -> &[VoiceInputModel] {
            self.supported_models
        }

        fn configuration(&self, _cx: &App) -> Result<VoiceTranscriptionConfig> {
            Ok(VoiceTranscriptionConfig::default())
        }

        fn settings_changed(&self, _session_active: bool, _cx: &mut App) {
            if let Some(settings_updates) = &self.settings_updates {
                settings_updates.fetch_add(1, Ordering::Relaxed);
            }
        }

        fn start(
            &self,
            _config: VoiceTranscriptionConfig,
            _cx: &mut App,
        ) -> Task<Result<VoiceTranscriptionSession>> {
            let (session, _) = voice_transcription_transport();
            Task::ready(Ok(session))
        }
    }

    #[derive(Clone, Copy)]
    enum TestCaptureStop {
        Complete,
        Stall,
    }

    struct TestCapture {
        sender: Sender<Result<Option<AudioChunk>>>,
        receiver: Receiver<Result<Option<AudioChunk>>>,
        stop: TestCaptureStop,
    }

    #[async_trait]
    impl VoiceInputAudioCapture for TestCapture {
        fn stop(&self) {
            if matches!(self.stop, TestCaptureStop::Complete) {
                match self.sender.try_send(Ok(None)) {
                    Ok(()) | Err(TrySendError::Closed(_)) => {}
                    Err(TrySendError::Full(_)) => {
                        panic!("the unbounded test capture channel cannot be full");
                    }
                }
            }
        }

        async fn receive(&self) -> Result<Option<AudioChunk>> {
            match self.receiver.recv().await {
                Ok(result) => result,
                Err(_) => Err(anyhow!("test capture channel closed")),
            }
        }
    }

    struct ControllableTestProvider {
        backend_sender: Sender<VoiceTranscriptionBackend>,
    }

    impl VoiceInputProvider for ControllableTestProvider {
        fn id(&self) -> VoiceInputProviderId {
            VoiceInputProviderId("controllable-test-provider".into())
        }

        fn start(
            &self,
            _config: VoiceTranscriptionConfig,
            _cx: &mut App,
        ) -> Task<Result<VoiceTranscriptionSession>> {
            let (session, backend) = voice_transcription_transport();
            let result = match self.backend_sender.try_send(backend) {
                Ok(()) => Ok(session),
                Err(error) => Err(anyhow!("failed to expose test backend: {error}")),
            };
            Task::ready(result)
        }
    }

    fn start_controllable_session(
        stop: TestCaptureStop,
        cx: &mut App,
    ) -> Result<(
        VoiceInputSession,
        Sender<Result<Option<AudioChunk>>>,
        Receiver<VoiceTranscriptionBackend>,
    )> {
        init(cx);
        let (capture_sender, capture_receiver) = async_channel::unbounded();
        let capture_factory = Arc::new({
            let capture_sender = capture_sender.clone();
            move |_config: &VoiceTranscriptionConfig, _cx: &App| {
                Ok(Box::new(TestCapture {
                    sender: capture_sender.clone(),
                    receiver: capture_receiver.clone(),
                    stop,
                }) as Box<dyn VoiceInputAudioCapture>)
            }
        });
        let (backend_sender, backend_receiver) = async_channel::unbounded();
        VoiceInputRegistry::global(cx).update(cx, |registry, cx| {
            registry.capture_factory = capture_factory;
            registry.register_provider(ControllableTestProvider { backend_sender }, cx);
        });
        let session = VoiceInputSession::start(cx)?;
        Ok((session, capture_sender, backend_receiver))
    }

    #[test]
    fn transcription_defaults_to_english_mono_pcm() {
        let config = VoiceTranscriptionConfig::default();

        assert_eq!(config.language, TRANSCRIPTION_LANGUAGE);
        assert_eq!(config.channel_count, TRANSCRIPTION_CHANNEL_COUNT);
        assert_eq!(config.sample_rate, TRANSCRIPTION_SAMPLE_RATE);
    }

    #[test]
    fn finishing_delivers_buffered_audio_before_finalization() -> Result<()> {
        let (session, backend) = voice_transcription_transport();
        let chunk = AudioChunk::new(Arc::<[i16]>::from([1, 2, 3]));

        session.try_send_audio(chunk.clone())?;
        session.finish();
        assert_eq!(
            block_on(backend.receive()),
            VoiceTranscriptionInput::Audio(chunk)
        );
        assert_eq!(block_on(backend.receive()), VoiceTranscriptionInput::Finish);
        Ok(())
    }

    #[test]
    fn backend_can_receive_cancellation_after_finish() {
        let (session, backend) = voice_transcription_transport();
        session.finish();
        assert_eq!(block_on(backend.receive()), VoiceTranscriptionInput::Finish);
        assert!(backend.receive_cancel().now_or_never().is_none());

        assert!(session.cancel());
        assert!(block_on(backend.receive_cancel()).is_ok());
    }

    #[test]
    fn cancellation_interrupts_buffered_audio() -> Result<()> {
        let (session, backend) = voice_transcription_transport();
        session.try_send_audio(AudioChunk::new(Arc::<[i16]>::from([1, 2, 3])))?;

        session.cancel();
        assert_eq!(block_on(backend.receive()), VoiceTranscriptionInput::Cancel);
        Ok(())
    }

    #[test]
    fn provider_events_reach_the_active_session() -> Result<()> {
        let (session, backend) = voice_transcription_transport();
        let event = VoiceTranscriptionEvent::Finished;
        block_on(backend.send_event(event.clone()))?;
        assert_eq!(block_on(session.receive_event())?, event);
        Ok(())
    }

    #[test]
    fn stopping_capture_preserves_an_existing_overflow() {
        let (chunk_sender, chunk_receiver) = async_channel::bounded(1);
        let status = Arc::new(AtomicU8::new(CAPTURE_STATUS_OVERFLOWED));
        let capture = MicrophoneCapture {
            chunk_receiver,
            stopped: Arc::new(AtomicBool::new(false)),
            status: status.clone(),
            worker: None,
        };
        drop(chunk_sender);

        capture.stop();
        assert_eq!(status.load(Ordering::Acquire), CAPTURE_STATUS_OVERFLOWED);
        let error = match block_on(capture.receive()) {
            Ok(_) => panic!("an overflowed capture must not stop successfully"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "microphone audio buffer exceeded capacity"
        );
    }

    async fn finishing_after_speech_delivers_audio_before_finalization_scenario(
        cx: &mut TestAppContext,
    ) -> Result<()> {
        let (mut session, capture_sender, backend_receiver) =
            cx.update(|cx| start_controllable_session(TestCaptureStop::Complete, cx))?;
        let events = session.events();
        let backend = backend_receiver.recv().await?;
        assert_eq!(
            events.recv().await?,
            VoiceInputEvent::StateChanged(VoiceInputState::Listening)
        );
        let chunk = AudioChunk::new(Arc::<[i16]>::from([1, 2, 3]));
        capture_sender.send(Ok(Some(chunk.clone()))).await?;

        assert!(session.finish());
        assert_eq!(
            events.recv().await?,
            VoiceInputEvent::StateChanged(VoiceInputState::Transcribing)
        );
        assert_eq!(
            backend.receive().await,
            VoiceTranscriptionInput::Audio(chunk)
        );
        assert_eq!(backend.receive().await, VoiceTranscriptionInput::Finish);
        backend
            .send_event(VoiceTranscriptionEvent::Finished)
            .await?;
        assert_eq!(events.recv().await?, VoiceInputEvent::Finished);
        Ok(())
    }

    #[gpui::test]
    async fn finishing_after_speech_delivers_audio_before_finalization(cx: &mut TestAppContext) {
        let result = finishing_after_speech_delivers_audio_before_finalization_scenario(cx).await;
        assert!(result.is_ok(), "{result:?}");
    }

    async fn finishing_during_startup_finalizes_instead_of_cancelling_scenario(
        cx: &mut TestAppContext,
    ) -> Result<()> {
        let (mut session, _capture_sender, backend_receiver) =
            cx.update(|cx| start_controllable_session(TestCaptureStop::Complete, cx))?;
        let events = session.events();
        assert!(session.finish());

        let backend = backend_receiver.recv().await?;
        assert_eq!(
            events.recv().await?,
            VoiceInputEvent::StateChanged(VoiceInputState::Transcribing)
        );
        assert_eq!(backend.receive().await, VoiceTranscriptionInput::Finish);
        backend
            .send_event(VoiceTranscriptionEvent::Finished)
            .await?;
        assert_eq!(events.recv().await?, VoiceInputEvent::Finished);
        Ok(())
    }

    #[gpui::test]
    async fn finishing_during_startup_finalizes_instead_of_cancelling(cx: &mut TestAppContext) {
        let result = finishing_during_startup_finalizes_instead_of_cancelling_scenario(cx).await;
        assert!(result.is_ok(), "{result:?}");
    }

    async fn capture_overflow_while_finishing_fails_instead_of_finalizing_scenario(
        cx: &mut TestAppContext,
    ) -> Result<()> {
        let (mut session, capture_sender, backend_receiver) =
            cx.update(|cx| start_controllable_session(TestCaptureStop::Complete, cx))?;
        let events = session.events();
        let backend = backend_receiver.recv().await?;
        assert_eq!(
            events.recv().await?,
            VoiceInputEvent::StateChanged(VoiceInputState::Listening)
        );
        capture_sender
            .send(Err(anyhow!("microphone audio buffer exceeded capacity")))
            .await?;

        assert!(session.finish());
        assert_eq!(
            events.recv().await?,
            VoiceInputEvent::StateChanged(VoiceInputState::Transcribing)
        );
        assert_eq!(
            events.recv().await?,
            VoiceInputEvent::Failed("microphone audio buffer exceeded capacity".into())
        );
        assert_eq!(backend.receive().await, VoiceTranscriptionInput::Cancel);
        Ok(())
    }

    #[gpui::test]
    async fn capture_overflow_while_finishing_fails_instead_of_finalizing(cx: &mut TestAppContext) {
        let result =
            capture_overflow_while_finishing_fails_instead_of_finalizing_scenario(cx).await;
        assert!(result.is_ok(), "{result:?}");
    }

    async fn stalled_capture_shutdown_fails_and_allows_recovery_scenario(
        cx: &mut TestAppContext,
    ) -> Result<()> {
        let (mut session, _capture_sender, backend_receiver) =
            cx.update(|cx| start_controllable_session(TestCaptureStop::Stall, cx))?;
        let events = session.events();
        let backend = backend_receiver.recv().await?;
        assert_eq!(
            events.recv().await?,
            VoiceInputEvent::StateChanged(VoiceInputState::Listening)
        );

        assert!(session.finish());
        assert_eq!(
            events.recv().await?,
            VoiceInputEvent::StateChanged(VoiceInputState::Transcribing)
        );
        assert_eq!(
            events.recv().await?,
            VoiceInputEvent::Failed(
                "Timed out while stopping microphone capture; the message was not sent.".into()
            )
        );
        assert_eq!(backend.receive().await, VoiceTranscriptionInput::Cancel);
        cx.background_executor.timer(Duration::from_millis(1)).await;
        assert!(cx.read(|cx| {
            VoiceInputRegistry::read_global(cx)
                .active_session()
                .is_none()
        }));
        Ok(())
    }

    #[gpui::test]
    async fn stalled_capture_shutdown_fails_and_allows_recovery(cx: &mut TestAppContext) {
        let result = stalled_capture_shutdown_fails_and_allows_recovery_scenario(cx).await;
        assert!(result.is_ok(), "{result:?}");
    }

    #[gpui::test]
    fn registry_selects_registered_providers(cx: &mut App) {
        init(cx);
        let registry = VoiceInputRegistry::global(cx);
        let first_id = VoiceInputProviderId("first".into());
        let second_id = VoiceInputProviderId("second".into());
        let first_settings_updates = Arc::new(AtomicU8::new(0));
        let second_settings_updates = Arc::new(AtomicU8::new(0));

        registry.update(cx, |registry, cx| {
            registry.register_provider(
                TestProvider {
                    id: first_id.clone(),
                    settings_updates: Some(first_settings_updates.clone()),
                    supported_models: &[],
                },
                cx,
            );
            registry.register_provider(
                TestProvider {
                    id: second_id.clone(),
                    settings_updates: Some(second_settings_updates.clone()),
                    supported_models: &[],
                },
                cx,
            );
        });

        assert_eq!(
            registry
                .read(cx)
                .active_provider()
                .map(|provider| provider.id()),
            Some(first_id)
        );
        registry.update(cx, |registry, cx| registry.update_provider_settings(cx));
        assert_eq!(first_settings_updates.load(Ordering::Relaxed), 1);
        assert_eq!(second_settings_updates.load(Ordering::Relaxed), 1);
        registry.update(cx, |registry, cx| {
            assert!(registry.set_active_provider(&second_id, cx));
        });
        assert_eq!(
            registry
                .read(cx)
                .active_provider()
                .map(|provider| provider.id()),
            Some(second_id)
        );
    }

    #[gpui::test]
    fn registry_rejects_duplicate_model_owners(cx: &mut App) {
        init(cx);
        let registry = VoiceInputRegistry::global(cx);
        let first_id = VoiceInputProviderId("first-model-owner".into());
        let duplicate_id = VoiceInputProviderId("duplicate-model-owner".into());
        registry.update(cx, |registry, cx| {
            registry.unregister_provider(&VoiceInputProviderId("parakeet".into()), cx);
            registry.register_provider(
                TestProvider {
                    id: first_id.clone(),
                    settings_updates: None,
                    supported_models: &[VoiceInputModel::ParakeetTdtV3],
                },
                cx,
            );
            registry.register_provider(
                TestProvider {
                    id: duplicate_id.clone(),
                    settings_updates: None,
                    supported_models: &[VoiceInputModel::ParakeetTdtV3],
                },
                cx,
            );
        });

        assert!(registry.read(cx).providers.contains_key(&first_id));
        assert!(!registry.read(cx).providers.contains_key(&duplicate_id));
    }

    #[gpui::test]
    fn configured_model_selects_its_provider(cx: &mut App) {
        let default_settings = settings::default_settings();
        let mut settings_store = SettingsStore::new(cx, default_settings.as_ref());
        assert_eq!(
            settings_store
                .set_user_settings(
                    r#"{"audio":{"voice_input_enabled":false,"voice_input_model":"parakeet_tdt_v3"}}"#,
                    cx,
                )
                .parse_status,
            settings::ParseStatus::Success
        );
        cx.set_global(settings_store);

        let registry = cx.new(|_| VoiceInputRegistry::default());
        cx.set_global(GlobalVoiceInputRegistry(registry.clone()));
        let other_provider_id = VoiceInputProviderId("other".into());
        let model_provider_id = VoiceInputProviderId("local-model".into());
        registry.update(cx, |registry, cx| {
            registry.register_provider(
                TestProvider {
                    id: other_provider_id.clone(),
                    settings_updates: None,
                    supported_models: &[],
                },
                cx,
            );
            registry.register_provider(
                TestProvider {
                    id: model_provider_id.clone(),
                    settings_updates: None,
                    supported_models: &[VoiceInputModel::ParakeetTdtV3],
                },
                cx,
            );
        });

        assert_eq!(
            registry
                .read(cx)
                .active_provider()
                .map(|provider| provider.id()),
            Some(model_provider_id)
        );
    }

    #[gpui::test]
    fn registry_allows_only_one_active_session(cx: &mut App) {
        init(cx);
        let registry = VoiceInputRegistry::global(cx);
        registry.update(cx, |registry, cx| {
            registry.register_provider(
                TestProvider {
                    id: VoiceInputProviderId("provider".into()),
                    settings_updates: None,
                    supported_models: &[],
                },
                cx,
            );
        });

        let first_session = registry.update(cx, |registry, cx| registry.claim_session(cx));
        let Some((first_session_id, _, _)) = first_session else {
            panic!("registered provider should allow a session");
        };
        assert!(
            registry
                .update(cx, |registry, cx| registry.claim_session(cx))
                .is_none()
        );
        assert!(!registry.update(cx, |registry, cx| {
            registry.release_session(VoiceInputSessionId(first_session_id.0.wrapping_add(1)), cx)
        }));
        assert!(registry.update(cx, |registry, cx| {
            registry.release_session(first_session_id, cx)
        }));
        assert!(
            registry
                .update(cx, |registry, cx| registry.claim_session(cx))
                .is_some()
        );
    }

    #[gpui::test]
    async fn session_enters_transcribing_when_stopped(cx: &mut TestAppContext) {
        let result = session_enters_transcribing_when_stopped_scenario(cx).await;
        assert!(result.is_ok(), "{result:?}");
    }

    async fn session_enters_transcribing_when_stopped_scenario(
        cx: &mut TestAppContext,
    ) -> Result<()> {
        let (registry, mut session) = cx.update(|cx| -> Result<_> {
            init(cx);
            let registry = VoiceInputRegistry::global(cx);
            registry.update(cx, |registry, cx| {
                registry.register_provider(
                    TestProvider {
                        id: VoiceInputProviderId("offline-test-provider".into()),
                        settings_updates: None,
                        supported_models: &[],
                    },
                    cx,
                );
            });
            Ok((registry, VoiceInputSession::start(cx)?))
        })?;

        assert!(session.finish());
        assert_eq!(session.state(), VoiceInputState::Transcribing);
        session.cancel();
        cx.run_until_parked();

        assert!(cx.read(|cx| registry.read(cx).active_session().is_none()));
        Ok(())
    }
}
