use core_audio::mixer::AudioInputProducer;
use core_audio::AUDIO_CHANNELS;
use core_types::clock::{PtsUs, MASTER_SAMPLE_RATE};
use core_types::media::{CodecKind, EncodedPacket};
use core_types::traits::AudioEncoder;
use core_types::{CoreError, CoreResult};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem;
use std::ptr;
use std::sync::{Arc, Mutex};

type OSStatus = i32;
type AudioFormatID = u32;
type AudioFormatFlags = u32;
type AudioConverterPropertyID = u32;
type AudioConverterRef = *mut c_void;
type AudioQueueRef = *mut c_void;

const NO_ERR: OSStatus = 0;
const NEED_MORE_DATA_STATUS: OSStatus = 0x6E6D_6F72; // 'nmor'

const K_AUDIO_FORMAT_LINEAR_PCM: AudioFormatID = u32::from_be_bytes(*b"lpcm");
const K_AUDIO_FORMAT_MPEG4_AAC: AudioFormatID = u32::from_be_bytes(*b"aac ");

const K_AUDIO_FORMAT_FLAG_IS_FLOAT: AudioFormatFlags = 1 << 0;
const K_AUDIO_FORMAT_FLAG_IS_PACKED: AudioFormatFlags = 1 << 3;
const K_AUDIO_FORMAT_FLAGS_NATIVE_FLOAT_PACKED: AudioFormatFlags =
    K_AUDIO_FORMAT_FLAG_IS_FLOAT | K_AUDIO_FORMAT_FLAG_IS_PACKED;

const K_AUDIO_CONVERTER_ENCODE_BIT_RATE: AudioConverterPropertyID = u32::from_be_bytes(*b"brat");
const K_AUDIO_CONVERTER_COMPRESSION_MAGIC_COOKIE: AudioConverterPropertyID =
    u32::from_be_bytes(*b"cmgc");
const K_AUDIO_CONVERTER_PROPERTY_MAX_OUTPUT_PACKET_SIZE: AudioConverterPropertyID =
    u32::from_be_bytes(*b"xops");

