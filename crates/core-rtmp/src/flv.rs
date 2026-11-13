use crate::amf0::{encode_values, Amf0Value};
use crate::client::StreamMetadata;
use core_types::media::{CodecKind, EncodedPacket};
use core_types::{CoreError, CoreResult};

/// FLV VideoTagHeader + AVC パケットの生成
/// `packet.is_sequence_header == true` の場合は `AVCDecoderConfigurationRecord` を格納、
/// 通常フレームの場合は 4バイト長プレフィックス付き AVCC NAL ユニット群を格納する。
/// （入力が Annex-B `00 00 00 01` スタートコード形式の場合は自動で AVCC へ変換する）
pub fn pack_video_tag(packet: &EncodedPacket) -> CoreResult<Vec<u8>> {
    if packet.codec != CodecKind::H264 {
        return Err(CoreError::UnsupportedFormat(format!(
            "RTMP FLV video packer currently expects H.264, got {:?}",
            packet.codec
        )));
    }

    if packet.is_sequence_header {
        // AVCDecoderConfigurationRecord がすでに渡された場合はそのまま使用、
        // Annex-B (SPS+PPS) が渡された場合は変換する
        let config_record = if looks_like_annexb(&packet.data) {
            build_avc_decoder_config_from_annexb(&packet.data)?
        } else {
            packet.data.clone()
        };

        let mut out = Vec::with_capacity(5 + config_record.len());
        out.push(0x17); // Keyframe (1) + AVC (7)
        out.push(0x00); // AVC sequence header
        out.extend_from_slice(&[0x00, 0x00, 0x00]); // CompositionTime = 0
        out.extend_from_slice(&config_record);
        return Ok(out);
    }

    let avcc_payload = if looks_like_annexb(&packet.data) {
        annexb_to_avcc(&packet.data)
    } else {
        packet.data.clone()
    };

    let pts_ms = packet.pts.0 / 1_000;
    let dts_ms = packet.dts.0 / 1_000;
    let cts_ms = pts_ms.saturating_sub(dts_ms).min(0xFF_FFFF) as i32;

    let frame_type: u8 = if packet.is_keyframe { 0x10 } else { 0x20 };
    let mut out = Vec::with_capacity(5 + avcc_payload.len());
    out.push(frame_type | 0x07); // AVC codec ID = 7
    out.push(0x01); // AVC NALU
    out.push(((cts_ms >> 16) & 0xFF) as u8);
    out.push(((cts_ms >> 8) & 0xFF) as u8);
    out.push((cts_ms & 0xFF) as u8);
    out.extend_from_slice(&avcc_payload);
    Ok(out)
}

/// FLV AudioTagHeader + AAC パケットの生成
pub fn pack_audio_tag(packet: &EncodedPacket) -> CoreResult<Vec<u8>> {
    if packet.codec != CodecKind::AacLc {
        return Err(CoreError::UnsupportedFormat(format!(
            "RTMP FLV audio packer expects AAC-LC, got {:?}",
            packet.codec
        )));
    }

    let mut out = Vec::with_capacity(2 + packet.data.len());
    // SoundFormat=10 (AAC), SoundRate=3 (44kHz marker per FLV spec), SoundSize=1 (16-bit), SoundType=1 (Stereo) -> 0xAF
    out.push(0xAF);
    out.push(if packet.is_sequence_header { 0x00 } else { 0x01 });
    out.extend_from_slice(&packet.data);
    Ok(out)
}

/// `@setDataFrame` + `onMetaData` AMF0 ペイロードの構築
pub fn build_metadata_payload(meta: &StreamMetadata) -> Vec<u8> {
    let props = vec![
        ("width".to_string(), Amf0Value::Number(meta.width as f64)),
        ("height".to_string(), Amf0Value::Number(meta.height as f64)),
        ("framerate".to_string(), Amf0Value::Number(meta.fps)),
        ("videocodecid".to_string(), Amf0Value::Number(7.0)), // H.264 / AVC
        (
            "videodatarate".to_string(),
            Amf0Value::Number((meta.video_bitrate_bps / 1000) as f64),
        ),
        ("audiocodecid".to_string(), Amf0Value::Number(10.0)), // AAC
        (
            "audiodatarate".to_string(),
            Amf0Value::Number((meta.audio_bitrate_bps / 1000) as f64),
        ),
        ("audiosamplerate".to_string(), Amf0Value::Number(48_000.0)),
        ("audiosamplesize".to_string(), Amf0Value::Number(16.0)),
        ("stereo".to_string(), Amf0Value::Boolean(true)),
        ("encoder".to_string(), Amf0Value::String("ikakim".to_string())),
    ];

    encode_values(&[
        Amf0Value::String("@setDataFrame".to_string()),
        Amf0Value::String("onMetaData".to_string()),
        Amf0Value::EcmaArray(props),
    ])
}

