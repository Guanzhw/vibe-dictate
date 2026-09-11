//! Native client for the local VibeVoice-ASR-Streaming websocket protocol.
//!
//! The capture callback only converts samples and performs a bounded
//! `try_send`. Resampling, websocket I/O, parsing, and output all happen off
//! the callback and UI threads. A full queue is an explicit session error so
//! that a user never receives text for audio that was silently dropped.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use std::{io, net::TcpStream};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use crossbeam_channel::{bounded, Receiver, Sender};
use serde::Deserialize;
use serde_json::json;
use tungstenite::client::IntoClientRequest;
use tungstenite::http::header::{HeaderValue, AUTHORIZATION};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{connect, Message, WebSocket};

use crate::config::{AudioConfig, ServerConfig, SttConfig};

pub const TARGET_SAMPLE_RATE: u32 = 24_000;
const AUDIO_QUEUE_CAPACITY: usize = 32;

#[derive(Debug, Clone)]
struct AudioChunk {
    samples: Vec<f32>,
    channels: u16,
    sample_rate: u32,
}

#[derive(Debug)]
enum Control {
    Release,
    Cancel,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Partial(String),
    Done(StreamResult),
    Error(String),
    Cancelled,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct StreamResult {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clean_text: Option<String>,
    pub first_update_ms: Option<u128>,
    pub final_ms: u128,
    pub release_to_final_ms: Option<u128>,
}

/// A linear resampler whose fractional position survives arbitrary input
/// chunk boundaries. `finish` pads the final interpolation point with the
/// final sample, avoiding a discontinuity when a short capture ends between
/// output sample positions.
#[derive(Debug, Clone)]
pub struct Resampler {
    input_rate: u32,
    output_rate: u32,
    buffer: Vec<f32>,
    buffer_start: usize,
    next_input_pos: f64,
}

impl Resampler {
    pub fn new(input_rate: u32, output_rate: u32) -> Result<Self> {
        if input_rate == 0 || output_rate == 0 {
            return Err(anyhow!("sample rates must be non-zero"));
        }
        Ok(Self {
            input_rate,
            output_rate,
            buffer: Vec::new(),
            buffer_start: 0,
            next_input_pos: 0.0,
        })
    }

    pub fn push(&mut self, input: &[f32]) -> Vec<f32> {
        if input.is_empty() {
            return Vec::new();
        }
        self.buffer.extend_from_slice(input);
        self.produce(false)
    }

    pub fn finish(&mut self) -> Vec<f32> {
        self.produce(true)
    }

    fn produce(&mut self, final_chunk: bool) -> Vec<f32> {
        let ratio = self.input_rate as f64 / self.output_rate as f64;
        let end = self.buffer_start + self.buffer.len();
        let mut out = Vec::new();
        loop {
            let pos = self.next_input_pos;
            let floor = pos.floor() as usize;
            if floor < self.buffer_start || floor >= end {
                break;
            }
            let local = floor - self.buffer_start;
            let a = self.buffer[local];
            let b = if local + 1 < self.buffer.len() {
                self.buffer[local + 1]
            } else if final_chunk {
                a
            } else {
                break;
            };
            let frac = (pos - floor as f64) as f32;
            out.push(a + (b - a) * frac);
            self.next_input_pos += ratio;
        }

        // Keep one prior sample for the next interpolation. This also bounds
        // memory independently of dictation duration.
        let keep_from = (self.next_input_pos.floor() as usize).saturating_sub(1);
        if keep_from > self.buffer_start {
            let drop_count = (keep_from - self.buffer_start).min(self.buffer.len());
            self.buffer.drain(..drop_count);
            self.buffer_start += drop_count;
        }
        out
    }
}

pub struct StreamingCapture {
    stream: Option<cpal::Stream>,
    control: Sender<Control>,
    cancel_token: Arc<AtomicBool>,
}

impl StreamingCapture {
    pub fn start(
        audio_cfg: &AudioConfig,
        server_cfg: ServerConfig,
        stt_cfg: SttConfig,
    ) -> Result<(Self, Receiver<StreamEvent>)> {
        let host = cpal::default_host();
        let device = pick_input_device(&host, &audio_cfg.mic_device)?;
        let default_cfg = device
            .default_input_config()
            .context("default_input_config")?;
        let format = default_cfg.sample_format();
        let channels = default_cfg.channels();
        let sample_rate = default_cfg.sample_rate().0;
        let stream_cfg: StreamConfig = default_cfg.into();
        Self::start_with_format(
            device,
            stream_cfg,
            format,
            channels,
            sample_rate,
            server_cfg,
            stt_cfg,
        )
    }

