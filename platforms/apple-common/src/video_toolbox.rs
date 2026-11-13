//! VideoToolbox ハードウェア H.264 エンコーダ (`VTCompressionSession`) ＆デコーダ (`VTDecompressionSession`)

use crate::cv_metal::{
    cf_number_i32, kCFAllocatorDefault, kCFBooleanTrue, kCFTypeDictionaryKeyCallBacks,
    kCFTypeDictionaryValueCallBacks, kCVPixelBufferHeightKey, kCVPixelBufferMetalCompatibilityKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey, CFDictionaryCreateMutable,
    CFDictionaryRef, CFDictionarySetValue, CFRelease, CFStringRef, CFTypeRef, CVPixelBufferRef,
    K_CV_PIXEL_FORMAT_NV12_VIDEO,
};
use crate::cv_metal::CVPixelBufferWrapper;
use core_types::clock::PtsUs;
use core_types::media::{CodecKind, EncodedPacket, VideoFrame};
use core_types::traits::{VideoDecoder, VideoEncoder};
use core_types::{CoreError, CoreResult};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr;
use std::sync::{Arc, Mutex};

// CoreFoundation missing CFArrayRef alias re-exported cleanly here
pub mod cf_ext {
    use std::ffi::c_void;
    pub type CFArrayRef = *const c_void;
}
use cf_ext::CFArrayRef as CFArrayHandle;

type OSStatus = i32;
type CMVideoCodecType = u32;
type VTCompressionSessionRef = *mut c_void;
type VTDecompressionSessionRef = *mut c_void;
type CMFormatDescriptionRef = *const c_void;
type CMSampleBufferRef = *mut c_void;
type CMBlockBufferRef = *mut c_void;

const K_CM_VIDEO_CODEC_TYPE_H264: CMVideoCodecType = u32::from_be_bytes(*b"avc1");
const K_CM_TIME_FLAGS_VALID: u32 = 1;
const K_CM_BLOCK_BUFFER_ASSUME_NON_ZERO_LENGTH: usize = 0;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

impl CMTime {
    fn from_pts_us(pts: PtsUs) -> Self {
        Self {
            value: pts.0 as i64,
            timescale: 1_000_000,
            flags: K_CM_TIME_FLAGS_VALID,
            epoch: 0,
        }
    }

    fn invalid() -> Self {
        Self {
            value: 0,
            timescale: 0,
            flags: 0,
            epoch: 0,
        }
    }
}

#[repr(C)]
struct VTDecompressionOutputCallbackRecord {
    decompression_output_callback: extern "C" fn(
        *mut c_void,
        *mut c_void,
        OSStatus,
        u32,
        CVPixelBufferRef,
        CMTime,
        CMTime,
    ),
    decompression_output_ref_con: *mut c_void,
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFArrayGetCount(the_array: CFArrayHandle) -> isize;
    fn CFArrayGetValueAtIndex(the_array: CFArrayHandle, idx: isize) -> *const c_void;
    fn CFDictionaryContainsKey(the_dict: CFDictionaryRef, key: *const c_void) -> u8;
}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    static kCMSampleAttachmentKey_NotSync: CFStringRef;

    fn CMSampleBufferGetFormatDescription(sbuf: CMSampleBufferRef) -> CMFormatDescriptionRef;
    fn CMSampleBufferGetSampleAttachmentsArray(
        sbuf: CMSampleBufferRef,
        create_if_necessary: u8,
    ) -> CFArrayHandle;
    fn CMSampleBufferGetDataBuffer(sbuf: CMSampleBufferRef) -> CMBlockBufferRef;
    fn CMSampleBufferGetPresentationTimeStamp(sbuf: CMSampleBufferRef) -> CMTime;
    fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        video_desc: CMFormatDescriptionRef,
        parameter_set_index: usize,
        parameter_set_pointer_out: *mut *const u8,
        parameter_set_size_out: *mut usize,
        parameter_set_count_out: *mut usize,
        nal_unit_header_length_out: *mut i32,
    ) -> OSStatus;
    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: *const c_void,
        parameter_set_count: usize,
        parameter_set_pointers: *const *const u8,
        parameter_set_sizes: *const usize,
        nal_unit_header_length: i32,
        format_description_out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    fn CMBlockBufferGetDataLength(the_buffer: CMBlockBufferRef) -> usize;
    fn CMBlockBufferCopyDataBytes(
        the_source_buffer: CMBlockBufferRef,
        offset_to_data: usize,
        data_length: usize,
        destination: *mut c_void,
    ) -> OSStatus;
    fn CMBlockBufferCreateWithMemoryBlock(
        structure_allocator: *const c_void,
        memory_block: *mut c_void,
        block_length: usize,
        block_allocator: *const c_void,
        custom_block_source: *const c_void,
        offset_to_data: usize,
        data_length: usize,
        flags: u32,
        block_buffer_out: *mut CMBlockBufferRef,
    ) -> OSStatus;
    fn CMBlockBufferReplaceDataBytes(
        source_bytes: *const c_void,
        destination_buffer: CMBlockBufferRef,
        offset_into_destination: usize,
        data_length: usize,
    ) -> OSStatus;
    fn CMSampleBufferCreateReady(
        allocator: *const c_void,
        data_buffer: CMBlockBufferRef,
        format_description: CMFormatDescriptionRef,
        num_samples: isize,
        num_sample_timing_entries: isize,
        sample_timing_array: *const c_void,
        num_sample_size_entries: isize,
        sample_size_array: *const usize,
        sample_buffer_out: *mut CMSampleBufferRef,
    ) -> OSStatus;
}

