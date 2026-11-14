//! Linux 実機ハードウェアエンコーダ自動選択ファクトリ (RK3588 MPP -> Amlogic/V4L2 M2M H264 -> SwH264)

use crate::rockchip_mpp::{MppCapability, MppH264Encoder};
use crate::v4l2_m2m::{list_m2m_codecs, V4l2M2mEncoder, V4L2_FOURCC_H264};
use core_engine::pipeline::EngineControlBridge;
use core_engine::SwH264Encoder;
use core_gpu::GpuCompositor;
use core_rtmp::AimdAbrController;
use core_types::clock::PtsUs;
use core_types::media::{EncodedPacket, PixelFormat};
use core_types::traits::VideoEncoder;
use core_types::{CoreError, CoreResult};
use std::os::fd::RawFd;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecBackendKind {
    RockchipMpp,
    V4l2M2mH264,
    SoftwareFallback,
}

#[derive(Debug, Clone)]
pub struct HardwareProbeReport {
    pub mpp: MppCapability,
    pub v4l2_m2m_h264_encoder: Option<String>,
    pub v4l2_m2m_h264_decoder: Option<String>,
    pub selected_backend: CodecBackendKind,
}

impl HardwareProbeReport {
    pub fn detect() -> Self {
        let mpp = MppCapability::detect();
        let codecs = list_m2m_codecs();
        let enc = codecs
            .iter()
            .find(|c| c.supports_encode_fourcc(V4L2_FOURCC_H264))
            .map(|c| c.path.clone());
        let dec = codecs
            .iter()
            .find(|c| c.supports_decode_fourcc(V4L2_FOURCC_H264))
            .map(|c| c.path.clone());

        let selected_backend = if mpp.is_available() {
            CodecBackendKind::RockchipMpp
        } else if enc.is_some() {
            CodecBackendKind::V4l2M2mH264
        } else {
            CodecBackendKind::SoftwareFallback
        };

        Self {
            mpp,
            v4l2_m2m_h264_encoder: enc,
            v4l2_m2m_h264_decoder: dec,
            selected_backend,
        }
    }
}

/// RK3588 MPP / Amlogic V4L2 M2M / ソフトウェア H.264 統合ラッパー
pub enum LinuxVideoEncoder {
    RockchipMpp(Box<MppH264Encoder>),
    V4l2M2m(Box<V4l2M2mEncoder>),
    Software(Box<SwH264Encoder>),
}

impl LinuxVideoEncoder {
    pub fn open_best_available(width: u32, height: u32, fps: u32, bitrate_bps: u32) -> Self {
        let probe = HardwareProbeReport::detect();
        if probe.mpp.is_available() {
            if let Ok(enc) = MppH264Encoder::open_hardware(width, height, fps, bitrate_bps) {
                return Self::RockchipMpp(Box::new(enc));
            }
        }
        if let Some(ref path) = probe.v4l2_m2m_h264_encoder {
            if let Ok(enc) = V4l2M2mEncoder::open(
                path,
                width,
                height,
                PixelFormat::Nv12,
                V4L2_FOURCC_H264,
                bitrate_bps,
                fps,
            ) {
                return Self::V4l2M2m(Box::new(enc));
            }
        }
        Self::Software(Box::new(SwH264Encoder::new(width, height, bitrate_bps)))
    }

    pub fn backend_kind(&self) -> CodecBackendKind {
        match self {
            Self::RockchipMpp(_) => CodecBackendKind::RockchipMpp,
            Self::V4l2M2m(_) => CodecBackendKind::V4l2M2mH264,
            Self::Software(_) => CodecBackendKind::SoftwareFallback,
        }
    }

    /// ゼロコピー `DMA-BUF` がある場合は直接 HW エンコーダへ投入し、無ければ GPU リードバックフォールバックを使う
    #[allow(clippy::too_many_arguments)]
    pub fn encode_compositor_target(
        &mut self,
        comp: &GpuCompositor,
        target_tex: &wgpu::Texture,
        dmabuf_fd: Option<(RawFd, usize, PixelFormat)>,
        width: u32,
        height: u32,
        pts: PtsUs,
    ) -> CoreResult<()> {
        match self {
            Self::RockchipMpp(enc) => {
                if let Some((fd, sz, fmt)) = dmabuf_fd {
                    enc.encode_dmabuf(fd, sz, width, height, fmt, pts)
                } else {
                    Err(CoreError::Codec(
                        "RockchipMpp requires DMA-BUF fd input for zero-copy encode".into(),
                    ))
                }
            }
            Self::V4l2M2m(enc) => {
                if let Some((fd, sz, _)) = dmabuf_fd {
                    enc.encode_dmabuf(fd, sz, pts)
                } else {
                    let rgba = comp.readback_texture_rgba(target_tex, width, height)?;
                    let nv12 = rgba_to_nv12(&rgba, width as usize, height as usize);
                    enc.encode_raw_slice(&nv12, pts)
                }
            }
            Self::Software(enc) => {
                let rgba = comp.readback_texture_rgba(target_tex, width, height)?;
                enc.encode_rgba(&rgba, pts)
            }
        }
    }
}

