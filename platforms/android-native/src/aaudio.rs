//! Android `AAudio` Low-Latency Capture Bridge -> `core-audio` `AudioInputProducer` (M4: Issue #8)

use core_audio::AudioInputProducer;
use core_types::clock::MASTER_SAMPLE_RATE;
use core_types::{CoreError, CoreResult};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub const AAUDIO_PERFORMANCE_MODE_LOW_LATENCY: u32 = 12;
pub const AAUDIO_FORMAT_PCM_FLOAT: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AAudioConfig {
    pub sample_rate: u32,
    pub channel_count: u32,
    pub performance_mode: u32,
    pub format: u32,
}

impl Default for AAudioConfig {
    fn default() -> Self {
        Self {
            sample_rate: MASTER_SAMPLE_RATE as u32,
            channel_count: 2,
            performance_mode: AAUDIO_PERFORMANCE_MODE_LOW_LATENCY,
            format: AAUDIO_FORMAT_PCM_FLOAT,
        }
    }
}

/// Abstract backend for Android `libaaudio.so` low-latency capture streams.
pub trait AAudioBackend: Send + Sync {
    fn open_input_stream(&self, cfg: &AAudioConfig) -> CoreResult<u64>;
    fn start(&self, stream_id: u64) -> CoreResult<()>;
    fn read_f32(&self, stream_id: u64, dst: &mut [f32]) -> CoreResult<usize>;
    fn stop(&self, stream_id: u64) -> CoreResult<()>;
    fn close(&self, stream_id: u64);
}

#[derive(Debug)]
struct MockAudioStream {
    cfg: AAudioConfig,
    running: bool,
    phase: usize,
}

/// Deterministic mock `AAudio` backend generating stereo PCM floats.
#[derive(Debug, Default)]
pub struct MockAAudioBackend {
    state: Mutex<(u64, HashMap<u64, MockAudioStream>)>,
}

impl MockAAudioBackend {
    pub fn new() -> Self {
        Self::default()
    }
}

impl AAudioBackend for MockAAudioBackend {
    fn open_input_stream(&self, cfg: &AAudioConfig) -> CoreResult<u64> {
        if cfg.sample_rate == 0 || cfg.channel_count == 0 {
            return Err(CoreError::PlatformIo("invalid AAudio config".into()));
        }
        let mut guard = self.state.lock().unwrap();
        guard.0 += 1;
        let id = guard.0;
        guard.1.insert(
            id,
            MockAudioStream {
                cfg: cfg.clone(),
                running: false,
                phase: 0,
            },
        );
        Ok(id)
    }

    fn start(&self, stream_id: u64) -> CoreResult<()> {
        let mut guard = self.state.lock().unwrap();
        let stream = guard
            .1
            .get_mut(&stream_id)
            .ok_or_else(|| CoreError::PlatformIo(format!("unknown AAudio stream {}", stream_id)))?;
        stream.running = true;
        Ok(())
    }

    fn read_f32(&self, stream_id: u64, dst: &mut [f32]) -> CoreResult<usize> {
        let mut guard = self.state.lock().unwrap();
        let stream = guard
            .1
            .get_mut(&stream_id)
            .ok_or_else(|| CoreError::PlatformIo(format!("unknown AAudio stream {}", stream_id)))?;
        if !stream.running {
            return Err(CoreError::PlatformIo("AAudio stream not started".into()));
        }
        let ch = stream.cfg.channel_count as usize;
        let frames = dst.len() / ch;
        for f in 0..frames {
            let amp = (((stream.phase + f) as f32) * 0.05).sin() * 0.25;
            for c in 0..ch {
                dst[f * ch + c] = amp;
            }
        }
        stream.phase += frames;
        Ok(frames * ch)
    }

    fn stop(&self, stream_id: u64) -> CoreResult<()> {
        let mut guard = self.state.lock().unwrap();
        if let Some(stream) = guard.1.get_mut(&stream_id) {
            stream.running = false;
        }
        Ok(())
    }

    fn close(&self, stream_id: u64) {
        self.state.lock().unwrap().1.remove(&stream_id);
    }
}

/// Captures PCM frames from `AAudioBackend` and feeds lock-free `AudioInputProducer`.
pub struct AAudioCaptureBridge {
    backend: Arc<dyn AAudioBackend>,
    stream_id: u64,
    scratch: Vec<f32>,
}

impl AAudioCaptureBridge {
    pub fn open(backend: Arc<dyn AAudioBackend>, cfg: AAudioConfig) -> CoreResult<Self> {
        let stream_id = backend.open_input_stream(&cfg)?;
        backend.start(stream_id)?;
        let scratch = vec![0.0f32; 480 * (cfg.channel_count.max(1) as usize)];
        Ok(Self {
            backend,
            stream_id,
            scratch,
        })
    }

    pub fn pump_into_producer(&mut self, producer: &mut AudioInputProducer) -> CoreResult<usize> {
        let read = self.backend.read_f32(self.stream_id, &mut self.scratch)?;
        Ok(producer.push_f32(&self.scratch[..read]))
    }
}

impl Drop for AAudioCaptureBridge {
    fn drop(&mut self) {
        let _ = self.backend.stop(self.stream_id);
        self.backend.close(self.stream_id);
    }
}