#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
    static kVTCompressionPropertyKey_RealTime: CFStringRef;
    static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
    static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
    static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFStringRef;
    static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
    static kVTProfileLevel_H264_Baseline_AutoLevel: CFStringRef;
    static kVTEncodeFrameOptionKey_ForceKeyFrame: CFStringRef;

    fn VTCompressionSessionCreate(
        allocator: *const c_void,
        width: i32,
        height: i32,
        codec_type: CMVideoCodecType,
        encoder_specification: CFDictionaryRef,
        source_image_buffer_attributes: CFDictionaryRef,
        compressed_data_allocator: *const c_void,
        output_callback: extern "C" fn(
            *mut c_void,
            *mut c_void,
            OSStatus,
            u32,
            CMSampleBufferRef,
        ),
        output_callback_ref_con: *mut c_void,
        compression_session_out: *mut VTCompressionSessionRef,
    ) -> OSStatus;
    fn VTCompressionSessionPrepareToEncodeFrames(session: VTCompressionSessionRef) -> OSStatus;
    fn VTCompressionSessionEncodeFrame(
        session: VTCompressionSessionRef,
        image_buffer: CVPixelBufferRef,
        presentation_time_stamp: CMTime,
        duration: CMTime,
        frame_properties: CFDictionaryRef,
        source_frame_ref_con: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OSStatus;
    fn VTCompressionSessionCompleteFrames(
        session: VTCompressionSessionRef,
        complete_until_presentation_time_stamp: CMTime,
    ) -> OSStatus;
    fn VTCompressionSessionInvalidate(session: VTCompressionSessionRef);
    fn VTSessionSetProperty(
        session: *mut c_void,
        property_key: CFStringRef,
        property_value: CFTypeRef,
    ) -> OSStatus;

    fn VTDecompressionSessionCreate(
        allocator: *const c_void,
        video_format_description: CMFormatDescriptionRef,
        video_decoder_specification: CFDictionaryRef,
        destination_image_buffer_attributes: CFDictionaryRef,
        output_callback: *const VTDecompressionOutputCallbackRecord,
        decompression_session_out: *mut VTDecompressionSessionRef,
    ) -> OSStatus;
    fn VTDecompressionSessionDecodeFrame(
        session: VTDecompressionSessionRef,
        sample_buffer: CMSampleBufferRef,
        decode_flags: u32,
        source_frame_ref_con: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OSStatus;
    fn VTDecompressionSessionWaitForAsynchronousFrames(
        session: VTDecompressionSessionRef,
    ) -> OSStatus;
    fn VTDecompressionSessionInvalidate(session: VTDecompressionSessionRef);
}

#[derive(Default)]
struct EncoderSinkState {
    seq_emitted: bool,
    packets: VecDeque<EncodedPacket>,
}

/// VideoToolbox ハードウェア H.264 エンコーダ（Bフレームなし・低遅延リアルタイム設定）
pub struct VtH264Encoder {
    session: VTCompressionSessionRef,
    state_ptr: *mut Arc<Mutex<EncoderSinkState>>,
    width: u32,
    height: u32,
    fps: u32,
    force_keyframe: bool,
}

unsafe impl Send for VtH264Encoder {}

extern "C" fn vt_encode_callback(
    output_callback_ref_con: *mut c_void,
    _source_frame_ref_con: *mut c_void,
    status: OSStatus,
    _info_flags: u32,
    sample_buffer: CMSampleBufferRef,
) {
    if status != 0 || sample_buffer.is_null() || output_callback_ref_con.is_null() {
        return;
    }
    unsafe {
        let sink = &*(output_callback_ref_con as *const Arc<Mutex<EncoderSinkState>>);
        let pts_cm = CMSampleBufferGetPresentationTimeStamp(sample_buffer);
        let pts_us = if pts_cm.timescale > 0 {
            ((pts_cm.value as i128 * 1_000_000) / (pts_cm.timescale as i128)).max(0) as u64
        } else {
            0
        };
        let pts = PtsUs(pts_us);

        let mut is_keyframe = true;
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sample_buffer, 0);
        if !attachments.is_null() && CFArrayGetCount(attachments) > 0 {
            let dict = CFArrayGetValueAtIndex(attachments, 0) as CFDictionaryRef;
            if !dict.is_null() && CFDictionaryContainsKey(dict, kCMSampleAttachmentKey_NotSync) != 0 {
                is_keyframe = false;
            }
        }

        let mut guard = sink.lock().unwrap();
        if is_keyframe && !guard.seq_emitted {
            let fmt = CMSampleBufferGetFormatDescription(sample_buffer);
            if !fmt.is_null() {
                let mut sps_ptr: *const u8 = ptr::null();
                let mut sps_len = 0usize;
                let mut pps_ptr: *const u8 = ptr::null();
                let mut pps_len = 0usize;
                let mut count = 0usize;
                let mut nal_hdr = 0i32;

                let st_sps = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    fmt,
                    0,
                    &mut sps_ptr,
                    &mut sps_len,
                    &mut count,
                    &mut nal_hdr,
                );
                let st_pps = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    fmt,
                    1,
                    &mut pps_ptr,
                    &mut pps_len,
                    &mut count,
                    &mut nal_hdr,
                );
                if st_sps == 0 && st_pps == 0 && !sps_ptr.is_null() && !pps_ptr.is_null() {
                    let mut seq_annex_b = Vec::with_capacity(8 + sps_len + pps_len);
                    seq_annex_b.extend_from_slice(&[0, 0, 0, 1]);
                    seq_annex_b.extend_from_slice(std::slice::from_raw_parts(sps_ptr, sps_len));
                    seq_annex_b.extend_from_slice(&[0, 0, 0, 1]);
                    seq_annex_b.extend_from_slice(std::slice::from_raw_parts(pps_ptr, pps_len));
                    guard.seq_emitted = true;
                    guard.packets.push_back(EncodedPacket {
                        codec: CodecKind::H264,
                        pts,
                        dts: pts,
                        is_keyframe: true,
                        is_sequence_header: true,
                        data: seq_annex_b,
                    });
                }
            }
        }

        let block = CMSampleBufferGetDataBuffer(sample_buffer);
        if block.is_null() {
            return;
        }
        let total_len = CMBlockBufferGetDataLength(block);
        if total_len < 4 {
            return;
        }
        let mut raw = vec![0u8; total_len];
        if CMBlockBufferCopyDataBytes(block, 0, total_len, raw.as_mut_ptr() as *mut c_void) != 0 {
            return;
        }

        // AVCC (4-byte big-endian length prefix) -> Annex B (00 00 00 01)
        let mut annex_b = Vec::with_capacity(total_len + 16);
        let mut pos = 0usize;
        while pos + 4 <= raw.len() {
            let nalu_len = u32::from_be_bytes([raw[pos], raw[pos + 1], raw[pos + 2], raw[pos + 3]])
                as usize;
            pos += 4;
            if nalu_len == 0 || pos + nalu_len > raw.len() {
                break;
            }
            annex_b.extend_from_slice(&[0, 0, 0, 1]);
            annex_b.extend_from_slice(&raw[pos..pos + nalu_len]);
            pos += nalu_len;
        }

        if !annex_b.is_empty() {
            guard.packets.push_back(EncodedPacket {
                codec: CodecKind::H264,
                pts,
                dts: pts,
                is_keyframe,
                is_sequence_header: false,
                data: annex_b,
            });
        }
    }
}