    fn start_with_format(
        device: cpal::Device,
        stream_cfg: StreamConfig,
        format: SampleFormat,
        channels: u16,
        sample_rate: u32,
        server_cfg: ServerConfig,
        stt_cfg: SttConfig,
    ) -> Result<(Self, Receiver<StreamEvent>)> {
        let (audio_tx, audio_rx) = bounded::<AudioChunk>(AUDIO_QUEUE_CAPACITY);
        let (control_tx, control_rx) = bounded::<Control>(2);
        let (event_tx, event_rx) = bounded::<StreamEvent>(64);
        let overflow = Arc::new(AtomicBool::new(false));
        let cancel_token = Arc::new(AtomicBool::new(false));
        let callback_overflow = overflow.clone();
        let error_slot = Arc::new(Mutex::new(None::<String>));
        let callback_error = error_slot.clone();
        let stream = match format {
            SampleFormat::F32 => device.build_input_stream(
                &stream_cfg,
                move |data: &[f32], _| {
                    let chunk = AudioChunk {
                        samples: data.to_vec(),
                        channels,
                        sample_rate,
                    };
                    if audio_tx.try_send(chunk).is_err() {
                        callback_overflow.store(true, Ordering::Release);
                    }
                },
                move |e| {
                    *callback_error.lock().unwrap() = Some(format!("audio stream error: {e:?}"));
                },
                None,
            ),
            SampleFormat::I16 => device.build_input_stream(
                &stream_cfg,
                move |data: &[i16], _| {
                    let chunk = AudioChunk {
                        samples: data.iter().map(|&x| x as f32 / 32768.0).collect(),
                        channels,
                        sample_rate,
                    };
                    if audio_tx.try_send(chunk).is_err() {
                        callback_overflow.store(true, Ordering::Release);
                    }
                },
                move |e| {
                    *callback_error.lock().unwrap() = Some(format!("audio stream error: {e:?}"));
                },
                None,
            ),
            SampleFormat::U16 => device.build_input_stream(
                &stream_cfg,
                move |data: &[u16], _| {
                    let chunk = AudioChunk {
                        samples: data
                            .iter()
                            .map(|&x| (x as f32 - 32768.0) / 32768.0)
                            .collect(),
                        channels,
                        sample_rate,
                    };
                    if audio_tx.try_send(chunk).is_err() {
                        callback_overflow.store(true, Ordering::Release);
                    }
                },
                move |e| {
                    *callback_error.lock().unwrap() = Some(format!("audio stream error: {e:?}"));
                },
                None,
            ),
            _ => return Err(anyhow!("unsupported sample format: {format:?}")),
        }
        .context("build_input_stream")?;
        stream.play().context("stream.play")?;
        spawn_worker(
            audio_rx,
            control_rx,
            event_tx,
            overflow.clone(),
            error_slot,
            server_cfg,
            stt_cfg,
        );
        Ok((
            Self {
                stream: Some(stream),
                control: control_tx,
                cancel_token,
            },
            event_rx,
        ))
    }

    pub fn release(&mut self) {
        self.stream.take();
        let _ = self.control.send(Control::Release);
    }

    pub fn cancel(&mut self) {
        self.cancel_token.store(true, Ordering::Release);
        self.stream.take();
        let _ = self.control.send(Control::Cancel);
    }

    pub fn cancel_token(&self) -> Arc<AtomicBool> {
        self.cancel_token.clone()
    }

