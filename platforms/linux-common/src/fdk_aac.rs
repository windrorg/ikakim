use core_audio::AUDIO_CHANNELS;
use core_types::clock::{PtsUs, MASTER_SAMPLE_RATE};
use core_types::media::{CodecKind, EncodedPacket};
use core_types::traits::AudioEncoder;
use core_types::{CoreError, CoreResult};
use std::collections::VecDeque;
use std::mem;
use std::os::raw::{c_int, c_uint, c_void};
use std::ptr;

#[allow(non_camel_case_types)]
type HANDLE_AACENCODER = *mut c_void;
#[allow(non_camel_case_types)]
type AACENC_ERROR = c_int;

const AACENC_OK: AACENC_ERROR = 0;

// AACENC_PARAM
const AACENC_AOT: c_int = 0x0100;
const AACENC_BITRATE: c_int = 0x0101;
const AACENC_SAMPLERATE: c_int = 0x0103;
const AACENC_CHANNELMODE: c_int = 0x0106;
const AACENC_CHANNELORDER: c_int = 0x0107;
const AACENC_AFTERBURNER: c_int = 0x0200;
const AACENC_TRANSMUX: c_int = 0x0300;

const AOT_AAC_LC: c_uint = 2;
const MODE_2: c_uint = 2;
const TT_MP4_RAW: c_uint = 0;

const IN_AUDIO_DATA: c_int = 0;
const OUT_BITSTREAM_DATA: c_int = 3;

const AAC_FRAME_SAMPLES: usize = 1024;

#[repr(C)]
struct AACENC_BufDesc {
    num_bufs: c_int,
    bufs: *mut *mut c_void,
    buffer_identifiers: *mut c_int,
    buf_sizes: *mut c_int,
    buf_el_sizes: *mut c_int,
}

#[repr(C)]
struct AACENC_InArgs {
    num_in_samples: c_int,
    num_anc_bytes: c_int,
}

#[repr(C)]
#[derive(Default)]
struct AACENC_OutArgs {
    num_out_bytes: c_int,
    num_in_samples: c_int,
    num_anc_bytes: c_int,
    bit_res_state: c_int,
}

#[repr(C)]
struct AACENC_InfoStruct {
    max_out_buf_bytes: c_uint,
    max_anc_bytes: c_uint,
    in_buf_fill_level: c_uint,
    input_channels: c_uint,
    frame_length: c_uint,
    n_delay: c_uint,
    n_delay_core: c_uint,
    conf_buf: [u8; 64],
    conf_size: c_uint,
}

#[link(name = "fdk-aac")]
extern "C" {
    fn aacEncOpen(
        ph_aac_encoder: *mut HANDLE_AACENCODER,
        enc_modules: c_uint,
        max_channels: c_uint,
    ) -> AACENC_ERROR;
    fn aacEncClose(ph_aac_encoder: *mut HANDLE_AACENCODER) -> AACENC_ERROR;
    fn aacEncoder_SetParam(
        h_aac_encoder: HANDLE_AACENCODER,
        param: c_int,
        value: c_uint,
    ) -> AACENC_ERROR;
    fn aacEncEncode(
        h_aac_encoder: HANDLE_AACENCODER,
        in_buf_desc: *const AACENC_BufDesc,
        out_buf_desc: *const AACENC_BufDesc,
        in_args: *const AACENC_InArgs,
        out_args: *mut AACENC_OutArgs,
    ) -> AACENC_ERROR;
    fn aacEncInfo(
        h_aac_encoder: HANDLE_AACENCODER,
        p_info: *mut AACENC_InfoStruct,
    ) -> AACENC_ERROR;
}

/// `libfdk-aac` 薄い C-FFI を用いた 48kHz / 2ch AAC-LC エンコーダ
pub struct FdkAacEncoder {
    handle: HANDLE_AACENCODER,
    pcm_i16_queue: VecDeque<i16>,
    in_frame_buf: Vec<i16>,
    out_buf: Vec<u8>,
    output_queue: VecDeque<EncodedPacket>,
    encoded_frames_total: u64,
    first_pts: Option<PtsUs>,
}

unsafe impl Send for FdkAacEncoder {}