impl VtH264Encoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate_bps: u32) -> CoreResult<Self> {
        let sink = Arc::new(Mutex::new(EncoderSinkState::default()));
        let state_ptr = Box::into_raw(Box::new(sink));
        let mut session: VTCompressionSessionRef = ptr::null_mut();

        unsafe {
            let status = VTCompressionSessionCreate(
                kCFAllocatorDefault,
                width as i32,
                height as i32,
                K_CM_VIDEO_CODEC_TYPE_H264,
                ptr::null(),
                ptr::null(),
                kCFAllocatorDefault,
                vt_encode_callback,
                state_ptr as *mut c_void,
                &mut session,
            );
            if status != 0 || session.is_null() {
                drop(Box::from_raw(state_ptr));
                return Err(CoreError::Codec(format!(
                    "VTCompressionSessionCreate failed: {}",
                    status
                )));
            }

            let _ = VTSessionSetProperty(
                session,
                kVTCompressionPropertyKey_RealTime,
                kCFBooleanTrue as CFTypeRef,
            );
            // B フレーム無効化（RTMP / MSE 低遅延プレビュー向け）
            let cf_false = cf_number_i32(0);
            let _ = VTSessionSetProperty(
                session,
                kVTCompressionPropertyKey_AllowFrameReordering,
                cf_false,
            );
            CFRelease(cf_false);

            let _ = VTSessionSetProperty(
                session,
                kVTCompressionPropertyKey_ProfileLevel,
                kVTProfileLevel_H264_Baseline_AutoLevel as CFTypeRef,
            );

            let cf_br = cf_number_i32(bitrate_bps as i32);
            let _ = VTSessionSetProperty(session, kVTCompressionPropertyKey_AverageBitRate, cf_br);
            CFRelease(cf_br);

            let gop = (fps.max(1) * 2) as i32;
            let cf_gop = cf_number_i32(gop);
            let _ = VTSessionSetProperty(
                session,
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                cf_gop,
            );
            CFRelease(cf_gop);

            let prep = VTCompressionSessionPrepareToEncodeFrames(session);
            if prep != 0 {
                VTCompressionSessionInvalidate(session);
                CFRelease(session as CFTypeRef);
                drop(Box::from_raw(state_ptr));
                return Err(CoreError::Codec(format!(
                    "VTCompressionSessionPrepareToEncodeFrames failed: {}",
                    prep
                )));
            }
        }

