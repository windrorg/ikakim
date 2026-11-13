//! 純 Rust H.264 Constrained Baseline (I_PCM) ソフトウェアエンコーダ＆デコーダ
//!
//! Linux VM やソフトフォールバック環境で `core-rtmp`（FLV）および
//! `core-net-ui`（fMP4/MSE）がそのまま扱える、ISO/IEC 14496-10 準拠の
//! SPS / PPS / IDR Annex B ストリームを生成・復号する。

use core_types::clock::PtsUs;
use core_types::media::{CodecKind, EncodedPacket, NativeBufferHandle, PixelFormat, VideoFrame};
use core_types::traits::{VideoDecoder, VideoEncoder};
use core_types::{CoreError, CoreResult};
use std::collections::VecDeque;

struct BitWriter {
    bytes: Vec<u8>,
    cur_byte: u8,
    bit_pos: u8,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(256),
            cur_byte: 0,
            bit_pos: 0,
        }
    }

    fn write_bit(&mut self, bit: u8) {
        self.cur_byte = (self.cur_byte << 1) | (bit & 1);
        self.bit_pos += 1;
        if self.bit_pos == 8 {
            self.bytes.push(self.cur_byte);
            self.cur_byte = 0;
            self.bit_pos = 0;
        }
    }

    fn write_bits(&mut self, val: u32, count: u8) {
        for i in (0..count).rev() {
            self.write_bit(((val >> i) & 1) as u8);
        }
    }

    fn write_ue(&mut self, val: u32) {
        let code_num = val + 1;
        let bits = 32 - code_num.leading_zeros();
        for _ in 0..(bits - 1) {
            self.write_bit(0);
        }
        self.write_bits(code_num, bits as u8);
    }

    fn write_se(&mut self, val: i32) {
        let mapped = if val <= 0 {
            (-2 * val) as u32
        } else {
            (2 * val - 1) as u32
        };
        self.write_ue(mapped);
    }

    fn align_zero(&mut self) {
        while self.bit_pos != 0 {
            self.write_bit(0);
        }
    }

    fn write_rbsp_trailing(&mut self) {
        self.write_bit(1);
        self.align_zero();
    }

    fn write_byte_aligned(&mut self, b: u8) {
        debug_assert_eq!(self.bit_pos, 0);
        self.bytes.push(b);
    }

    fn into_annex_b_nal(self, nal_header: u8) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.bytes.len() + 16);
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, nal_header]);
        let mut zeros = 0u8;
        for b in self.bytes {
            if zeros >= 2 && b <= 0x03 {
                out.push(0x03);
                zeros = 0;
            }
            out.push(b);
            if b == 0 {
                zeros += 1;
            } else {
                zeros = 0;
            }
        }
        out
    }
}

struct BitReader<'a> {
    rbsp: &'a [u8],
    byte_pos: usize,
    bit_pos: u8,
}

impl<'a> BitReader<'a> {
    fn new(rbsp: &'a [u8]) -> Self {
        Self {
            rbsp,
            byte_pos: 0,
            bit_pos: 0,
        }
    }

    fn read_bit(&mut self) -> CoreResult<u8> {
        if self.byte_pos >= self.rbsp.len() {
            return Err(CoreError::Codec("unexpected end of H.264 RBSP".into()));
        }
        let b = (self.rbsp[self.byte_pos] >> (7 - self.bit_pos)) & 1;
        self.bit_pos += 1;
        if self.bit_pos == 8 {
            self.bit_pos = 0;
            self.byte_pos += 1;
        }
        Ok(b)
    }

    fn read_bits(&mut self, count: u8) -> CoreResult<u32> {
        let mut v = 0u32;
        for _ in 0..count {
            v = (v << 1) | (self.read_bit()? as u32);
        }
        Ok(v)
    }