const AAC_FRAME_SAMPLES: usize = 1024;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct AudioStreamBasicDescription {
    m_sample_rate: f64,
    m_format_id: AudioFormatID,
    m_format_flags: AudioFormatFlags,
    m_bytes_per_packet: u32,
    m_frames_per_packet: u32,
    m_bytes_per_frame: u32,
    m_channels_per_frame: u32,
    m_bits_per_channel: u32,
    m_reserved: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct AudioBuffer {
    m_number_channels: u32,
    m_data_byte_size: u32,
    m_data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    m_number_buffers: u32,
    m_buffers: [AudioBuffer; 1],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct AudioStreamPacketDescription {
    m_start_offset: i64,
    m_variable_frames_in_packet: u32,
    m_data_byte_size: u32,
}

#[repr(C)]
struct AudioTimeStamp {
    _opaque: [u8; 64],
}

#[repr(C)]
struct AudioQueueBuffer {
    m_audio_data_bytes_capacity: u32,
    m_audio_data: *mut c_void,
    m_audio_data_byte_size: u32,
    m_user_data: *mut c_void,
    m_packet_description_capacity: u32,
    m_packet_descriptions: *mut AudioStreamPacketDescription,
    m_packet_description_count: u32,
}

type AudioConverterComplexInputDataProc = unsafe extern "C" fn(
    in_audio_converter: AudioConverterRef,
    io_number_data_packets: *mut u32,
    io_data: *mut AudioBufferList,
    out_data_packet_description: *mut *mut AudioStreamPacketDescription,
    in_user_data: *mut c_void,
) -> OSStatus;

type AudioQueueInputCallback = unsafe extern "C" fn(
    in_user_data: *mut c_void,
    in_aq: AudioQueueRef,
    in_buffer: *mut AudioQueueBuffer,
    in_start_time: *const AudioTimeStamp,
    in_number_packet_descriptions: u32,
    in_packet_descs: *const AudioStreamPacketDescription,
);

#[link(name = "AudioToolbox", kind = "framework")]
extern "C" {
    fn AudioConverterNew(
        in_source_format: *const AudioStreamBasicDescription,
        in_destination_format: *const AudioStreamBasicDescription,
        out_audio_converter: *mut AudioConverterRef,
    ) -> OSStatus;
    fn AudioConverterDispose(in_audio_converter: AudioConverterRef) -> OSStatus;
    fn AudioConverterSetProperty(
        in_audio_converter: AudioConverterRef,
        in_property_id: AudioConverterPropertyID,
        in_property_data_size: u32,
        in_property_data: *const c_void,
    ) -> OSStatus;
    fn AudioConverterGetProperty(
        in_audio_converter: AudioConverterRef,
        in_property_id: AudioConverterPropertyID,
        io_property_data_size: *mut u32,
        out_property_data: *mut c_void,
    ) -> OSStatus;
    fn AudioConverterGetPropertyInfo(
        in_audio_converter: AudioConverterRef,
        in_property_id: AudioConverterPropertyID,
        out_size: *mut u32,
        out_writable: *mut u8,
    ) -> OSStatus;
    fn AudioConverterFillComplexBuffer(
        in_audio_converter: AudioConverterRef,
        in_input_data_proc: AudioConverterComplexInputDataProc,
        in_input_data_proc_user_data: *mut c_void,
        io_output_data_packet_size: *mut u32,
        out_output_data: *mut AudioBufferList,
        out_packet_description: *mut AudioStreamPacketDescription,
    ) -> OSStatus;

    fn AudioQueueNewInput(
        in_format: *const AudioStreamBasicDescription,
        in_callback_proc: AudioQueueInputCallback,
        in_user_data: *mut c_void,
        in_callback_run_loop: *mut c_void,
        in_callback_run_loop_mode: *mut c_void,
        in_flags: u32,
        out_aq: *mut AudioQueueRef,
    ) -> OSStatus;
    fn AudioQueueAllocateBuffer(
        in_aq: AudioQueueRef,
        in_buffer_byte_size: u32,
        out_buffer: *mut *mut AudioQueueBuffer,
    ) -> OSStatus;
    fn AudioQueueEnqueueBuffer(
        in_aq: AudioQueueRef,
        in_buffer: *mut AudioQueueBuffer,
        in_num_packet_descs: u32,
        in_packet_descs: *const AudioStreamPacketDescription,
    ) -> OSStatus;
    fn AudioQueueStart(in_aq: AudioQueueRef, in_start_time: *const AudioTimeStamp) -> OSStatus;
    fn AudioQueueStop(in_aq: AudioQueueRef, in_immediate: u8) -> OSStatus;
    fn AudioQueueDispose(in_aq: AudioQueueRef, in_immediate: u8) -> OSStatus;
}

struct InputFeedState {
    pcm: VecDeque<f32>,
    staged: Vec<f32>,
}

unsafe extern "C" fn converter_input_proc(
    _in_audio_converter: AudioConverterRef,
    io_number_data_packets: *mut u32,
    io_data: *mut AudioBufferList,
    _out_data_packet_description: *mut *mut AudioStreamPacketDescription,
    in_user_data: *mut c_void,
) -> OSStatus {
    let state = &mut *(in_user_data as *mut InputFeedState);
    let requested_frames = *io_number_data_packets as usize;
    let avail_frames = state.pcm.len() / AUDIO_CHANNELS;

    if avail_frames == 0 || avail_frames < requested_frames {
        *io_number_data_packets = 0;
        (*io_data).m_buffers[0].m_data_byte_size = 0;
        (*io_data).m_buffers[0].m_data = ptr::null_mut();
        return NEED_MORE_DATA_STATUS;
    }

    let provide_frames = requested_frames;
    let sample_count = provide_frames * AUDIO_CHANNELS;
    state.staged.clear();
    let (front, back) = state.pcm.as_slices();
    if front.len() >= sample_count {
        state.staged.extend_from_slice(&front[..sample_count]);
    } else {
        state.staged.extend_from_slice(front);
        state
            .staged
            .extend_from_slice(&back[..sample_count - front.len()]);
    }
    state.pcm.drain(..sample_count);

    *io_number_data_packets = provide_frames as u32;
    (*io_data).m_buffers[0].m_number_channels = AUDIO_CHANNELS as u32;
    (*io_data).m_buffers[0].m_data_byte_size = (sample_count * mem::size_of::<f32>()) as u32;
    (*io_data).m_buffers[0].m_data = state.staged.as_mut_ptr() as *mut c_void;
    NO_ERR
}

/// AudioToolbox (`AudioConverterRef`) を用いた 48kHz / 2ch AAC-LC ハードウェア/OSエンコーダ
pub struct AudioToolboxAacEncoder {
    converter: AudioConverterRef,
    feed: Box<InputFeedState>,
    out_buffer: Vec<u8>,
    max_output_packet_size: usize,
    output_queue: VecDeque<EncodedPacket>,
    encoded_frames_total: u64,
    first_pts: Option<PtsUs>,
}

unsafe impl Send for AudioToolboxAacEncoder {}

impl AudioToolboxAacEncoder {
    pub fn new(bitrate_bps: u32) -> CoreResult<Self> {
        let src_asbd = AudioStreamBasicDescription {
            m_sample_rate: MASTER_SAMPLE_RATE as f64,
            m_format_id: K_AUDIO_FORMAT_LINEAR_PCM,
            m_format_flags: K_AUDIO_FORMAT_FLAGS_NATIVE_FLOAT_PACKED,
            m_bytes_per_packet: (AUDIO_CHANNELS * mem::size_of::<f32>()) as u32,
            m_frames_per_packet: 1,
            m_bytes_per_frame: (AUDIO_CHANNELS * mem::size_of::<f32>()) as u32,
            m_channels_per_frame: AUDIO_CHANNELS as u32,
            m_bits_per_channel: 32,
            m_reserved: 0,
        };

        let dst_asbd = AudioStreamBasicDescription {
            m_sample_rate: MASTER_SAMPLE_RATE as f64,
            m_format_id: K_AUDIO_FORMAT_MPEG4_AAC,
            m_format_flags: 0, // kMPEG4Object_AAC_LC (default 0 or explicit)
            m_bytes_per_packet: 0,
            m_frames_per_packet: AAC_FRAME_SAMPLES as u32,
            m_bytes_per_frame: 0,
            m_channels_per_frame: AUDIO_CHANNELS as u32,
            m_bits_per_channel: 0,
            m_reserved: 0,
        };

        let mut converter: AudioConverterRef = ptr::null_mut();
        let status = unsafe { AudioConverterNew(&src_asbd, &dst_asbd, &mut converter) };
        if status != NO_ERR || converter.is_null() {
            return Err(CoreError::Codec(format!(
                "AudioConverterNew(AAC-LC) failed with OSStatus {}",
                status
            )));
        }

        let br = bitrate_bps;
        let br_status = unsafe {
            AudioConverterSetProperty(
                converter,
                K_AUDIO_CONVERTER_ENCODE_BIT_RATE,
                mem::size_of::<u32>() as u32,
                &br as *const u32 as *const c_void,
            )
        };
        if br_status != NO_ERR {
            unsafe { AudioConverterDispose(converter) };
            return Err(CoreError::Codec(format!(
                "AudioConverterSetProperty(EncodeBitRate={}) failed with OSStatus {}",
                bitrate_bps, br_status
            )));
        }

        let mut max_pkt_size: u32 = 0;
        let mut prop_size = mem::size_of::<u32>() as u32;
        let max_status = unsafe {
            AudioConverterGetProperty(
                converter,
                K_AUDIO_CONVERTER_PROPERTY_MAX_OUTPUT_PACKET_SIZE,
                &mut prop_size,
                &mut max_pkt_size as *mut u32 as *mut c_void,
            )
        };
        if max_status != NO_ERR || max_pkt_size == 0 {
            max_pkt_size = 4096;
        }

        let asc = Self::read_audio_specific_config(converter);
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
            converter,
            feed: Box::new(InputFeedState {
                pcm: VecDeque::with_capacity(AAC_FRAME_SAMPLES * AUDIO_CHANNELS * 4),
                staged: Vec::with_capacity(AAC_FRAME_SAMPLES * AUDIO_CHANNELS * 2),
            }),
            out_buffer: vec![0u8; max_pkt_size as usize],
            max_output_packet_size: max_pkt_size as usize,
            output_queue,
            encoded_frames_total: 0,
            first_pts: None,
        })
    }

    fn read_audio_specific_config(converter: AudioConverterRef) -> Vec<u8> {
        let mut cookie_size: u32 = 0;
        let info_status = unsafe {
            AudioConverterGetPropertyInfo(
                converter,
                K_AUDIO_CONVERTER_COMPRESSION_MAGIC_COOKIE,
                &mut cookie_size,
                ptr::null_mut(),
            )
        };
        if info_status == NO_ERR && cookie_size > 0 {
            let mut cookie = vec![0u8; cookie_size as usize];
            let get_status = unsafe {
                AudioConverterGetProperty(
                    converter,
                    K_AUDIO_CONVERTER_COMPRESSION_MAGIC_COOKIE,
                    &mut cookie_size,
                    cookie.as_mut_ptr() as *mut c_void,
                )
            };
            if get_status == NO_ERR {
                if let Some(asc) = extract_asc_from_esds(&cookie[..cookie_size as usize]) {
                    return asc;
                }
            }
        }
        // フォールバック: AAC-LC (2), 48kHz (index=3), 2ch -> 0x11 0x90
        vec![0x11, 0x90]
    }

    fn drain_available_aac(&mut self) -> CoreResult<()> {
        loop {
            if self.feed.pcm.len() / AUDIO_CHANNELS < AAC_FRAME_SAMPLES {
                break;
            }

            let mut out_packets: u32 = 1;
            let mut out_desc = AudioStreamPacketDescription::default();
            let mut out_abl = AudioBufferList {
                m_number_buffers: 1,
                m_buffers: [AudioBuffer {
                    m_number_channels: AUDIO_CHANNELS as u32,
                    m_data_byte_size: self.max_output_packet_size as u32,
                    m_data: self.out_buffer.as_mut_ptr() as *mut c_void,
                }],
            };

            let status = unsafe {
                AudioConverterFillComplexBuffer(
                    self.converter,
                    converter_input_proc,
                    (&mut *self.feed as *mut InputFeedState) as *mut c_void,
                    &mut out_packets,
                    &mut out_abl,
                    &mut out_desc,
                )
            };

            if status == NEED_MORE_DATA_STATUS {
                break;
            }
            if status != NO_ERR {
                return Err(CoreError::Codec(format!(
                    "AudioConverterFillComplexBuffer failed with OSStatus {}",
                    status
                )));
            }
            if out_packets == 0 || out_abl.m_buffers[0].m_data_byte_size == 0 {
                break;
            }

            let offset = out_desc.m_start_offset as usize;
            let size = out_desc.m_data_byte_size as usize;
            let data = if size > 0 && offset + size <= self.out_buffer.len() {
                self.out_buffer[offset..offset + size].to_vec()
            } else {
                self.out_buffer[..out_abl.m_buffers[0].m_data_byte_size as usize].to_vec()
            };

            let base_pts = self.first_pts.unwrap_or(PtsUs(0)).0;
            let delta_us = (self.encoded_frames_total as u128 * 1_000_000)
                / (MASTER_SAMPLE_RATE as u128);
            let pts = PtsUs(base_pts + delta_us as u64);
            self.encoded_frames_total += AAC_FRAME_SAMPLES as u64;

            self.output_queue.push_back(EncodedPacket {
                codec: CodecKind::AacLc,
                pts,
                dts: pts,
                is_keyframe: true,
                is_sequence_header: false,
                data,
            });
        }
        Ok(())
    }
}