        Ok(Self {
            session,
            state_ptr,
            width,
            height,
            fps: fps.max(1),
            force_keyframe: true,
        })
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// `CVPixelBufferWrapper` をハードウェア H.264 エンコーダへゼロコピー投入する
    pub fn encode_pixel_buffer(&mut self, pb: &CVPixelBufferWrapper, pts: PtsUs) -> CoreResult<()> {
        unsafe {
            let frame_props = if self.force_keyframe {
                let d = CFDictionaryCreateMutable(
                    kCFAllocatorDefault,
                    1,
                    &kCFTypeDictionaryKeyCallBacks,
                    &kCFTypeDictionaryValueCallBacks,
                );
                CFDictionarySetValue(d, kVTEncodeFrameOptionKey_ForceKeyFrame, kCFBooleanTrue);
                Some(d)
            } else {
                None
            };
            self.force_keyframe = false;

            let dur = CMTime {
                value: 1_000_000 / (self.fps as i64),
                timescale: 1_000_000,
                flags: K_CM_TIME_FLAGS_VALID,
                epoch: 0,
            };
            let mut flags = 0u32;
            let st = VTCompressionSessionEncodeFrame(
                self.session,
                pb.raw(),
                CMTime::from_pts_us(pts),
                dur,
                frame_props.map(|p| p as CFDictionaryRef).unwrap_or(ptr::null()),
                ptr::null_mut(),
                &mut flags,
            );
            if let Some(d) = frame_props {
                CFRelease(d as CFTypeRef);
            }
            if st != 0 {
                return Err(CoreError::Codec(format!(
                    "VTCompressionSessionEncodeFrame failed: {}",
                    st
                )));
            }
        }
        Ok(())
    }