    fn read_ue(&mut self) -> CoreResult<u32> {
        let mut zeros = 0u8;
        while self.read_bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err(CoreError::Codec("invalid H.264 exp-golomb".into()));
            }
        }
        if zeros == 0 {
            return Ok(0);
        }
        let rest = self.read_bits(zeros)?;
        Ok(((1u32 << zeros) - 1) + rest)
    }

    fn read_se(&mut self) -> CoreResult<i32> {
        let u = self.read_ue()? as i32;
        Ok(if u % 2 == 0 { -(u / 2) } else { (u + 1) / 2 })
    }

    fn align_byte(&mut self) {
        if self.bit_pos != 0 {
            self.bit_pos = 0;
            self.byte_pos += 1;
        }
    }

    fn read_byte_aligned(&mut self) -> CoreResult<u8> {
        debug_assert_eq!(self.bit_pos, 0);
        if self.byte_pos >= self.rbsp.len() {
            return Err(CoreError::Codec("unexpected EOF reading I_PCM bytes".into()));
        }
        let b = self.rbsp[self.byte_pos];
        self.byte_pos += 1;
        Ok(b)
    }
}

/// 純 Rust ソフトウェア H.264 エンコーダ（VM / フォールバック用 `VideoEncoder` 実装）
pub struct SwH264Encoder {
    width: u32,
    height: u32,
    coded_w: u32,
    coded_h: u32,
    target_bitrate_bps: u32,
    frame_idx: u32,
    seq_header_emitted: bool,
    force_keyframe: bool,
    queue: VecDeque<EncodedPacket>,
}

impl SwH264Encoder {
    pub fn new(width: u32, height: u32, target_bitrate_bps: u32) -> Self {
        let w = width.max(16);
        let h = height.max(16);
        let coded_w = w.div_ceil(16) * 16;
        let coded_h = h.div_ceil(16) * 16;
        Self {
            width: w,
            height: h,
            coded_w,
            coded_h,
            target_bitrate_bps,
            frame_idx: 0,
            seq_header_emitted: false,
            force_keyframe: true,
            queue: VecDeque::new(),
        }
    }

    pub fn target_bitrate_bps(&self) -> u32 {
        self.target_bitrate_bps
    }

    /// SPS + PPS シーケンスヘッダ（Annex B）を構築する
    pub fn build_sequence_header(&self) -> Vec<u8> {
        let mb_w = self.coded_w / 16;
        let mb_h = self.coded_h / 16;

        let mut sps = BitWriter::new();
        sps.write_bits(66, 8); // Baseline profile
        sps.write_bits(0xC0, 8); // constraint_set0 | constraint_set1
        sps.write_bits(31, 8); // Level 3.1
        sps.write_ue(0); // seq_parameter_set_id
        sps.write_ue(0); // log2_max_frame_num_minus4
        sps.write_ue(2); // pic_order_cnt_type = 2
        sps.write_ue(1); // max_num_ref_frames
        sps.write_bit(0); // gaps_in_frame_num_value_allowed_flag
        sps.write_ue(mb_w - 1);
        sps.write_ue(mb_h - 1);
        sps.write_bit(1); // frame_mbs_only_flag
        sps.write_bit(0); // direct_8x8_inference_flag
        if self.coded_w != self.width || self.coded_h != self.height {
            sps.write_bit(1); // frame_cropping_flag
            sps.write_ue(0);
            sps.write_ue((self.coded_w - self.width) / 2);
            sps.write_ue(0);
            sps.write_ue((self.coded_h - self.height) / 2);
        } else {
            sps.write_bit(0);
        }
        sps.write_bit(0); // vui_parameters_present_flag
        sps.write_rbsp_trailing();

        let mut pps = BitWriter::new();
        pps.write_ue(0); // pic_parameter_set_id
        pps.write_ue(0); // seq_parameter_set_id
        pps.write_bit(0); // entropy_coding_mode_flag (CAVLC)
        pps.write_bit(0); // bottom_field_pic_order_in_frame_present_flag
        pps.write_ue(0); // num_slice_groups_minus1
        pps.write_ue(0); // num_ref_idx_l0_default_active_minus1
        pps.write_ue(0); // num_ref_idx_l1_default_active_minus1
        pps.write_bit(0); // weighted_pred_flag
        pps.write_bits(0, 2); // weighted_bipred_idc
        pps.write_se(0); // pic_init_qp_minus26
        pps.write_se(0); // pic_init_qs_minus26
        pps.write_se(0); // chroma_qp_index_offset
        pps.write_bit(0); // deblocking_filter_control_present_flag
        pps.write_bit(0); // constrained_intra_pred_flag
        pps.write_bit(0); // redundant_pic_cnt_present_flag
        pps.write_rbsp_trailing();

        let mut out = sps.into_annex_b_nal(0x67);
        out.extend_from_slice(&pps.into_annex_b_nal(0x68));
        out
    }

