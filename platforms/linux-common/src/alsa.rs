use core_audio::mixer::AudioInputProducer;
use core_types::{CoreError, CoreResult};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

#[allow(non_camel_case_types)]
type snd_pcm_t = c_void;
#[allow(non_camel_case_types)]
type snd_pcm_hw_params_t = c_void;
#[allow(non_camel_case_types)]
type snd_pcm_uframes_t = c_ulong;
#[allow(non_camel_case_types)]
type snd_pcm_sframes_t = c_long;

const SND_PCM_STREAM_CAPTURE: c_int = 1;
const SND_PCM_ACCESS_RW_INTERLEAVED: c_int = 3;
const SND_PCM_FORMAT_S16_LE: c_int = 2;
const SND_PCM_FORMAT_S32_LE: c_int = 10;

#[link(name = "asound")]
extern "C" {
    fn snd_pcm_open(
        pcm: *mut *mut snd_pcm_t,
        name: *const c_char,
        stream: c_int,
        mode: c_int,
    ) -> c_int;
    fn snd_pcm_close(pcm: *mut snd_pcm_t) -> c_int;
    fn snd_pcm_prepare(pcm: *mut snd_pcm_t) -> c_int;
    fn snd_pcm_recover(pcm: *mut snd_pcm_t, err: c_int, silent: c_int) -> c_int;
    fn snd_pcm_readi(
        pcm: *mut snd_pcm_t,
        buffer: *mut c_void,
        size: snd_pcm_uframes_t,
    ) -> snd_pcm_sframes_t;

    fn snd_pcm_hw_params_malloc(ptr: *mut *mut snd_pcm_hw_params_t) -> c_int;
    fn snd_pcm_hw_params_free(obj: *mut snd_pcm_hw_params_t);
    fn snd_pcm_hw_params_any(pcm: *mut snd_pcm_t, params: *mut snd_pcm_hw_params_t) -> c_int;
    fn snd_pcm_hw_params_set_access(
        pcm: *mut snd_pcm_t,
        params: *mut snd_pcm_hw_params_t,
        access: c_int,
    ) -> c_int;
    fn snd_pcm_hw_params_set_format(
        pcm: *mut snd_pcm_t,
        params: *mut snd_pcm_hw_params_t,
        val: c_int,
    ) -> c_int;
    fn snd_pcm_hw_params_set_channels(
        pcm: *mut snd_pcm_t,
        params: *mut snd_pcm_hw_params_t,
        val: c_uint,
    ) -> c_int;
    fn snd_pcm_hw_params_set_rate_near(
        pcm: *mut snd_pcm_t,
        params: *mut snd_pcm_hw_params_t,
        val: *mut c_uint,
        dir: *mut c_int,
    ) -> c_int;
    fn snd_pcm_hw_params_set_period_size_near(
        pcm: *mut snd_pcm_t,
        params: *mut snd_pcm_hw_params_t,
        val: *mut snd_pcm_uframes_t,
        dir: *mut c_int,
    ) -> c_int;
    fn snd_pcm_hw_params_set_buffer_size_near(
        pcm: *mut snd_pcm_t,
        params: *mut snd_pcm_hw_params_t,
        val: *mut snd_pcm_uframes_t,
    ) -> c_int;
    fn snd_pcm_hw_params(pcm: *mut snd_pcm_t, params: *mut snd_pcm_hw_params_t) -> c_int;
    fn snd_strerror(errnum: c_int) -> *const c_char;
}

fn alsa_err(ctx: &str, code: c_int) -> CoreError {
    let msg = unsafe {
        let p = snd_strerror(code);
        if p.is_null() {
            "unknown ALSA error".to_string()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    };
    CoreError::PlatformIo(format!("ALSA {}: {} ({})", ctx, msg, code))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlsaSampleFormat {
    S16Le,
    S32Le,
}

#[derive(Debug, Clone)]
pub struct AlsaCaptureConfig {
    pub device_name: String,
    pub sample_rate: u32,
    pub channels: u32,
    /// 推奨ピリオドサイズ: 256〜512 frames (約 5.3〜10.6ms @ 48kHz)
    pub period_frames: u32,
}

impl Default for AlsaCaptureConfig {
    fn default() -> Self {
        Self {
            device_name: "default".to_string(),
            sample_rate: 48_000,
            channels: 2,
            period_frames: 480,
        }
    }
}

struct PcmHandle(*mut snd_pcm_t);
unsafe impl Send for PcmHandle {}

impl Drop for PcmHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                snd_pcm_close(self.0);
            }
            self.0 = ptr::null_mut();
        }
    }
}

/// `libasound.so` 直接ハンドリングによる低レイテンシ ALSA キャプチャスレッド
pub struct AlsaCaptureThread {
    stop_flag: Arc<AtomicBool>,
    join_handle: Option<JoinHandle<()>>,
    pub negotiated_rate: u32,
    pub negotiated_channels: u32,
    pub negotiated_period_frames: u32,
    pub negotiated_format: AlsaSampleFormat,
}