    /// RGBA8 バッファを BGRA `CVPixelBuffer` 経由でハードウェア H.264 エンコードする
    pub fn encode_rgba(&mut self, width: u32, height: u32, rgba: &[u8], pts: PtsUs) -> CoreResult<()> {
        if width != self.width || height != self.height {
            return Err(CoreError::Codec(format!(
                "frame {}x{} != encoder {}x{}",
                width, height, self.width, self.height
            )));
        }
        let mut bgra = vec![0u8; rgba.len()];
        for (src, dst) in rgba.chunks_exact(4).zip(bgra.chunks_exact_mut(4)) {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
            dst[3] = src[3];
        }
        let pb = CVPixelBufferWrapper::new_bgra(width, height, &bgra)?;
        self.encode_pixel_buffer(&pb, pts)
    }

    /// 保留中の非同期エンコードフレームの完了を待機する
    pub fn flush(&mut self) -> CoreResult<()> {
        let st = unsafe { VTCompressionSessionCompleteFrames(self.session, CMTime::invalid()) };
        if st != 0 {
            return Err(CoreError::Codec(format!(
                "VTCompressionSessionCompleteFrames failed: {}",
                st
            )));
        }
        Ok(())
    }
}

impl VideoEncoder for VtH264Encoder {
    fn set_target_bitrate(&mut self, bitrate_bps: u32) -> CoreResult<()> {
        unsafe {
            let cf_br = cf_number_i32(bitrate_bps as i32);
            let st = VTSessionSetProperty(
                self.session,
                kVTCompressionPropertyKey_AverageBitRate,
                cf_br,
            );
            CFRelease(cf_br);
            if st != 0 {
                return Err(CoreError::Codec(format!(
                    "VTSessionSetProperty AverageBitRate failed: {}",
                    st
                )));
            }
        }
        Ok(())
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn pull_packet(&mut self) -> CoreResult<Option<EncodedPacket>> {
        let sink = unsafe { &*self.state_ptr };
        Ok(sink.lock().unwrap().packets.pop_front())
    }
}

impl Drop for VtH264Encoder {
    fn drop(&mut self) {
        unsafe {
            if !self.session.is_null() {
                let _ = VTCompressionSessionCompleteFrames(self.session, CMTime::invalid());
                VTCompressionSessionInvalidate(self.session);
                CFRelease(self.session as CFTypeRef);
                self.session = ptr::null_mut();
            }
            if !self.state_ptr.is_null() {
                drop(Box::from_raw(self.state_ptr));
                self.state_ptr = ptr::null_mut();
            }
        }
    }
}

struct DecoderQueue {
    frames: VecDeque<(CVPixelBufferWrapper, PtsUs)>,
}

/// VideoToolbox ハードウェア H.264 デコーダ（出力は Metal 互換 NV12 `CVPixelBuffer`）
pub struct VtH264Decoder {
    fmt_desc: CMFormatDescriptionRef,
    session: VTDecompressionSessionRef,
    queue_ptr: *mut Arc<Mutex<DecoderQueue>>,
}

unsafe impl Send for VtH264Decoder {}

extern "C" fn vt_decode_callback(
    decompression_output_ref_con: *mut c_void,
    _source_frame_ref_con: *mut c_void,
    status: OSStatus,
    _info_flags: u32,
    image_buffer: CVPixelBufferRef,
    pts_cm: CMTime,
    _duration: CMTime,
) {
    if status != 0 || image_buffer.is_null() || decompression_output_ref_con.is_null() {
        return;
    }
    unsafe {
        if let Ok(wrapped) = CVPixelBufferWrapper::from_retained(image_buffer) {
            let pts_us = if pts_cm.timescale > 0 {
                ((pts_cm.value as i128 * 1_000_000) / (pts_cm.timescale as i128)).max(0) as u64
            } else {
                0
            };
            let q = &*(decompression_output_ref_con as *const Arc<Mutex<DecoderQueue>>);
            q.lock().unwrap().frames.push_back((wrapped, PtsUs(pts_us)));
        }
    }
}

impl VtH264Decoder {
    pub fn new() -> Self {
        let q = Arc::new(Mutex::new(DecoderQueue {
            frames: VecDeque::new(),
        }));
        Self {
            fmt_desc: ptr::null(),
            session: ptr::null_mut(),
            queue_ptr: Box::into_raw(Box::new(q)),
        }
    }