    pub fn is_recording(&self) -> bool {
        self.stream.is_some()
    }
}

impl Drop for StreamingCapture {
    fn drop(&mut self) {
        self.cancel_token.store(true, Ordering::Release);
        self.stream.take();
        let _ = self.control.try_send(Control::Cancel);
    }
}

fn spawn_worker(
    audio_rx: Receiver<AudioChunk>,
    control_rx: Receiver<Control>,
    event_tx: Sender<StreamEvent>,
    overflow: Arc<AtomicBool>,
    error_slot: Arc<Mutex<Option<String>>>,
    server_cfg: ServerConfig,
    stt_cfg: SttConfig,
) {
    thread::spawn(move || {
        if let Err(e) = run_worker(
            audio_rx,
            control_rx,
            event_tx.clone(),
            overflow,
            error_slot,
            server_cfg,
            stt_cfg,
        ) {
            let _ = event_tx.send(StreamEvent::Error(format!("{e:#}")));
        }
    });
}

fn run_worker(
    audio_rx: Receiver<AudioChunk>,
    control_rx: Receiver<Control>,
    event_tx: Sender<StreamEvent>,
    overflow: Arc<AtomicBool>,
    error_slot: Arc<Mutex<Option<String>>>,
    server_cfg: ServerConfig,
    stt_cfg: SttConfig,
) -> Result<()> {
    let url = websocket_url(&server_cfg.base_url)?;
    let start = Instant::now();
    let (mut socket, _) =
        connect_stream(&url, &server_cfg.api_key).context("connect streaming websocket")?;
    set_nonblocking(&mut socket).context("set streaming socket nonblocking")?;
    socket
        .send(Message::Text(
            json!({
                "context_info": stt_cfg.context_info,
                "max_tokens": 256,
                "temperature": 0,
            })
            .to_string(),
        ))
        .context("send streaming config")?;
    let mut resampler: Option<Resampler> = None;
    let mut text = String::new();
    let mut clean_text = None;
    let mut first_update_ms = None;
    let release_at: Option<Instant>;
    let mut audio_closed = false;
    loop {
        if let Ok(command) = control_rx.try_recv() {
            match command {
                Control::Cancel => {
                    let _ = socket.close(None);
                    let _ = event_tx.send(StreamEvent::Cancelled);
                    return Ok(());
                }
                Control::Release => {
                    release_at = Some(Instant::now());
                    while let Ok(chunk) = audio_rx.try_recv() {
                        send_chunk(&mut socket, &mut resampler, chunk)?;
                        receive_available(
                            &mut socket,
                            &event_tx,
                            &mut text,
                            &mut clean_text,
                            &mut first_update_ms,
                            start,
                        )?;
                    }
                    if overflow.load(Ordering::Acquire) {
                        return Err(anyhow!(
                            "audio capture queue overflowed; transcription discarded"
                        ));
                    }
                    if let Some(r) = resampler.as_mut() {
                        let tail = r.finish();
                        if !tail.is_empty() {
                            socket.send(Message::Binary(f32_bytes(&tail)))?;
                        }
                    }
                    socket
                        .send(Message::Text("end".into()))
                        .context("send streaming end")?;
                    match wait_for_done(
                        &mut socket,
                        &control_rx,
                        &event_tx,
                        &mut text,
                        &mut clean_text,
                        &mut first_update_ms,
                        start,
                    )? {
                        WaitOutcome::Done => {}
                        WaitOutcome::Cancelled => return Ok(()),
                        WaitOutcome::Closed => return Err(anyhow!("stream closed before done")),
                    }
                    let final_ms = start.elapsed().as_millis();
                    let release_to_final_ms = release_at.map(|t| t.elapsed().as_millis());
                    let _ = event_tx.send(StreamEvent::Done(StreamResult {
                        text,
                        clean_text,
                        first_update_ms,
                        final_ms,
                        release_to_final_ms,
                    }));
                    return Ok(());
                }
            }
        }
        if !audio_closed {
            match audio_rx.recv_timeout(Duration::from_millis(20)) {
                Ok(chunk) => {
                    send_chunk(&mut socket, &mut resampler, chunk)?;
                    receive_available(
                        &mut socket,
                        &event_tx,
                        &mut text,
                        &mut clean_text,
                        &mut first_update_ms,
                        start,
                    )?;
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    receive_available(
                        &mut socket,
                        &event_tx,
                        &mut text,
                        &mut clean_text,
                        &mut first_update_ms,
                        start,
                    )?;
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => audio_closed = true,
            }
        } else {
            receive_available(
                &mut socket,
                &event_tx,
                &mut text,
                &mut clean_text,
                &mut first_update_ms,
                start,
            )?;
            thread::sleep(Duration::from_millis(10));
        }
        if let Some(msg) = error_slot.lock().unwrap().take() {
            return Err(anyhow!(msg));
        }
    }
}

fn send_chunk<S: std::io::Read + std::io::Write>(
    socket: &mut tungstenite::WebSocket<S>,
    resampler: &mut Option<Resampler>,
    chunk: AudioChunk,
) -> Result<()> {
    let mono = downmix(&chunk.samples, chunk.channels as usize);
    let r = resampler.get_or_insert(Resampler::new(chunk.sample_rate, TARGET_SAMPLE_RATE)?);
    if r.input_rate != chunk.sample_rate {
        return Err(anyhow!("input sample rate changed mid-session"));
    }
    let out = r.push(&mono);
    if !out.is_empty() {
        socket.send(Message::Binary(f32_bytes(&out)))?;
    }
    Ok(())
}

fn receive_available<S: std::io::Read + std::io::Write>(
    socket: &mut tungstenite::WebSocket<S>,
    event_tx: &Sender<StreamEvent>,
    text: &mut String,
    clean_text: &mut Option<String>,
    first_update_ms: &mut Option<u128>,
    start: Instant,
) -> Result<()> {
    loop {
        match socket.read() {
            Ok(Message::Text(s)) => {
                let _ =
                    parse_server_message(&s, event_tx, text, clean_text, first_update_ms, start)?;
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                return Ok(())
            }
            Err(e) => return Err(anyhow!(e).context("read streaming update")),
        }
    }
}

fn set_nonblocking(socket: &mut WebSocket<MaybeTlsStream<TcpStream>>) -> io::Result<()> {
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => stream.set_nonblocking(true),
        #[allow(unreachable_patterns)]
        _ => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "non-plain streaming sockets are unsupported",
        )),
    }
}