    /// RGBA8 フレームを NV12 相当の BT.601 YUV へ変換し H.264 IDR としてエンコードする
    pub fn encode_rgba(&mut self, rgba: &[u8], pts: PtsUs) -> CoreResult<()> {
        let expected = (self.width as usize) * (self.height as usize) * 4;
        if rgba.len() < expected {
            return Err(CoreError::UnsupportedFormat("rgba frame too small".into()));
        }
        let mut y_plane = vec![16u8; (self.coded_w * self.coded_h) as usize];
        let mut u_plane = vec![128u8; ((self.coded_w / 2) * (self.coded_h / 2)) as usize];
        let mut v_plane = vec![128u8; ((self.coded_w / 2) * (self.coded_h / 2)) as usize];

        for row in 0..self.height as usize {
            for col in 0..self.width as usize {
                let idx = (row * self.width as usize + col) * 4;
                let r = rgba[idx] as f32;
                let g = rgba[idx + 1] as f32;
                let b = rgba[idx + 2] as f32;
                let y = (0.257 * r + 0.504 * g + 0.098 * b + 16.0).clamp(16.0, 235.0) as u8;
                y_plane[row * self.coded_w as usize + col] = y;
                if row % 2 == 0 && col % 2 == 0 {
                    let u = (-0.148 * r - 0.291 * g + 0.439 * b + 128.0).clamp(16.0, 240.0) as u8;
                    let v = (0.439 * r - 0.368 * g - 0.071 * b + 128.0).clamp(16.0, 240.0) as u8;
                    let c_idx = (row / 2) * (self.coded_w as usize / 2) + (col / 2);
                    u_plane[c_idx] = u;
                    v_plane[c_idx] = v;
                }
            }
        }
        self.encode_yuv420_planes(&y_plane, &u_plane, &v_plane, pts)
    }