impl AudioEncoder for AudioToolboxAacEncoder {
    fn push_pcm_f32(&mut self, interleaved_pcm: &[f32], pts: PtsUs) -> CoreResult<()> {
        if self.first_pts.is_none() {
            self.first_pts = Some(pts);
        }
        let valid = (interleaved_pcm.len() / AUDIO_CHANNELS) * AUDIO_CHANNELS;
        self.feed.pcm.extend(&interleaved_pcm[..valid]);
        self.drain_available_aac()
    }

    fn pull_packet(&mut self) -> CoreResult<Option<EncodedPacket>> {
        Ok(self.output_queue.pop_front())
    }
}

impl Drop for AudioToolboxAacEncoder {
    fn drop(&mut self) {
        if !self.converter.is_null() {
            unsafe {
                AudioConverterDispose(self.converter);
            }
            self.converter = ptr::null_mut();
        }
    }
}

/// MPEG-4 ES_Descriptor (`kAudioConverterCompressionMagicCookie`) から
/// DecoderSpecificInfo (tag `0x05`) の `AudioSpecificConfig` バイト列を抽出する
pub fn extract_asc_from_esds(esds: &[u8]) -> Option<Vec<u8>> {
    fn read_desc_len(data: &[u8], pos: &mut usize) -> Option<usize> {
        let mut len = 0usize;
        for _ in 0..4 {
            let b = *data.get(*pos)?;
            *pos += 1;
            len = (len << 7) | ((b & 0x7F) as usize);
            if (b & 0x80) == 0 {
                return Some(len);
            }
        }
        Some(len)
    }

    let mut pos = 0usize;
    while pos < esds.len() {
        let tag = esds[pos];
        pos += 1;
        let len = read_desc_len(esds, &mut pos)?;
        let end = (pos + len).min(esds.len());
        match tag {
            0x03 => {
                // ES_Descriptor: 2 bytes ES_ID + 1 byte flags
                let flags = *esds.get(pos + 2)?;
                pos += 3;
                if (flags & 0x80) != 0 {
                    pos += 2; // dependsOn_ES_ID
                }
                if (flags & 0x40) != 0 {
                    let url_len = *esds.get(pos)? as usize;
                    pos += 1 + url_len;
                }
                if (flags & 0x20) != 0 {
                    pos += 2; // OCR_ES_Id
                }
            }
            0x04 => {
                // DecoderConfigDescriptor: 13 bytes fixed header
                pos += 13;
            }
            0x05 => {
                // DecoderSpecificInfo = AudioSpecificConfig
                return Some(esds[pos..end].to_vec());
            }
            _ => {
                pos = end;
            }
        }
    }
    None
}