enum WaitOutcome {
    Done,
    Cancelled,
    Closed,
}

fn wait_for_done<S: std::io::Read + std::io::Write>(
    socket: &mut tungstenite::WebSocket<S>,
    control_rx: &Receiver<Control>,
    event_tx: &Sender<StreamEvent>,
    text: &mut String,
    clean_text: &mut Option<String>,
    first_update_ms: &mut Option<u128>,
    start: Instant,
) -> Result<WaitOutcome> {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if Instant::now() >= deadline {
            return Err(anyhow!("timed out waiting for streaming done"));
        }
        if let Ok(Control::Cancel) = control_rx.try_recv() {
            let _ = socket.close(None);
            let _ = event_tx.send(StreamEvent::Cancelled);
            return Ok(WaitOutcome::Cancelled);
        }
        match socket.read() {
            Ok(Message::Text(s)) => {
                if parse_server_message(&s, event_tx, text, clean_text, first_update_ms, start)? {
                    return Ok(WaitOutcome::Done);
                }
            }
            Ok(Message::Close(_)) => return Ok(WaitOutcome::Closed),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(anyhow!(e).context("read streaming response")),
        }
    }
}

fn parse_server_message(
    raw: &str,
    event_tx: &Sender<StreamEvent>,
    text: &mut String,
    clean_text: &mut Option<String>,
    first_update_ms: &mut Option<u128>,
    start: Instant,
) -> Result<bool> {
    #[derive(Deserialize)]
    struct ServerMessage {
        text: Option<String>,
        _chunks: Option<serde_json::Value>,
        clean_text: Option<String>,
        done: Option<bool>,
        error: Option<String>,
    }
    let msg: ServerMessage = serde_json::from_str(raw).context("parse streaming response")?;
    if let Some(error) = msg.error {
        return Err(anyhow!("streaming backend: {error}"));
    }
    if let Some(clean) = msg.clean_text.clone() {
        *clean_text = Some(clean);
    }
    if let Some(v) = msg.text {
        *text = v.clone();
        if first_update_ms.is_none() {
            *first_update_ms = Some(start.elapsed().as_millis());
        }
        let _ = event_tx.try_send(StreamEvent::Partial(msg.clean_text.unwrap_or(v)));
    }
    Ok(msg.done.unwrap_or(false))
}