impl FdkAacEncoder {
    pub fn new(bitrate_bps: u32) -> CoreResult<Self> {
        let mut handle: HANDLE_AACENCODER = ptr::null_mut();
        let rc = unsafe { aacEncOpen(&mut handle, 0, AUDIO_CHANNELS as c_uint) };
        if rc != AACENC_OK || handle.is_null() {
            return Err(CoreError::Codec(format!(
                "aacEncOpen failed with code {}",
                rc
            )));
        }

        let set_res = unsafe {
            let mut err = AACENC_OK;
            for (param, val) in [
                (AACENC_AOT, AOT_AAC_LC),
                (AACENC_SAMPLERATE, MASTER_SAMPLE_RATE as c_uint),
                (AACENC_CHANNELMODE, MODE_2),
                (AACENC_CHANNELORDER, 1),
                (AACENC_BITRATE, bitrate_bps as c_uint),
                (AACENC_TRANSMUX, TT_MP4_RAW),
                (AACENC_AFTERBURNER, 1),
            ] {
                let r = aacEncoder_SetParam(handle, param, val);
                if r != AACENC_OK {
                    err = r;
                    break;
                }
            }
            if err == AACENC_OK {
                err = aacEncEncode(
                    handle,
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null_mut(),
                );
            }
            err
        };

        if set_res != AACENC_OK {
            unsafe {
                aacEncClose(&mut handle);
            }
            return Err(CoreError::Codec(format!(
                "libfdk-aac parameter setup failed with code {}",
                set_res
            )));
        }

        let mut info: AACENC_InfoStruct = unsafe { mem::zeroed() };
        let info_rc = unsafe { aacEncInfo(handle, &mut info) };
        if info_rc != AACENC_OK {
            unsafe {
                aacEncClose(&mut handle);
            }
            return Err(CoreError::Codec(format!(
                "aacEncInfo failed with code {}",
                info_rc
            )));
        }

        let conf_len = (info.conf_size as usize).min(info.conf_buf.len());
        let asc = if conf_len > 0 {
            info.conf_buf[..conf_len].to_vec()
        } else {
            vec![0x11, 0x90]
        };

        let max_out = (info.max_out_buf_bytes as usize).max(4096);
        let mut output_queue = VecDeque::new();
        output_queue.push_back(EncodedPacket {
            codec: CodecKind::AacLc,
            pts: PtsUs(0),
            dts: PtsUs(0),
            is_keyframe: true,
            is_sequence_header: true,
            data: asc,
        });

        Ok(Self {
            handle,
            pcm_i16_queue: VecDeque::with_capacity(AAC_FRAME_SAMPLES * AUDIO_CHANNELS * 4),
            in_frame_buf: Vec::with_capacity(AAC_FRAME_SAMPLES * AUDIO_CHANNELS),
            out_buf: vec![0u8; max_out],
            output_queue,
            encoded_frames_total: 0,
            first_pts: None,
        })
    }

    fn drain_available_aac(&mut self) -> CoreResult<()> {
        let frame_samples = AAC_FRAME_SAMPLES * AUDIO_CHANNELS;
        while self.pcm_i16_queue.len() >= frame_samples {
            self.in_frame_buf.clear();
            let (front, back) = self.pcm_i16_queue.as_slices();
            if front.len() >= frame_samples {
                self.in_frame_buf.extend_from_slice(&front[..frame_samples]);
            } else {
                self.in_frame_buf.extend_from_slice(front);
                self.in_frame_buf
                    .extend_from_slice(&back[..frame_samples - front.len()]);
            }
            self.pcm_i16_queue.drain(..frame_samples);

            let mut in_ptr = self.in_frame_buf.as_mut_ptr() as *mut c_void;
            let mut in_id = IN_AUDIO_DATA;
            let mut in_size = (frame_samples * mem::size_of::<i16>()) as c_int;
            let mut in_el_size = mem::size_of::<i16>() as c_int;
            let in_desc = AACENC_BufDesc {
                num_bufs: 1,
                bufs: &mut in_ptr,
                buffer_identifiers: &mut in_id,
                buf_sizes: &mut in_size,
                buf_el_sizes: &mut in_el_size,
            };

            let mut out_ptr = self.out_buf.as_mut_ptr() as *mut c_void;
            let mut out_id = OUT_BITSTREAM_DATA;
            let mut out_size = self.out_buf.len() as c_int;
            let mut out_el_size = 1 as c_int;
            let out_desc = AACENC_BufDesc {
                num_bufs: 1,
                bufs: &mut out_ptr,
                buffer_identifiers: &mut out_id,
                buf_sizes: &mut out_size,
                buf_el_sizes: &mut out_el_size,
            };

            let in_args = AACENC_InArgs {
                num_in_samples: frame_samples as c_int,
                num_anc_bytes: 0,
            };
            let mut out_args = AACENC_OutArgs::default();

            let rc = unsafe {
                aacEncEncode(self.handle, &in_desc, &out_desc, &in_args, &mut out_args)
            };
            if rc != AACENC_OK {
                return Err(CoreError::Codec(format!(
                    "aacEncEncode failed with code {}",
                    rc
                )));
            }

            if out_args.num_out_bytes > 0 {
                let n = out_args.num_out_bytes as usize;
                let base_pts = self.first_pts.unwrap_or(PtsUs(0)).0;
                let delta_us = (self.encoded_frames_total as u128 * 1_000_000)
                    / (MASTER_SAMPLE_RATE as u128);
                let pts = PtsUs(base_pts + delta_us as u64);

                self.output_queue.push_back(EncodedPacket {
                    codec: CodecKind::AacLc,
                    pts,
                    dts: pts,
                    is_keyframe: true,
                    is_sequence_header: false,
                    data: self.out_buf[..n].to_vec(),
                });
            }
            self.encoded_frames_total += AAC_FRAME_SAMPLES as u64;
        }
        Ok(())
    }
}

impl AudioEncoder for FdkAacEncoder {
    fn push_pcm_f32(&mut self, interleaved_pcm: &[f32], pts: PtsUs) -> CoreResult<()> {
        if self.first_pts.is_none() {
            self.first_pts = Some(pts);
        }
        let valid = (interleaved_pcm.len() / AUDIO_CHANNELS) * AUDIO_CHANNELS;
        for &s in &interleaved_pcm[..valid] {
            let scaled = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
            self.pcm_i16_queue.push_back(scaled);
        }
        self.drain_available_aac()
    }

    fn pull_packet(&mut self) -> CoreResult<Option<EncodedPacket>> {
        Ok(self.output_queue.pop_front())
    }
}

impl Drop for FdkAacEncoder {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                aacEncClose(&mut self.handle);
            }
            self.handle = ptr::null_mut();
        }
    }
}