struct CaptureContext {
    producer: AudioInputProducer,
}

unsafe extern "C" fn audio_queue_input_cb(
    in_user_data: *mut c_void,
    in_aq: AudioQueueRef,
    in_buffer: *mut AudioQueueBuffer,
    _in_start_time: *const AudioTimeStamp,
    _in_number_packet_descriptions: u32,
    _in_packet_descs: *const AudioStreamPacketDescription,
) {
    if in_user_data.is_null() || in_buffer.is_null() {
        return;
    }
    let ctx_arc = &*(in_user_data as *const Mutex<Option<CaptureContext>>);
    let buf = &*in_buffer;
    if !buf.m_audio_data.is_null() && buf.m_audio_data_byte_size > 0 {
        let sample_count = (buf.m_audio_data_byte_size as usize) / mem::size_of::<f32>();
        let samples = std::slice::from_raw_parts(buf.m_audio_data as *const f32, sample_count);
        if let Ok(mut guard) = ctx_arc.lock() {
            if let Some(ctx) = guard.as_mut() {
                let _ = ctx.producer.push_f32(samples);
            }
        }
    }
    let _ = AudioQueueEnqueueBuffer(in_aq, in_buffer, 0, ptr::null());
}

/// CoreAudio `AudioQueueNewInput` を用いた 48kHz / 2ch f32 マイク/ライン入力キャプチャ
pub struct AudioQueueCapture {
    queue: AudioQueueRef,
    ctx: Arc<Mutex<Option<CaptureContext>>>,
    ctx_raw: *const Mutex<Option<CaptureContext>>,
}