fn f32_bytes(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 4);
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

fn downmix(samples: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return samples.to_vec();
    }
    samples
        .chunks(channels)
        .map(|frame| frame.iter().copied().sum::<f32>() / channels as f32)
        .collect()
}

fn pick_input_device(host: &cpal::Host, name: &str) -> Result<cpal::Device> {
    if name.is_empty() {
        return host
            .default_input_device()
            .ok_or_else(|| anyhow!("No default input device"));
    }
    for d in host.input_devices()? {
        if d.name().map(|n| n == name).unwrap_or(false) {
            return Ok(d);
        }
    }
    host.default_input_device()
        .ok_or_else(|| anyhow!("No default input device"))
}

pub fn websocket_url(base_url: &str) -> Result<String> {
    let base = base_url.trim_end_matches('/');
    if base.starts_with("ws://") || base.starts_with("wss://") {
        if base.ends_with("/ws/asr") {
            return Ok(base.to_string());
        }
        return Ok(format!("{base}/ws/asr"));
    }
    let http = base
        .strip_prefix("http://")
        .map(|x| ("ws://", x))
        .or_else(|| base.strip_prefix("https://").map(|x| ("wss://", x)))
        .ok_or_else(|| anyhow!("unsupported streaming server URL '{base_url}'"))?;
    Ok(format!("{}{}/ws/asr", http.0, http.1))
}

pub fn health_check(server_cfg: &ServerConfig) -> Result<()> {
    let ws = websocket_url(&server_cfg.base_url)?;
    let http = ws
        .strip_prefix("ws://")
        .map(|s| format!("http://{s}"))
        .or_else(|| ws.strip_prefix("wss://").map(|s| format!("https://{s}")))
        .ok_or_else(|| anyhow!("unsupported streaming health URL"))?;
    let root = http.strip_suffix("/ws/asr").unwrap_or(&http);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let mut request = client.get(format!("{root}/healthz"));
    if !server_cfg.api_key.is_empty() {
        request = request.bearer_auth(&server_cfg.api_key);
    }
    let response = request.send()?;
    if !response.status().is_success() {
        return Err(anyhow!("healthz returned {}", response.status()));
    }
    #[derive(Deserialize)]
    struct HealthResponse {
        status: String,
    }
    let health: HealthResponse = response.json().context("parse streaming healthz")?;
    if health.status != "ok" {
        return Err(anyhow!("streaming backend status is '{}'", health.status));
    }
    Ok(())
}