    /// YUV420 (NV12/I420) プレーンを H.264 Constrained Baseline IDR (`I_PCM`) NAL へエンコード
    pub fn encode_yuv420_planes(
        &mut self,
        y_plane: &[u8],
        u_plane: &[u8],
        v_plane: &[u8],
        pts: PtsUs,
    ) -> CoreResult<()> {
        if !self.seq_header_emitted {
            self.seq_header_emitted = true;
            self.queue.push_back(EncodedPacket {
                codec: CodecKind::H264,
                pts,
                dts: pts,
                is_keyframe: true,
                is_sequence_header: true,
                data: self.build_sequence_header(),
            });
        }

        let mb_w = self.coded_w / 16;
        let mb_h = self.coded_h / 16;
        let mut bw = BitWriter::new();
        bw.write_ue(0); // first_mb_in_slice
        bw.write_ue(7); // slice_type = I (all I)
        bw.write_ue(0); // pic_parameter_set_id
        bw.write_bits(0, 4); // frame_num (IDR MUST be 0)
        bw.write_ue(self.frame_idx & 0x7FFF); // idr_pic_id
        bw.write_bit(0); // no_output_of_prior_pics_flag
        bw.write_bit(0); // long_term_reference_flag
        bw.write_se(0); // slice_qp_delta

        for mby in 0..mb_h {
            for mbx in 0..mb_w {
                bw.write_ue(25); // I_PCM
                bw.align_zero();
                // 16x16 Luma
                for dy in 0..16 {
                    let row = (mby * 16 + dy) as usize;
                    for dx in 0..16 {
                        let col = (mbx * 16 + dx) as usize;
                        let val = *y_plane.get(row * self.coded_w as usize + col).unwrap_or(&16);
                        bw.write_byte_aligned(val);
                    }
                }
                // 8x8 Cb
                for dy in 0..8 {
                    let row = (mby * 8 + dy) as usize;
                    for dx in 0..8 {
                        let col = (mbx * 8 + dx) as usize;
                        let val = *u_plane
                            .get(row * (self.coded_w as usize / 2) + col)
                            .unwrap_or(&128);
                        bw.write_byte_aligned(val);
                    }
                }
                // 8x8 Cr
                for dy in 0..8 {
                    let row = (mby * 8 + dy) as usize;
                    for dx in 0..8 {
                        let col = (mbx * 8 + dx) as usize;
                        let val = *v_plane
                            .get(row * (self.coded_w as usize / 2) + col)
                            .unwrap_or(&128);
                        bw.write_byte_aligned(val);
                    }
                }
            }
        }
        bw.write_rbsp_trailing();

        let _was_forced = self.force_keyframe;
        self.force_keyframe = false;
        self.frame_idx = self.frame_idx.wrapping_add(1);

        self.queue.push_back(EncodedPacket {
            codec: CodecKind::H264,
            pts,
            dts: pts,
            is_keyframe: true,
            is_sequence_header: false,
            data: bw.into_annex_b_nal(0x65),
        });
        Ok(())
    }
}

impl VideoEncoder for SwH264Encoder {
    fn set_target_bitrate(&mut self, bitrate_bps: u32) -> CoreResult<()> {
        self.target_bitrate_bps = bitrate_bps;
        Ok(())
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn pull_packet(&mut self) -> CoreResult<Option<EncodedPacket>> {
        Ok(self.queue.pop_front())
    }
}

/// 純 Rust ソフトウェア H.264 (I_PCM) デコーダ（VM / フォールバック用 `VideoDecoder` 実装）
pub struct SwH264Decoder {
    width: u32,
    height: u32,
    coded_w: u32,
    coded_h: u32,
    frames: VecDeque<VideoFrame>,
}

impl SwH264Decoder {
    pub fn new() -> Self {
        Self {
            width: 0,
            height: 0,
            coded_w: 0,
            coded_h: 0,
            frames: VecDeque::new(),
        }
    }

    fn parse_sps(&mut self, rbsp: &[u8]) -> CoreResult<()> {
        let mut br = BitReader::new(rbsp);
        let _profile = br.read_bits(8)?;
        let _compat = br.read_bits(8)?;
        let _level = br.read_bits(8)?;
        let _sps_id = br.read_ue()?;
        let _log2_max_fn = br.read_ue()?;
        let poc_type = br.read_ue()?;
        if poc_type == 0 {
            let _ = br.read_ue()?;
        }
        let _max_refs = br.read_ue()?;
        let _gaps = br.read_bit()?;
        let mb_w = br.read_ue()? + 1;
        let mb_h = br.read_ue()? + 1;
        let _frame_mbs_only = br.read_bit()?;
        let _direct = br.read_bit()?;
        let crop_flag = br.read_bit()?;
        let mut crop_right = 0u32;
        let mut crop_bottom = 0u32;
        if crop_flag == 1 {
            let _left = br.read_ue()?;
            crop_right = br.read_ue()? * 2;
            let _top = br.read_ue()?;
            crop_bottom = br.read_ue()? * 2;
        }
        self.coded_w = mb_w * 16;
        self.coded_h = mb_h * 16;
        self.width = self.coded_w.saturating_sub(crop_right);
        self.height = self.coded_h.saturating_sub(crop_bottom);
        Ok(())
    }