unsafe impl Send for AudioQueueCapture {}

impl AudioQueueCapture {
    pub fn start_default_input(producer: AudioInputProducer) -> CoreResult<Self> {
        let asbd = AudioStreamBasicDescription {
            m_sample_rate: MASTER_SAMPLE_RATE as f64,
            m_format_id: K_AUDIO_FORMAT_LINEAR_PCM,
            m_format_flags: K_AUDIO_FORMAT_FLAGS_NATIVE_FLOAT_PACKED,
            m_bytes_per_packet: (AUDIO_CHANNELS * mem::size_of::<f32>()) as u32,
            m_frames_per_packet: 1,
            m_bytes_per_frame: (AUDIO_CHANNELS * mem::size_of::<f32>()) as u32,
            m_channels_per_frame: AUDIO_CHANNELS as u32,
            m_bits_per_channel: 32,
            m_reserved: 0,
        };

        let ctx = Arc::new(Mutex::new(Some(CaptureContext { producer })));
        let ctx_raw = Arc::into_raw(Arc::clone(&ctx));
        let mut queue: AudioQueueRef = ptr::null_mut();

        let status = unsafe {
            AudioQueueNewInput(
                &asbd,
                audio_queue_input_cb,
                ctx_raw as *mut c_void,
                ptr::null_mut(),
                ptr::null_mut(),
                0,
                &mut queue,
            )
        };
        if status != NO_ERR || queue.is_null() {
            unsafe { drop(Arc::from_raw(ctx_raw)) };
            return Err(CoreError::PlatformIo(format!(
                "AudioQueueNewInput failed with OSStatus {}",
                status
            )));
        }

        // 10ms (480 frames) × 3 バッファを事前確保
        let buf_bytes = (480 * AUDIO_CHANNELS * mem::size_of::<f32>()) as u32;
        for _ in 0..3 {
            let mut buf: *mut AudioQueueBuffer = ptr::null_mut();
            let alloc_st = unsafe { AudioQueueAllocateBuffer(queue, buf_bytes, &mut buf) };
            if alloc_st != NO_ERR || buf.is_null() {
                unsafe {
                    AudioQueueDispose(queue, 1);
                    drop(Arc::from_raw(ctx_raw));
                }
                return Err(CoreError::PlatformIo(format!(
                    "AudioQueueAllocateBuffer failed with OSStatus {}",
                    alloc_st
                )));
            }
            let enq_st = unsafe { AudioQueueEnqueueBuffer(queue, buf, 0, ptr::null()) };
            if enq_st != NO_ERR {
                unsafe {
                    AudioQueueDispose(queue, 1);
                    drop(Arc::from_raw(ctx_raw));
                }
                return Err(CoreError::PlatformIo(format!(
                    "AudioQueueEnqueueBuffer failed with OSStatus {}",
                    enq_st
                )));
            }
        }

        let start_st = unsafe { AudioQueueStart(queue, ptr::null()) };
        if start_st != NO_ERR {
            unsafe {
                AudioQueueDispose(queue, 1);
                drop(Arc::from_raw(ctx_raw));
            }
            return Err(CoreError::PlatformIo(format!(
                "AudioQueueStart failed with OSStatus {}",
                start_st
            )));
        }

        Ok(Self {
            queue,
            ctx,
            ctx_raw,
        })
    }

    pub fn stop(mut self) {
        self.teardown();
    }

    fn teardown(&mut self) {
        if !self.queue.is_null() {
            unsafe {
                AudioQueueStop(self.queue, 1);
                AudioQueueDispose(self.queue, 1);
            }
            self.queue = ptr::null_mut();
            if let Ok(mut guard) = self.ctx.lock() {
                *guard = None;
            }
            unsafe {
                drop(Arc::from_raw(self.ctx_raw));
            }
            self.ctx_raw = ptr::null();
        }
    }
}

impl Drop for AudioQueueCapture {
    fn drop(&mut self) {
        self.teardown();
    }
}