/// File transport used by `--transcribe-file`; it exercises the same wire
/// protocol and resampler without opening a microphone. When supplied,
/// `event_sink` receives best-effort nonblocking partial and done events.
pub fn transcribe_file(
    path: &str,
    server_cfg: ServerConfig,
    stt_cfg: SttConfig,
    realtime: bool,
    event_sink: Option<Sender<StreamEvent>>,
) -> Result<StreamResult> {
    let mut reader = hound::WavReader::open(path).with_context(|| format!("open WAV '{path}'"))?;
    let spec = reader.spec();
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<std::result::Result<_, _>>()?,
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .map(|v| v.map(|s| s as f32 / i16::MAX as f32))
            .collect::<std::result::Result<_, _>>()?,
    };
    let mono = downmix(&samples, spec.channels as usize);
    let mut r = Resampler::new(spec.sample_rate, TARGET_SAMPLE_RATE)?;
    let mut chunks = r.push(&mono);
    chunks.extend(r.finish());
    let ws_url = websocket_url(&server_cfg.base_url)?;
    let (mut socket, _) =
        connect_stream(&ws_url, &server_cfg.api_key).context("connect streaming websocket")?;
    set_nonblocking(&mut socket).context("set streaming socket nonblocking")?;
    socket.send(Message::Text(
        json!({"context_info": stt_cfg.context_info, "max_tokens": 256, "temperature": 0})
            .to_string(),
    ))?;
    let started = Instant::now();
    let event_tx = event_sink.unwrap_or_else(|| bounded(1).0);
    let (_control_tx, control_rx) = bounded(1);
    let mut text = String::new();
    let mut clean_text = None;
    let mut first = None;
    for part in chunks.chunks(TARGET_SAMPLE_RATE as usize / 20) {
        socket.send(Message::Binary(f32_bytes(part)))?;
        receive_available(
            &mut socket,
            &event_tx,
            &mut text,
            &mut clean_text,
            &mut first,
            started,
        )?;
        if realtime {
            thread::sleep(Duration::from_millis(50));
        }
    }
    let released = Instant::now();
    socket.send(Message::Text("end".into()))?;
    let wait = wait_for_done(
        &mut socket,
        &control_rx,
        &event_tx,
        &mut text,
        &mut clean_text,
        &mut first,
        started,
    )?;
    if !matches!(wait, WaitOutcome::Done) {
        return Err(anyhow!("stream closed before done"));
    }
    let result = StreamResult {
        text,
        clean_text,
        first_update_ms: first,
        final_ms: started.elapsed().as_millis(),
        release_to_final_ms: Some(released.elapsed().as_millis()),
    };
    let _ = event_tx.try_send(StreamEvent::Done(result.clone()));
    Ok(result)
}

/// Real microphone smoke path. It exercises WASAPI/cpal startup, bounded
/// callback capture, websocket streaming, release drain, and finalization;
/// callers receive metrics only and perform no text injection.
pub fn record_seconds(
    audio_cfg: &AudioConfig,
    server_cfg: ServerConfig,
    stt_cfg: SttConfig,
    seconds: u64,
) -> Result<StreamResult> {
    if !(1..=15).contains(&seconds) {
        return Err(anyhow!("record duration must be between 1 and 15 seconds"));
    }
    let (mut capture, events) = StreamingCapture::start(audio_cfg, server_cfg, stt_cfg)?;
    thread::sleep(Duration::from_secs(seconds));
    capture.release();
    loop {
        match events.recv_timeout(Duration::from_secs(120))? {
            StreamEvent::Partial(_) => {}
            StreamEvent::Done(result) => return Ok(result),
            StreamEvent::Cancelled => return Err(anyhow!("recording was cancelled")),
            StreamEvent::Error(message) => return Err(anyhow!(message)),
        }
    }
}