    pub fn pull_pixel_buffer(&mut self) -> Option<(CVPixelBufferWrapper, PtsUs)> {
        let q = unsafe { &*self.queue_ptr };
        q.lock().unwrap().frames.pop_front()
    }

    fn init_session_from_sps_pps(&mut self, sps: &[u8], pps: &[u8]) -> CoreResult<()> {
        unsafe {
            if !self.session.is_null() {
                VTDecompressionSessionInvalidate(self.session);
                CFRelease(self.session as CFTypeRef);
                self.session = ptr::null_mut();
            }
            if !self.fmt_desc.is_null() {
                CFRelease(self.fmt_desc as CFTypeRef);
                self.fmt_desc = ptr::null();
            }

            let ptrs = [sps.as_ptr(), pps.as_ptr()];
            let sizes = [sps.len(), pps.len()];
            let mut fmt: CMFormatDescriptionRef = ptr::null();
            let st = CMVideoFormatDescriptionCreateFromH264ParameterSets(
                kCFAllocatorDefault,
                2,
                ptrs.as_ptr(),
                sizes.as_ptr(),
                4,
                &mut fmt,
            );
            if st != 0 || fmt.is_null() {
                return Err(CoreError::Codec(format!(
                    "CMVideoFormatDescriptionCreateFromH264ParameterSets failed: {}",
                    st
                )));
            }
            self.fmt_desc = fmt;

            let dst_attrs = CFDictionaryCreateMutable(
                kCFAllocatorDefault,
                4,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            );
            let pix_fmt = cf_number_i32(K_CV_PIXEL_FORMAT_NV12_VIDEO as i32);
            CFDictionarySetValue(dst_attrs, kCVPixelBufferPixelFormatTypeKey, pix_fmt);
            CFDictionarySetValue(
                dst_attrs,
                kCVPixelBufferMetalCompatibilityKey,
                kCFBooleanTrue,
            );
            let _ = (&kCVPixelBufferWidthKey, &kCVPixelBufferHeightKey);

            let cb_record = VTDecompressionOutputCallbackRecord {
                decompression_output_callback: vt_decode_callback,
                decompression_output_ref_con: self.queue_ptr as *mut c_void,
            };

            let mut sess: VTDecompressionSessionRef = ptr::null_mut();
            let st_sess = VTDecompressionSessionCreate(
                kCFAllocatorDefault,
                self.fmt_desc,
                ptr::null(),
                dst_attrs as CFDictionaryRef,
                &cb_record,
                &mut sess,
            );
            CFRelease(pix_fmt);
            CFRelease(dst_attrs as CFTypeRef);
            if st_sess != 0 || sess.is_null() {
                return Err(CoreError::Codec(format!(
                    "VTDecompressionSessionCreate failed: {}",
                    st_sess
                )));
            }
            self.session = sess;
        }
        Ok(())
    }
}

impl Default for VtH264Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoDecoder for VtH264Decoder {
    fn push_packet(&mut self, packet: &EncodedPacket) -> CoreResult<()> {
        if packet.codec != CodecKind::H264 {
            return Err(CoreError::UnsupportedFormat("expected H.264 packet".into()));
        }

        let nals = split_annex_b_slice(&packet.data);
        let mut sps: Option<Vec<u8>> = None;
        let mut pps: Option<Vec<u8>> = None;
        let mut avcc = Vec::with_capacity(packet.data.len() + 8);

        for nal in nals {
            if nal.is_empty() {
                continue;
            }
            match nal[0] & 0x1F {
                7 => sps = Some(nal.to_vec()),
                8 => pps = Some(nal.to_vec()),
                9 => {}
                _ => {
                    avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    avcc.extend_from_slice(nal);
                }
            }
        }

        if let (Some(s), Some(p)) = (sps, pps) {
            self.init_session_from_sps_pps(&s, &p)?;
        }

        if avcc.is_empty() {
            return Ok(());
        }
        if self.session.is_null() || self.fmt_desc.is_null() {
            return Err(CoreError::Codec(
                "H.264 slice arrived before SPS/PPS".into(),
            ));
        }

        unsafe {
            let mut block: CMBlockBufferRef = ptr::null_mut();
            let st_blk = CMBlockBufferCreateWithMemoryBlock(
                kCFAllocatorDefault,
                ptr::null_mut(),
                avcc.len(),
                kCFAllocatorDefault,
                ptr::null(),
                0,
                avcc.len(),
                K_CM_BLOCK_BUFFER_ASSUME_NON_ZERO_LENGTH as u32,
                &mut block,
            );
            if st_blk != 0 || block.is_null() {
                return Err(CoreError::Codec(format!(
                    "CMBlockBufferCreateWithMemoryBlock failed: {}",
                    st_blk
                )));
            }
            let st_rep = CMBlockBufferReplaceDataBytes(
                avcc.as_ptr() as *const c_void,
                block,
                0,
                avcc.len(),
            );
            if st_rep != 0 {
                CFRelease(block as CFTypeRef);
                return Err(CoreError::Codec(format!(
                    "CMBlockBufferReplaceDataBytes failed: {}",
                    st_rep
                )));
            }

            let mut sbuf: CMSampleBufferRef = ptr::null_mut();
            let sample_size = avcc.len();
            let st_sb = CMSampleBufferCreateReady(
                kCFAllocatorDefault,
                block,
                self.fmt_desc,
                1,
                0,
                ptr::null(),
                1,
                &sample_size,
                &mut sbuf,
            );
            CFRelease(block as CFTypeRef);
            if st_sb != 0 || sbuf.is_null() {
                return Err(CoreError::Codec(format!(
                    "CMSampleBufferCreateReady failed: {}",
                    st_sb
                )));
            }

            let mut flags = 0u32;
            let st_dec = VTDecompressionSessionDecodeFrame(
                self.session,
                sbuf,
                0,
                ptr::null_mut(),
                &mut flags,
            );
            CFRelease(sbuf as CFTypeRef);
            if st_dec != 0 {
                return Err(CoreError::Codec(format!(
                    "VTDecompressionSessionDecodeFrame failed: {}",
                    st_dec
                )));
            }
            let _ = VTDecompressionSessionWaitForAsynchronousFrames(self.session);
        }
        Ok(())
    }