impl AlsaCaptureThread {
    pub fn spawn(
        config: AlsaCaptureConfig,
        mut producer: AudioInputProducer,
    ) -> CoreResult<Self> {
        let c_dev = CString::new(config.device_name.as_str())
            .map_err(|e| CoreError::PlatformIo(format!("invalid ALSA device name: {}", e)))?;

        let mut pcm_ptr: *mut snd_pcm_t = ptr::null_mut();
        let rc = unsafe { snd_pcm_open(&mut pcm_ptr, c_dev.as_ptr(), SND_PCM_STREAM_CAPTURE, 0) };
        if rc < 0 || pcm_ptr.is_null() {
            return Err(alsa_err("snd_pcm_open", rc));
        }
        let pcm = PcmHandle(pcm_ptr);

        let mut hw_params: *mut snd_pcm_hw_params_t = ptr::null_mut();
        let rc = unsafe { snd_pcm_hw_params_malloc(&mut hw_params) };
        if rc < 0 || hw_params.is_null() {
            return Err(alsa_err("snd_pcm_hw_params_malloc", rc));
        }

        let setup_res = unsafe {
            let mut res = Ok(());
            macro_rules! check {
                ($expr:expr, $label:expr) => {
                    let r = $expr;
                    if r < 0 && res.is_ok() {
                        res = Err(alsa_err($label, r));
                    }
                };
            }

            check!(snd_pcm_hw_params_any(pcm.0, hw_params), "hw_params_any");
            check!(
                snd_pcm_hw_params_set_access(pcm.0, hw_params, SND_PCM_ACCESS_RW_INTERLEAVED),
                "set_access"
            );

            let fmt = if snd_pcm_hw_params_set_format(pcm.0, hw_params, SND_PCM_FORMAT_S16_LE) >= 0 {
                AlsaSampleFormat::S16Le
            } else if snd_pcm_hw_params_set_format(pcm.0, hw_params, SND_PCM_FORMAT_S32_LE) >= 0 {
                AlsaSampleFormat::S32Le
            } else {
                res = Err(CoreError::UnsupportedFormat(
                    "ALSA device supports neither S16_LE nor S32_LE".to_string(),
                ));
                AlsaSampleFormat::S16Le
            };

            check!(
                snd_pcm_hw_params_set_channels(pcm.0, hw_params, config.channels as c_uint),
                "set_channels"
            );

            let mut rate = config.sample_rate as c_uint;
            let mut dir: c_int = 0;
            check!(
                snd_pcm_hw_params_set_rate_near(pcm.0, hw_params, &mut rate, &mut dir),
                "set_rate_near"
            );

            let mut period = config.period_frames.clamp(128, 2048) as snd_pcm_uframes_t;
            let mut pdir: c_int = 0;
            check!(
                snd_pcm_hw_params_set_period_size_near(pcm.0, hw_params, &mut period, &mut pdir),
                "set_period_size_near"
            );

            let mut buffer_size = period * 4;
            check!(
                snd_pcm_hw_params_set_buffer_size_near(pcm.0, hw_params, &mut buffer_size),
                "set_buffer_size_near"
            );

            check!(snd_pcm_hw_params(pcm.0, hw_params), "snd_pcm_hw_params");
            check!(snd_pcm_prepare(pcm.0), "snd_pcm_prepare");

            snd_pcm_hw_params_free(hw_params);
            res.map(|_| (rate as u32, period as u32, fmt))
        };

        let (negotiated_rate, negotiated_period_frames, negotiated_format) = setup_res?;
        let negotiated_channels = config.channels;
        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop_flag);

        let join_handle = thread::Builder::new()
            .name(format!("alsa-cap-{}", config.device_name))
            .spawn(move || {
                let _pcm_guard = pcm;
                let samples_per_period =
                    (negotiated_period_frames as usize) * (negotiated_channels as usize);
                match negotiated_format {
                    AlsaSampleFormat::S16Le => {
                        let mut buf = vec![0i16; samples_per_period];
                        while !thread_stop.load(Ordering::Acquire) {
                            let n = unsafe {
                                snd_pcm_readi(
                                    _pcm_guard.0,
                                    buf.as_mut_ptr() as *mut c_void,
                                    negotiated_period_frames as snd_pcm_uframes_t,
                                )
                            };
                            if n > 0 {
                                let valid = (n as usize) * (negotiated_channels as usize);
                                let _ = producer.push_i16(&buf[..valid]);
                            } else if n < 0 {
                                let rec = unsafe { snd_pcm_recover(_pcm_guard.0, n as c_int, 1) };
                                if rec < 0 {
                                    break;
                                }
                            }
                        }
                    }
                    AlsaSampleFormat::S32Le => {
                        let mut buf = vec![0i32; samples_per_period];
                        while !thread_stop.load(Ordering::Acquire) {
                            let n = unsafe {
                                snd_pcm_readi(
                                    _pcm_guard.0,
                                    buf.as_mut_ptr() as *mut c_void,
                                    negotiated_period_frames as snd_pcm_uframes_t,
                                )
                            };
                            if n > 0 {
                                let valid = (n as usize) * (negotiated_channels as usize);
                                let _ = producer.push_i32(&buf[..valid]);
                            } else if n < 0 {
                                let rec = unsafe { snd_pcm_recover(_pcm_guard.0, n as c_int, 1) };
                                if rec < 0 {
                                    break;
                                }
                            }
                        }
                    }
                }
            })
            .map_err(|e| CoreError::PlatformIo(format!("failed to spawn ALSA thread: {}", e)))?;

        Ok(Self {
            stop_flag,
            join_handle: Some(join_handle),
            negotiated_rate,
            negotiated_channels,
            negotiated_period_frames,
            negotiated_format,
        })
    }

    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::Release);
        if let Some(h) = self.join_handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for AlsaCaptureThread {
    fn drop(&mut self) {
        self.stop();
    }
}