fn connect_stream(
    url: &str,
    api_key: &str,
) -> Result<(
    WebSocket<MaybeTlsStream<TcpStream>>,
    tungstenite::handshake::client::Response,
)> {
    let mut request = url
        .into_client_request()
        .context("build websocket request")?;
    if !api_key.trim().is_empty() {
        let value = HeaderValue::from_str(&format!("Bearer {}", api_key.trim()))
            .context("invalid streaming API key")?;
        request.headers_mut().insert(AUTHORIZATION, value);
    }
    Ok(connect(request)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn resampler_keeps_fractional_phase_across_chunks() {
        let input: Vec<f32> = (0..32).map(|n| n as f32 / 32.0).collect();
        let mut whole = Resampler::new(16_000, TARGET_SAMPLE_RATE).unwrap();
        let mut expected = whole.push(&input);
        expected.extend(whole.finish());

        let mut split = Resampler::new(16_000, TARGET_SAMPLE_RATE).unwrap();
        let mut actual = split.push(&input[..7]);
        actual.extend(split.push(&input[7..19]));
        actual.extend(split.push(&input[19..]));
        actual.extend(split.finish());

        assert_eq!(actual.len(), expected.len());
        for (a, b) in actual.iter().zip(expected.iter()) {
            assert!((a - b).abs() < 1e-6, "{a} != {b}");
        }
    }

    #[test]
    fn websocket_url_has_one_asr_path() {
        assert_eq!(
            websocket_url("http://127.0.0.1:7870").unwrap(),
            "ws://127.0.0.1:7870/ws/asr"
        );
        assert_eq!(
            websocket_url("ws://127.0.0.1:7870/ws/asr/").unwrap(),
            "ws://127.0.0.1:7870/ws/asr"
        );
    }

    #[test]
    fn pcm_wire_bytes_are_little_endian_f32() {
        assert_eq!(f32_bytes(&[1.0, -0.5]), vec![0, 0, 128, 63, 0, 0, 0, 191]);
    }

    #[test]
    fn worker_drains_audio_then_sends_literal_end_and_waits_for_done() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            assert!(matches!(socket.read().unwrap(), Message::Text(_)));
            let mut binary_frames = 0;
            loop {
                match socket.read().unwrap() {
                    Message::Binary(_) => binary_frames += 1,
                    Message::Text(text) => {
                        assert_eq!(text, "end");
                        break;
                    }
                    _ => {}
                }
            }
            assert!(binary_frames >= 1);
            socket
                .send(Message::Text(
                    json!({"text":"hello", "done":true, "total_chunks":1}).to_string(),
                ))
                .unwrap();
        });

        let (audio_tx, audio_rx) = bounded(4);
        let (control_tx, control_rx) = bounded(2);
        let (event_tx, event_rx) = bounded(8);
        let server_cfg = ServerConfig {
            base_url: format!("http://{address}"),
            ..ServerConfig::default()
        };
        let stt_cfg = SttConfig {
            context_info: "test".into(),
            ..SttConfig::default()
        };
        let worker = thread::spawn(move || {
            run_worker(
                audio_rx,
                control_rx,
                event_tx,
                Arc::new(AtomicBool::new(false)),
                Arc::new(Mutex::new(None)),
                server_cfg,
                stt_cfg,
            )
            .unwrap();
        });
        audio_tx
            .send(AudioChunk {
                samples: vec![0.0; 10],
                channels: 1,
                sample_rate: 16_000,
            })
            .unwrap();
        control_tx.send(Control::Release).unwrap();
        let result = event_rx
            .iter()
            .find_map(|evt| match evt {
                StreamEvent::Done(result) => Some(result),
                _ => None,
            })
            .unwrap();
        assert_eq!(result.text, "hello");
        assert!(result.first_update_ms.is_some());
        worker.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn file_transport_delivers_partial_before_end_frame() {
        let path = std::env::temp_dir().join(format!(
            "vibe-dictate-streaming-{}-{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: TARGET_SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for _ in 0..TARGET_SAMPLE_RATE {
            writer.write_sample(0i16).unwrap();
        }
        writer.finalize().unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let end_seen = Arc::new(AtomicBool::new(false));
        let server_end_seen = end_seen.clone();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            assert!(matches!(socket.read().unwrap(), Message::Text(_)));
            let mut sent_partial = false;
            loop {
                match socket.read().unwrap() {
                    Message::Binary(_) if !sent_partial => {
                        socket
                            .send(Message::Text(
                                json!({"text":"raw live", "clean_text":"live"}).to_string(),
                            ))
                            .unwrap();
                        sent_partial = true;
                    }
                    Message::Text(text) if text == "end" => {
                        server_end_seen.store(true, Ordering::Release);
                        socket
                            .send(Message::Text(
                                json!({"text":"raw final", "clean_text":"final", "done":true})
                                    .to_string(),
                            ))
                            .unwrap();
                        break;
                    }
                    _ => {}
                }
            }
        });

        let (event_tx, event_rx) = bounded(8);
        let server_cfg = ServerConfig {
            base_url: format!("http://{address}"),
            ..ServerConfig::default()
        };
        let call = thread::spawn({
            let path = path.clone();
            move || {
                transcribe_file(
                    path.to_str().unwrap(),
                    server_cfg,
                    SttConfig::default(),
                    true,
                    Some(event_tx),
                )
            }
        });

        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            StreamEvent::Partial(text) if text == "live"
        ));
        assert!(!end_seen.load(Ordering::Acquire));
        assert!(!call.is_finished());
        let result = call.join().unwrap().unwrap();
        assert_eq!(result.text, "raw final");
        assert_eq!(result.clean_text.as_deref(), Some("final"));
        let released_ms = result.final_ms - result.release_to_final_ms.unwrap();
        assert!(result.first_update_ms.unwrap() < released_ms);
        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            StreamEvent::Partial(text) if text == "final"
        ));
        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            StreamEvent::Done(result) if result.text == "raw final"
        ));
        server.join().unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn parser_state_isolated_between_sessions() {
        let (events, _rx) = bounded(4);
        let mut first = String::new();
        let mut first_clean = None;
        let mut first_update = None;
        let mut second = String::new();
        let mut second_clean = None;
        let mut second_update = None;
        parse_server_message(
            r#"{"text":"first"}"#,
            &events,
            &mut first,
            &mut first_clean,
            &mut first_update,
            Instant::now(),
        )
        .unwrap();
        parse_server_message(
            r#"{"text":"second"}"#,
            &events,
            &mut second,
            &mut second_clean,
            &mut second_update,
            Instant::now(),
        )
        .unwrap();
        assert_eq!(first, "first");
        assert_eq!(second, "second");
    }

    #[test]
    fn cancel_closes_socket_without_sending_an_end_frame() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            assert!(matches!(socket.read().unwrap(), Message::Text(_)));
            match socket.read() {
                Ok(Message::Close(_)) | Err(_) => {}
                Ok(other) => panic!("unexpected cancel frame: {other:?}"),
            }
        });
        let (_audio_tx, audio_rx) = bounded(2);
        let (control_tx, control_rx) = bounded(2);
        let (event_tx, event_rx) = bounded(4);
        let server_cfg = ServerConfig {
            base_url: format!("http://{address}"),
            ..ServerConfig::default()
        };
        let stt_cfg = SttConfig::default();
        let worker = thread::spawn(move || {
            run_worker(
                audio_rx,
                control_rx,
                event_tx,
                Arc::new(AtomicBool::new(false)),
                Arc::new(Mutex::new(None)),
                server_cfg,
                stt_cfg,
            )
            .unwrap();
        });
        control_tx.send(Control::Cancel).unwrap();
        assert!(matches!(event_rx.recv().unwrap(), StreamEvent::Cancelled));
        worker.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn cancel_during_finishing_interrupts_delayed_done() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let _ = socket.read().unwrap();
            loop {
                match socket.read().unwrap() {
                    Message::Text(text) if text == "end" => break,
                    _ => {}
                }
            }
            thread::sleep(Duration::from_secs(2));
        });
        let (_audio_tx, audio_rx) = bounded(2);
        let (control_tx, control_rx) = bounded(2);
        let (event_tx, event_rx) = bounded(4);
        let server_cfg = ServerConfig {
            base_url: format!("http://{address}"),
            ..ServerConfig::default()
        };
        let worker = thread::spawn(move || {
            run_worker(
                audio_rx,
                control_rx,
                event_tx,
                Arc::new(AtomicBool::new(false)),
                Arc::new(Mutex::new(None)),
                server_cfg,
                SttConfig::default(),
            )
            .unwrap();
        });
        control_tx.send(Control::Release).unwrap();
        thread::sleep(Duration::from_millis(30));
        control_tx.send(Control::Cancel).unwrap();
        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            StreamEvent::Cancelled
        ));
        worker.join().unwrap();
        server.join().unwrap();
    }
}