    fn pull_frame(&mut self) -> CoreResult<Option<VideoFrame>> {
        if let Some((pb, pts)) = self.pull_pixel_buffer() {
            let vf = pb.as_video_frame(pts);
            // Note: ゼロコピー側で CVPixelBufferRef を取り出す場合に参照を保持する
            std::mem::forget(pb);
            Ok(Some(vf))
        } else {
            Ok(None)
        }
    }
}

impl Drop for VtH264Decoder {
    fn drop(&mut self) {
        unsafe {
            if !self.session.is_null() {
                let _ = VTDecompressionSessionWaitForAsynchronousFrames(self.session);
                VTDecompressionSessionInvalidate(self.session);
                CFRelease(self.session as CFTypeRef);
            }
            if !self.fmt_desc.is_null() {
                CFRelease(self.fmt_desc as CFTypeRef);
            }
            if !self.queue_ptr.is_null() {
                drop(Box::from_raw(self.queue_ptr));
            }
        }
    }
}

fn split_annex_b_slice(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut i = 0usize;
    let mut start = None;
    while i < data.len() {
        let sc = if i + 4 <= data.len() && data[i..i + 4] == [0, 0, 0, 1] {
            4
        } else if i + 3 <= data.len() && data[i..i + 3] == [0, 0, 1] {
            3
        } else {
            0
        };
        if sc > 0 {
            if let Some(s) = start {
                if i > s {
                    nals.push(&data[s..i]);
                }
            }
            i += sc;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = start {
        if data.len() > s {
            nals.push(&data[s..]);
        }
    }
    nals
}