pub fn looks_like_annexb(data: &[u8]) -> bool {
    data.starts_with(&[0x00, 0x00, 0x00, 0x01]) || data.starts_with(&[0x00, 0x00, 0x01])
}

/// Annex-B スタートコード (`00 00 01` / `00 00 00 01`) で区切られた NAL ユニット群を抽出する
pub fn split_annexb_nalus(data: &[u8]) -> Vec<&[u8]> {
    let mut nalus = Vec::new();
    let mut i = 0;
    let mut nalu_start = None;

    while i < data.len() {
        let sc_len = if i + 4 <= data.len() && data[i..i + 4] == [0x00, 0x00, 0x00, 0x01] {
            4
        } else if i + 3 <= data.len() && data[i..i + 3] == [0x00, 0x00, 0x01] {
            3
        } else {
            0
        };

        if sc_len > 0 {
            if let Some(start) = nalu_start {
                if i > start {
                    nalus.push(&data[start..i]);
                }
            }
            i += sc_len;
            nalu_start = Some(i);
        } else {
            i += 1;
        }
    }

    if let Some(start) = nalu_start {
        if start < data.len() {
            nalus.push(&data[start..]);
        }
    }
    nalus
}

/// Annex-B NALU 列を 4バイト長プレフィックス付き AVCC 列へ変換する（SPS/PPS を除くスライス・AUD 等をそのまま変換）
pub fn annexb_to_avcc(annexb: &[u8]) -> Vec<u8> {
    let nalus = split_annexb_nalus(annexb);
    let total: usize = nalus.iter().map(|n| 4 + n.len()).sum();
    let mut out = Vec::with_capacity(total);
    for nalu in nalus {
        out.extend_from_slice(&(nalu.len() as u32).to_be_bytes());
        out.extend_from_slice(nalu);
    }
    out
}

/// Annex-B の SPS/PPS を含むバッファから `AVCDecoderConfigurationRecord` を生成する
pub fn build_avc_decoder_config_from_annexb(annexb: &[u8]) -> CoreResult<Vec<u8>> {
    let nalus = split_annexb_nalus(annexb);
    let mut sps_opt: Option<&[u8]> = None;
    let mut pps_opt: Option<&[u8]> = None;

    for nalu in nalus {
        if nalu.is_empty() {
            continue;
        }
        match nalu[0] & 0x1F {
            7 if sps_opt.is_none() => sps_opt = Some(nalu),
            8 if pps_opt.is_none() => pps_opt = Some(nalu),
            _ => {}
        }
    }

    let sps = sps_opt.ok_or_else(|| CoreError::Codec("missing H.264 SPS in sequence header".into()))?;
    let pps = pps_opt.ok_or_else(|| CoreError::Codec("missing H.264 PPS in sequence header".into()))?;
    build_avc_decoder_config(sps, pps)
}

/// 生の SPS と PPS バイト列から `AVCDecoderConfigurationRecord` を組み立てる
pub fn build_avc_decoder_config(sps: &[u8], pps: &[u8]) -> CoreResult<Vec<u8>> {
    if sps.len() < 4 || pps.is_empty() {
        return Err(CoreError::Codec("invalid SPS/PPS length".into()));
    }
    let mut out = Vec::with_capacity(11 + sps.len() + pps.len());
    out.push(0x01); // configurationVersion
    out.push(sps[1]); // AVCProfileIndication
    out.push(sps[2]); // profile_compatibility
    out.push(sps[3]); // AVCLevelIndication
    out.push(0xFF); // lengthSizeMinusOne = 3 (4-byte NALU length)
    out.push(0xE1); // numOfSequenceParameterSets = 1 (0xE0 | 1)
    out.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    out.extend_from_slice(sps);
    out.push(0x01); // numOfPictureParameterSets = 1
    out.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    out.extend_from_slice(pps);
    Ok(out)
}