impl VideoEncoder for LinuxVideoEncoder {
    fn set_target_bitrate(&mut self, bitrate_bps: u32) -> CoreResult<()> {
        match self {
            Self::RockchipMpp(e) => e.set_target_bitrate(bitrate_bps),
            Self::V4l2M2m(e) => e.set_target_bitrate(bitrate_bps),
            Self::Software(e) => e.set_target_bitrate(bitrate_bps),
        }
    }

    fn request_keyframe(&mut self) {
        match self {
            Self::RockchipMpp(e) => e.request_keyframe(),
            Self::V4l2M2m(e) => e.request_keyframe(),
            Self::Software(e) => e.request_keyframe(),
        }
    }

    fn pull_packet(&mut self) -> CoreResult<Option<EncodedPacket>> {
        match self {
            Self::RockchipMpp(e) => e.pull_packet(),
            Self::V4l2M2m(e) => e.pull_packet(),
            Self::Software(e) => e.pull_packet(),
        }
    }
}

/// `AimdAbrController` のビットレート更新および緊急 IDR 要求を `EngineControlBridge` ＆ エンコーダへ反映する
pub fn sync_abr_step(
    abr: &mut AimdAbrController,
    bridge: &EngineControlBridge,
    encoder: &mut dyn VideoEncoder,
    queued_bytes: usize,
    elapsed_ms: u64,
) -> CoreResult<Option<u32>> {
    let decision = abr.evaluate(queued_bytes, elapsed_ms);
    if decision.request_keyframe {
        encoder.request_keyframe();
    }
    if let Some(new_bps) = decision.new_bitrate_bps {
        bridge.apply_abr_bitrate(new_bps, encoder)?;
        return Ok(Some(new_bps));
    }
    Ok(None)
}

fn rgba_to_nv12(rgba: &[u8], w: usize, h: usize) -> Vec<u8> {
    let y_len = w * h;
    let uv_len = y_len / 2;
    let mut out = vec![0u8; y_len + uv_len];
    let (y_plane, uv_plane) = out.split_at_mut(y_len);
    let row_bytes = w * 4;

    for y_pair in 0..(h / 2) {
        let y0 = y_pair * 2;
        let y1 = y0 + 1;
        let rgba_row0 = &rgba[y0 * row_bytes..(y0 + 1) * row_bytes];
        let rgba_row1 = &rgba[y1 * row_bytes..(y1 + 1) * row_bytes];
        let y_out0 = &mut y_plane[y0 * w..(y0 + 1) * w];
        let y_out1 = &mut y_plane[y1 * w..(y1 + 1) * w];
        let uv_out = &mut uv_plane[y_pair * w..(y_pair + 1) * w];

        for x_pair in 0..(w / 2) {
            let x0 = x_pair * 2;
            let x1 = x0 + 1;
            let p00 = x0 * 4;
            let p01 = x1 * 4;

            let r00 = rgba_row0[p00] as i32;
            let g00 = rgba_row0[p00 + 1] as i32;
            let b00 = rgba_row0[p00 + 2] as i32;
            y_out0[x0] = (((66 * r00 + 129 * g00 + 25 * b00 + 128) >> 8) + 16).clamp(0, 255) as u8;

            let r01 = rgba_row0[p01] as i32;
            let g01 = rgba_row0[p01 + 1] as i32;
            let b01 = rgba_row0[p01 + 2] as i32;
            y_out0[x1] = (((66 * r01 + 129 * g01 + 25 * b01 + 128) >> 8) + 16).clamp(0, 255) as u8;

            let r10 = rgba_row1[p00] as i32;
            let g10 = rgba_row1[p00 + 1] as i32;
            let b10 = rgba_row1[p00 + 2] as i32;
            y_out1[x0] = (((66 * r10 + 129 * g10 + 25 * b10 + 128) >> 8) + 16).clamp(0, 255) as u8;

            let r11 = rgba_row1[p01] as i32;
            let g11 = rgba_row1[p01 + 1] as i32;
            let b11 = rgba_row1[p01 + 2] as i32;
            y_out1[x1] = (((66 * r11 + 129 * g11 + 25 * b11 + 128) >> 8) + 16).clamp(0, 255) as u8;

            let u = ((-38 * r00 - 74 * g00 + 112 * b00 + 128) >> 8) + 128;
            let v = ((112 * r00 - 94 * g00 - 18 * b00 + 128) >> 8) + 128;
            uv_out[x0] = u.clamp(0, 255) as u8;
            uv_out[x1] = v.clamp(0, 255) as u8;
        }
    }
    out
}