    fn decode_idr_slice(&mut self, rbsp: &[u8], pts: PtsUs) -> CoreResult<VideoFrame> {
        if self.coded_w == 0 || self.coded_h == 0 {
            return Err(CoreError::Codec("IDR received before SPS".into()));
        }
        let mut br = BitReader::new(rbsp);
        let _first_mb = br.read_ue()?;
        let _slice_type = br.read_ue()?;
        let _pps_id = br.read_ue()?;
        let _frame_num = br.read_bits(4)?;
        let _idr_pic_id = br.read_ue()?;
        let _no_out = br.read_bit()?;
        let _long_term = br.read_bit()?;
        let _qp_delta = br.read_se()?;

        let mb_w = self.coded_w / 16;
        let mb_h = self.coded_h / 16;
        let y_size = (self.width * self.height) as usize;
        let uv_size = ((self.width / 2) * (self.height / 2) * 2) as usize;
        let mut nv12 = vec![0u8; y_size + uv_size];

        for mby in 0..mb_h {
            for mbx in 0..mb_w {
                let mb_type = br.read_ue()?;
                if mb_type != 25 {
                    return Err(CoreError::UnsupportedFormat(format!(
                        "SwH264Decoder supports I_PCM (25) slices, got mb_type {}",
                        mb_type
                    )));
                }
                br.align_byte();
                for dy in 0..16 {
                    let row = mby * 16 + dy;
                    for dx in 0..16 {
                        let col = mbx * 16 + dx;
                        let val = br.read_byte_aligned()?;
                        if row < self.height && col < self.width {
                            nv12[(row * self.width + col) as usize] = val;
                        }
                    }
                }
                for dy in 0..8 {
                    let row = mby * 8 + dy;
                    for dx in 0..8 {
                        let col = mbx * 8 + dx;
                        let cb = br.read_byte_aligned()?;
                        if row < self.height / 2 && col < self.width / 2 {
                            let idx = y_size + ((row * (self.width / 2) + col) * 2) as usize;
                            nv12[idx] = cb;
                        }
                    }
                }
                for dy in 0..8 {
                    let row = mby * 8 + dy;
                    for dx in 0..8 {
                        let col = mbx * 8 + dx;
                        let cr = br.read_byte_aligned()?;
                        if row < self.height / 2 && col < self.width / 2 {
                            let idx = y_size + ((row * (self.width / 2) + col) * 2 + 1) as usize;
                            nv12[idx] = cr;
                        }
                    }
                }
            }
        }

        Ok(VideoFrame {
            width: self.width,
            height: self.height,
            format: PixelFormat::Nv12,
            pts,
            handle: NativeBufferHandle::CpuFallback {
                data: nv12,
                stride: self.width,
            },
        })
    }
}

impl Default for SwH264Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoDecoder for SwH264Decoder {
    fn push_packet(&mut self, packet: &EncodedPacket) -> CoreResult<()> {
        for nal in split_annex_b(&packet.data) {
            if nal.is_empty() {
                continue;
            }
            let nal_type = nal[0] & 0x1F;
            let rbsp = unescape_rbsp(&nal[1..]);
            match nal_type {
                7 => self.parse_sps(&rbsp)?,
                5 => {
                    let frame = self.decode_idr_slice(&rbsp, packet.pts)?;
                    self.frames.push_back(frame);
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn pull_frame(&mut self) -> CoreResult<Option<VideoFrame>> {
        Ok(self.frames.pop_front())
    }
}

fn split_annex_b(data: &[u8]) -> Vec<&[u8]> {
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

fn unescape_rbsp(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0u8;
    for &b in data {
        if zeros >= 2 && b == 0x03 {
            zeros = 0;
            continue;
        }
        out.push(b);
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
    }
    out
}
