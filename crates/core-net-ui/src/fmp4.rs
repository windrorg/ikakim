//! ゼロ依存 fMP4 (ISO BMFF) ミキサ（MSE 低遅延プレビュー用）
//!
//! Bフレームなし・短GOPの H.264 ストリーム（および任意の AAC-LC 音声）を
//! ブラウザ Media Source Extensions (MSE) が直接再生できる
//! 初期化セグメント (`ftyp` + `moov`) およびメディアフラグメント (`moof` + `mdat`) に変換する。

use core_types::media::{CodecKind, EncodedPacket};
use core_types::{CoreError, CoreResult};

pub const VIDEO_TIMESCALE: u32 = 90_000;
pub const AUDIO_TIMESCALE: u32 = 48_000;

/// H.264 ビデオトラック設定（Init Segment 生成に必要）
#[derive(Debug, Clone)]
pub struct VideoTrackConfig {
    pub width: u32,
    pub height: u32,
    pub default_fps: u32,
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
}

impl VideoTrackConfig {
    /// Annex B あるいは AVCC 形式のシーケンスヘッダから `VideoTrackConfig` を生成する
    pub fn from_h264_seq_header(
        width: u32,
        height: u32,
        default_fps: u32,
        data: &[u8],
    ) -> CoreResult<Self> {
        let (sps, pps) = extract_sps_pps(data)?;
        Ok(Self {
            width,
            height,
            default_fps: default_fps.max(1),
            sps,
            pps,
        })
    }

    /// MSE `MediaSource.addSourceBuffer` に渡す RFC 6381 コーデック文字列（例: `avc1.42E01F`）
    pub fn codec_string(&self) -> String {
        if self.sps.len() >= 4 {
            format!(
                "avc1.{:02X}{:02X}{:02X}",
                self.sps[1], self.sps[2], self.sps[3]
            )
        } else {
            "avc1.42E01E".to_string()
        }
    }
}

/// 任意の AAC-LC 音声トラック設定
#[derive(Debug, Clone)]
pub struct AudioTrackConfig {
    pub sample_rate: u32,
    pub channels: u16,
    pub audio_specific_config: Vec<u8>,
}

/// fMP4 ミキサが保持・出力するセグメント情報
#[derive(Debug, Clone)]
pub struct Fmp4Segment {
    pub is_init: bool,
    pub sequence_number: u32,
    pub contains_keyframe: bool,
    pub bytes: Vec<u8>,
}

/// ゼロ依存 fMP4 / MSE セグメントビルダー
pub struct Fmp4Muxer {
    video_cfg: Option<VideoTrackConfig>,
    audio_cfg: Option<AudioTrackConfig>,
    init_segment: Option<Vec<u8>>,
    sequence_number: u32,
    last_video_pts_us: Option<u64>,
    last_audio_pts_us: Option<u64>,
    pending_audio: Vec<QueuedSample>,
    waiting_for_idr: bool,
}

#[derive(Debug, Clone)]
struct QueuedSample {
    pts_90k_or_48k: u64,
    duration: u32,
    data: Vec<u8>,
}

impl Fmp4Muxer {
    pub fn new() -> Self {
        Self {
            video_cfg: None,
            audio_cfg: None,
            init_segment: None,
            sequence_number: 1,
            last_video_pts_us: None,
            last_audio_pts_us: None,
            pending_audio: Vec::new(),
            waiting_for_idr: true,
        }
    }

    pub fn with_tracks(video: VideoTrackConfig, audio: Option<AudioTrackConfig>) -> Self {
        let mut m = Self::new();
        m.video_cfg = Some(video);
        m.audio_cfg = audio;
        m.rebuild_init_segment();
        m
    }

    /// 現在の MSE MIME タイプを返す
    pub fn mime_type(&self) -> Option<String> {
        let v = self.video_cfg.as_ref()?;
        if self.audio_cfg.is_some() {
            Some(format!("video/mp4; codecs=\"{}, mp4a.40.2\"", v.codec_string()))
        } else {
            Some(format!("video/mp4; codecs=\"{}\"", v.codec_string()))
        }
    }

    /// キャッシュ済み初期化セグメント (`ftyp` + `moov`) を返す
    pub fn init_segment(&self) -> Option<&[u8]> {
        self.init_segment.as_deref()
    }

    /// 映像解像度を設定／更新して Init Segment を初期化する際に使用
    pub fn configure_video(&mut self, cfg: VideoTrackConfig) -> Vec<u8> {
        self.video_cfg = Some(cfg);
        self.waiting_for_idr = true;
        self.rebuild_init_segment();
        self.init_segment.clone().unwrap_or_default()
    }

    /// 音声トラックを設定して Init Segment を更新する
    pub fn configure_audio(&mut self, cfg: AudioTrackConfig) {
        self.audio_cfg = Some(cfg);
        if self.video_cfg.is_some() {
            self.rebuild_init_segment();
        }
    }

    /// `EncodedPacket` を投入し、生成された fMP4 セグメント（Init または Media）を返す。
    /// 低遅延 MSE のため、IDR キーフレームを受信済みであれば各ビデオフレームごとに `moof+mdat` を即時出力する。
    pub fn push_packet(
        &mut self,
        packet: &EncodedPacket,
        fallback_width: u32,
        fallback_height: u32,
        fallback_fps: u32,
    ) -> CoreResult<Option<Fmp4Segment>> {
        if packet.is_sequence_header {
            match packet.codec {
                CodecKind::H264 => {
                    let cfg = VideoTrackConfig::from_h264_seq_header(
                        fallback_width,
                        fallback_height,
                        fallback_fps,
                        &packet.data,
                    )?;
                    let bytes = self.configure_video(cfg);
                    return Ok(Some(Fmp4Segment {
                        is_init: true,
                        sequence_number: 0,
                        contains_keyframe: true,
                        bytes,
                    }));
                }
                CodecKind::AacLc => {
                    self.configure_audio(AudioTrackConfig {
                        sample_rate: AUDIO_TIMESCALE,
                        channels: 2,
                        audio_specific_config: packet.data.clone(),
                    });
                    if let Some(init) = &self.init_segment {
                        return Ok(Some(Fmp4Segment {
                            is_init: true,
                            sequence_number: 0,
                            contains_keyframe: false,
                            bytes: init.clone(),
                        }));
                    }
                    return Ok(None);
                }
                CodecKind::Hevc => {
                    return Err(CoreError::UnsupportedFormat(
                        "fMP4 MSE muxer currently supports H.264/AAC".into(),
                    ))
                }
            }
        }

        match packet.codec {
            CodecKind::AacLc => {
                if self.audio_cfg.is_some() {
                    let pts_ticks = us_to_timescale(packet.pts.0, AUDIO_TIMESCALE);
                    let dur_ticks = if let Some(prev_us) = self.last_audio_pts_us {
                        let delta_us = packet.pts.0.saturating_sub(prev_us).max(1_000);
                        us_to_timescale(delta_us, AUDIO_TIMESCALE).max(1) as u32
                    } else {
                        1024
                    };
                    self.last_audio_pts_us = Some(packet.pts.0);
                    if self.pending_audio.len() >= 128 {
                        self.pending_audio.drain(0..64);
                    }
                    self.pending_audio.push(QueuedSample {
                        pts_90k_or_48k: pts_ticks,
                        duration: dur_ticks,
                        data: packet.data.clone(),
                    });
                }
                Ok(None)
            }
            CodecKind::H264 => {
                let video_cfg = match &self.video_cfg {
                    Some(c) => c,
                    None => {
                        return Err(CoreError::Codec(
                            "video sequence header must precede video frames".into(),
                        ))
                    }
                };

                if self.waiting_for_idr {
                    if !packet.is_keyframe {
                        return Ok(None);
                    }
                    self.waiting_for_idr = false;
                }

                // Annex B / AVCC を 4-byte length-prefixed AVCC へ正規化（SPS/PPS は除く）
                let avcc_payload = normalize_h264_frame_to_avcc(&packet.data)?;
                if avcc_payload.is_empty() {
                    return Ok(None);
                }

                let pts_90k = us_to_timescale(packet.pts.0, VIDEO_TIMESCALE);
                let default_dur = (VIDEO_TIMESCALE / video_cfg.default_fps.max(1)).max(1);
                let dur_90k = if let Some(prev_us) = self.last_video_pts_us {
                    let delta_us = packet.pts.0.saturating_sub(prev_us);
                    if delta_us > 0 {
                        (us_to_timescale(delta_us, VIDEO_TIMESCALE) as u32).max(1)
                    } else {
                        default_dur
                    }
                } else {
                    default_dur
                };
                self.last_video_pts_us = Some(packet.pts.0);

                let audio_batch = std::mem::take(&mut self.pending_audio);
                let seq = self.sequence_number;
                self.sequence_number += 1;

                let bytes = build_media_fragment(
                    seq,
                    pts_90k,
                    dur_90k,
                    packet.is_keyframe,
                    &avcc_payload,
                    &audio_batch,
                );

                Ok(Some(Fmp4Segment {
                    is_init: false,
                    sequence_number: seq,
                    contains_keyframe: packet.is_keyframe,
                    bytes,
                }))
            }
            CodecKind::Hevc => Err(CoreError::UnsupportedFormat(
                "fMP4 MSE muxer currently supports H.264/AAC".into(),
            )),
        }
    }

    fn rebuild_init_segment(&mut self) {
        if let Some(v) = &self.video_cfg {
            self.init_segment = Some(build_init_segment(v, self.audio_cfg.as_ref()));
        }
    }
}

impl Default for Fmp4Muxer {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn us_to_timescale(us: u64, timescale: u32) -> u64 {
    ((us as u128 * timescale as u128) / 1_000_000u128) as u64
}

/// トップレベルおよび内部ボックスを走査し、4-char Box タイプ一覧を返す（デバッグ・テスト用）
pub fn inspect_box_types(mut data: &[u8]) -> Vec<[u8; 4]> {
    let mut out = Vec::new();
    while data.len() >= 8 {
        let size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
        if size < 8 || size > data.len() {
            break;
        }
        out.push([data[4], data[5], data[6], data[7]]);
        data = &data[size..];
    }
    out
}

fn write_box(out: &mut Vec<u8>, tag: &[u8; 4], build: impl FnOnce(&mut Vec<u8>)) {
    let start = out.len();
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(tag);
    build(out);
    let len = (out.len() - start) as u32;
    out[start..start + 4].copy_from_slice(&len.to_be_bytes());
}

fn write_full_box(
    out: &mut Vec<u8>,
    tag: &[u8; 4],
    version: u8,
    flags: u32,
    build: impl FnOnce(&mut Vec<u8>),
) {
    write_box(out, tag, |buf| {
        buf.push(version);
        buf.extend_from_slice(&flags.to_be_bytes()[1..4]);
        build(buf);
    });
}

fn build_init_segment(video: &VideoTrackConfig, audio: Option<&AudioTrackConfig>) -> Vec<u8> {
    let mut out = Vec::with_capacity(1024);

    // 1. ftyp
    write_box(&mut out, b"ftyp", |buf| {
        buf.extend_from_slice(b"isom");
        buf.extend_from_slice(&512u32.to_be_bytes());
        buf.extend_from_slice(b"isom");
        buf.extend_from_slice(b"iso5");
        buf.extend_from_slice(b"iso6");
        buf.extend_from_slice(b"avc1");
        buf.extend_from_slice(b"mp41");
    });

    // 2. moov
    write_box(&mut out, b"moov", |moov| {
        let next_track_id: u32 = if audio.is_some() { 3 } else { 2 };

        // mvhd (version 0)
        write_full_box(moov, b"mvhd", 0, 0, |buf| {
            buf.extend_from_slice(&0u32.to_be_bytes()); // creation_time
            buf.extend_from_slice(&0u32.to_be_bytes()); // modification_time
            buf.extend_from_slice(&1000u32.to_be_bytes()); // timescale
            buf.extend_from_slice(&0u32.to_be_bytes()); // duration (fragmented = 0)
            buf.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
            buf.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
            buf.extend_from_slice(&[0u8; 10]); // reserved
            write_identity_matrix(buf);
            buf.extend_from_slice(&[0u8; 24]); // pre_defined
            buf.extend_from_slice(&next_track_id.to_be_bytes());
        });

        // Video trak (track_id = 1)
        write_video_trak(moov, 1, video);

        // Audio trak (track_id = 2)
        if let Some(a) = audio {
            write_audio_trak(moov, 2, a);
        }

        // mvex
        write_box(moov, b"mvex", |mvex| {
            let default_v_dur = (VIDEO_TIMESCALE / video.default_fps.max(1)).max(1);
            write_trex(mvex, 1, default_v_dur, 0x0001_0000);
            if audio.is_some() {
                write_trex(mvex, 2, 1024, 0x0200_0000);
            }
        });
    });

    out
}

fn write_trex(out: &mut Vec<u8>, track_id: u32, default_duration: u32, default_flags: u32) {
    write_full_box(out, b"trex", 0, 0, |buf| {
        buf.extend_from_slice(&track_id.to_be_bytes());
        buf.extend_from_slice(&1u32.to_be_bytes()); // default_sample_description_index
        buf.extend_from_slice(&default_duration.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes()); // default_sample_size
        buf.extend_from_slice(&default_flags.to_be_bytes());
    });
}

fn write_identity_matrix(buf: &mut Vec<u8>) {
    let matrix = [
        0x0001_0000u32,
        0,
        0,
        0,
        0x0001_0000u32,
        0,
        0,
        0,
        0x4000_0000u32,
    ];
    for m in matrix {
        buf.extend_from_slice(&m.to_be_bytes());
    }
}

fn write_video_trak(moov: &mut Vec<u8>, track_id: u32, video: &VideoTrackConfig) {
    write_box(moov, b"trak", |trak| {
        write_full_box(trak, b"tkhd", 0, 0x0000_0003, |buf| {
            buf.extend_from_slice(&0u32.to_be_bytes());
            buf.extend_from_slice(&0u32.to_be_bytes());
            buf.extend_from_slice(&track_id.to_be_bytes());
            buf.extend_from_slice(&0u32.to_be_bytes());
            buf.extend_from_slice(&0u32.to_be_bytes()); // duration
            buf.extend_from_slice(&[0u8; 8]);
            buf.extend_from_slice(&0u16.to_be_bytes()); // layer
            buf.extend_from_slice(&0u16.to_be_bytes()); // alternate_group
            buf.extend_from_slice(&0u16.to_be_bytes()); // volume
            buf.extend_from_slice(&0u16.to_be_bytes());
            write_identity_matrix(buf);
            buf.extend_from_slice(&(video.width << 16).to_be_bytes());
            buf.extend_from_slice(&(video.height << 16).to_be_bytes());
        });

        write_box(trak, b"mdia", |mdia| {
            write_full_box(mdia, b"mdhd", 0, 0, |buf| {
                buf.extend_from_slice(&0u32.to_be_bytes());
                buf.extend_from_slice(&0u32.to_be_bytes());
                buf.extend_from_slice(&VIDEO_TIMESCALE.to_be_bytes());
                buf.extend_from_slice(&0u32.to_be_bytes());
                buf.extend_from_slice(&0x55C4u16.to_be_bytes()); // und
                buf.extend_from_slice(&0u16.to_be_bytes());
            });
            write_full_box(mdia, b"hdlr", 0, 0, |buf| {
                buf.extend_from_slice(&0u32.to_be_bytes());
                buf.extend_from_slice(b"vide");
                buf.extend_from_slice(&[0u8; 12]);
                buf.extend_from_slice(b"VideoHandler\0");
            });
            write_box(mdia, b"minf", |minf| {
                write_full_box(minf, b"vmhd", 0, 1, |buf| {
                    buf.extend_from_slice(&[0u8; 8]);
                });
                write_dinf(minf);
                write_box(minf, b"stbl", |stbl| {
                    write_full_box(stbl, b"stsd", 0, 0, |stsd| {
                        stsd.extend_from_slice(&1u32.to_be_bytes());
                        write_box(stsd, b"avc1", |avc1| {
                            avc1.extend_from_slice(&[0u8; 6]);
                            avc1.extend_from_slice(&1u16.to_be_bytes()); // data_ref_index
                            avc1.extend_from_slice(&[0u8; 16]);
                            avc1.extend_from_slice(&(video.width as u16).to_be_bytes());
                            avc1.extend_from_slice(&(video.height as u16).to_be_bytes());
                            avc1.extend_from_slice(&0x0048_0000u32.to_be_bytes());
                            avc1.extend_from_slice(&0x0048_0000u32.to_be_bytes());
                            avc1.extend_from_slice(&0u32.to_be_bytes());
                            avc1.extend_from_slice(&1u16.to_be_bytes());
                            avc1.extend_from_slice(&[0u8; 32]);
                            avc1.extend_from_slice(&0x0018u16.to_be_bytes());
                            avc1.extend_from_slice(&(-1i16).to_be_bytes());

                            write_box(avc1, b"avcC", |avcc| {
                                avcc.push(1);
                                avcc.push(*video.sps.get(1).unwrap_or(&0x42));
                                avcc.push(*video.sps.get(2).unwrap_or(&0xE0));
                                avcc.push(*video.sps.get(3).unwrap_or(&0x1E));
                                avcc.push(0xFF); // 4-byte NAL length
                                avcc.push(0xE1); // 1 SPS
                                avcc.extend_from_slice(&(video.sps.len() as u16).to_be_bytes());
                                avcc.extend_from_slice(&video.sps);
                                avcc.push(1); // 1 PPS
                                avcc.extend_from_slice(&(video.pps.len() as u16).to_be_bytes());
                                avcc.extend_from_slice(&video.pps);
                            });
                        });
                    });
                    write_empty_sample_tables(stbl);
                });
            });
        });
    });
}

fn write_audio_trak(moov: &mut Vec<u8>, track_id: u32, audio: &AudioTrackConfig) {
    write_box(moov, b"trak", |trak| {
        write_full_box(trak, b"tkhd", 0, 0x0000_0003, |buf| {
            buf.extend_from_slice(&0u32.to_be_bytes());
            buf.extend_from_slice(&0u32.to_be_bytes());
            buf.extend_from_slice(&track_id.to_be_bytes());
            buf.extend_from_slice(&0u32.to_be_bytes());
            buf.extend_from_slice(&0u32.to_be_bytes());
            buf.extend_from_slice(&[0u8; 8]);
            buf.extend_from_slice(&0u16.to_be_bytes());
            buf.extend_from_slice(&0u16.to_be_bytes());
            buf.extend_from_slice(&0x0100u16.to_be_bytes());
            buf.extend_from_slice(&0u16.to_be_bytes());
            write_identity_matrix(buf);
            buf.extend_from_slice(&0u32.to_be_bytes());
            buf.extend_from_slice(&0u32.to_be_bytes());
        });

        write_box(trak, b"mdia", |mdia| {
            write_full_box(mdia, b"mdhd", 0, 0, |buf| {
                buf.extend_from_slice(&0u32.to_be_bytes());
                buf.extend_from_slice(&0u32.to_be_bytes());
                buf.extend_from_slice(&audio.sample_rate.to_be_bytes());
                buf.extend_from_slice(&0u32.to_be_bytes());
                buf.extend_from_slice(&0x55C4u16.to_be_bytes());
                buf.extend_from_slice(&0u16.to_be_bytes());
            });
            write_full_box(mdia, b"hdlr", 0, 0, |buf| {
                buf.extend_from_slice(&0u32.to_be_bytes());
                buf.extend_from_slice(b"soun");
                buf.extend_from_slice(&[0u8; 12]);
                buf.extend_from_slice(b"SoundHandler\0");
            });
            write_box(mdia, b"minf", |minf| {
                write_full_box(minf, b"smhd", 0, 0, |buf| {
                    buf.extend_from_slice(&[0u8; 4]);
                });
                write_dinf(minf);
                write_box(minf, b"stbl", |stbl| {
                    write_full_box(stbl, b"stsd", 0, 0, |stsd| {
                        stsd.extend_from_slice(&1u32.to_be_bytes());
                        write_box(stsd, b"mp4a", |mp4a| {
                            mp4a.extend_from_slice(&[0u8; 6]);
                            mp4a.extend_from_slice(&1u16.to_be_bytes());
                            mp4a.extend_from_slice(&[0u8; 8]);
                            mp4a.extend_from_slice(&audio.channels.to_be_bytes());
                            mp4a.extend_from_slice(&16u16.to_be_bytes());
                            mp4a.extend_from_slice(&[0u8; 4]);
                            mp4a.extend_from_slice(&(audio.sample_rate << 16).to_be_bytes());

                            write_full_box(mp4a, b"esds", 0, 0, |esds| {
                                let asc = &audio.audio_specific_config;
                                // ES_Descriptor (tag 0x03)
                                esds.push(0x03);
                                esds.push((21 + asc.len()) as u8);
                                esds.extend_from_slice(&track_id.to_be_bytes()[2..4]);
                                esds.push(0x00);
                                // DecoderConfigDescriptor (tag 0x04)
                                esds.push(0x04);
                                esds.push((15 + asc.len()) as u8);
                                esds.push(0x40); // MPEG-4 Audio
                                esds.push(0x15); // AudioStream
                                esds.extend_from_slice(&[0x00, 0x00, 0x00]);
                                esds.extend_from_slice(&128_000u32.to_be_bytes());
                                esds.extend_from_slice(&128_000u32.to_be_bytes());
                                // DecoderSpecificInfo (tag 0x05)
                                esds.push(0x05);
                                esds.push(asc.len() as u8);
                                esds.extend_from_slice(asc);
                                // SLConfigDescriptor (tag 0x06)
                                esds.push(0x06);
                                esds.push(1);
                                esds.push(0x02);
                            });
                        });
                    });
                    write_empty_sample_tables(stbl);
                });
            });
        });
    });
}

fn write_dinf(minf: &mut Vec<u8>) {
    write_box(minf, b"dinf", |dinf| {
        write_full_box(dinf, b"dref", 0, 0, |dref| {
            dref.extend_from_slice(&1u32.to_be_bytes());
            write_full_box(dref, b"url ", 0, 1, |_| {});
        });
    });
}

fn write_empty_sample_tables(stbl: &mut Vec<u8>) {
    write_full_box(stbl, b"stts", 0, 0, |b| b.extend_from_slice(&0u32.to_be_bytes()));
    write_full_box(stbl, b"stsc", 0, 0, |b| b.extend_from_slice(&0u32.to_be_bytes()));
    write_full_box(stbl, b"stsz", 0, 0, |b| {
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
    });
    write_full_box(stbl, b"stco", 0, 0, |b| b.extend_from_slice(&0u32.to_be_bytes()));
}

fn build_media_fragment(
    sequence_number: u32,
    video_decode_time_90k: u64,
    video_duration_90k: u32,
    video_is_keyframe: bool,
    video_avcc: &[u8],
    audio_samples: &[QueuedSample],
) -> Vec<u8> {
    let mut moof = Vec::with_capacity(256);
    let mut v_data_offset_pos = 0usize;
    let mut a_data_offset_pos = None;

    write_box(&mut moof, b"moof", |buf| {
        write_full_box(buf, b"mfhd", 0, 0, |mfhd| {
            mfhd.extend_from_slice(&sequence_number.to_be_bytes());
        });

        // Video traf (track 1)
        write_box(buf, b"traf", |traf| {
            // flags = 0x020000 (default-base-is-moof)
            write_full_box(traf, b"tfhd", 0, 0x020000, |tfhd| {
                tfhd.extend_from_slice(&1u32.to_be_bytes());
            });
            write_full_box(traf, b"tfdt", 1, 0, |tfdt| {
                tfdt.extend_from_slice(&video_decode_time_90k.to_be_bytes());
            });
            // flags = 0x000701: data-offset + sample-duration + sample-size + sample-flags
            write_full_box(traf, b"trun", 0, 0x000701, |trun| {
                trun.extend_from_slice(&1u32.to_be_bytes()); // sample_count = 1
                v_data_offset_pos = trun.len();
                trun.extend_from_slice(&0i32.to_be_bytes()); // placeholder
                trun.extend_from_slice(&video_duration_90k.to_be_bytes());
                trun.extend_from_slice(&(video_avcc.len() as u32).to_be_bytes());
                let sample_flags: u32 = if video_is_keyframe {
                    0x0200_0000 // depends_on=2 (does not depend), sync=0
                } else {
                    0x0101_0000 // depends_on=1, non_sync=1
                };
                trun.extend_from_slice(&sample_flags.to_be_bytes());
            });
        });

        // Optional Audio traf (track 2)
        if !audio_samples.is_empty() {
            write_box(buf, b"traf", |traf| {
                write_full_box(traf, b"tfhd", 0, 0x020000, |tfhd| {
                    tfhd.extend_from_slice(&2u32.to_be_bytes());
                });
                write_full_box(traf, b"tfdt", 1, 0, |tfdt| {
                    tfdt.extend_from_slice(&audio_samples[0].pts_90k_or_48k.to_be_bytes());
                });
                // flags = 0x000301: data-offset + sample-duration + sample-size
                write_full_box(traf, b"trun", 0, 0x000301, |trun| {
                    trun.extend_from_slice(&(audio_samples.len() as u32).to_be_bytes());
                    a_data_offset_pos = Some(trun.len());
                    trun.extend_from_slice(&0i32.to_be_bytes());
                    for s in audio_samples {
                        trun.extend_from_slice(&s.duration.to_be_bytes());
                        trun.extend_from_slice(&(s.data.len() as u32).to_be_bytes());
                    }
                });
            });
        }
    });

    // moof の直後に mdat 8-byte ヘッダがあり、その直後からデータが始まる
    let mdat_payload_offset = (moof.len() + 8) as i32;
    moof[v_data_offset_pos..v_data_offset_pos + 4]
        .copy_from_slice(&mdat_payload_offset.to_be_bytes());

    if let Some(pos) = a_data_offset_pos {
        let a_offset = mdat_payload_offset + video_avcc.len() as i32;
        moof[pos..pos + 4].copy_from_slice(&a_offset.to_be_bytes());
    }

    let audio_bytes: usize = audio_samples.iter().map(|s| s.data.len()).sum();
    let mdat_size = (8 + video_avcc.len() + audio_bytes) as u32;

    let mut out = Vec::with_capacity(moof.len() + mdat_size as usize);
    out.extend_from_slice(&moof);
    out.extend_from_slice(&mdat_size.to_be_bytes());
    out.extend_from_slice(b"mdat");
    out.extend_from_slice(video_avcc);
    for s in audio_samples {
        out.extend_from_slice(&s.data);
    }
    out
}

/// Annex B（`00 00 00 01` / `00 00 01`）あるいは AVCC (`avcC` レコード) から SPS と PPS を抽出する
pub fn extract_sps_pps(data: &[u8]) -> CoreResult<(Vec<u8>, Vec<u8>)> {
    if data.is_empty() {
        return Err(CoreError::Codec("empty H.264 sequence header".into()));
    }

    // AVCDecoderConfigurationRecord (version == 1)
    if data.len() >= 11 && data[0] == 1 && !data.starts_with(&[0, 0, 1]) && !data.starts_with(&[0, 0, 0, 1]) {
        let num_sps = (data[5] & 0x1F) as usize;
        let mut pos = 6usize;
        let mut sps = None;
        for i in 0..num_sps {
            if pos + 2 > data.len() {
                break;
            }
            let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2;
            if pos + len > data.len() {
                break;
            }
            if i == 0 {
                sps = Some(data[pos..pos + len].to_vec());
            }
            pos += len;
        }
        let mut pps = None;
        if pos < data.len() {
            let num_pps = data[pos] as usize;
            pos += 1;
            for i in 0..num_pps {
                if pos + 2 > data.len() {
                    break;
                }
                let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
                pos += 2;
                if pos + len > data.len() {
                    break;
                }
                if i == 0 {
                    pps = Some(data[pos..pos + len].to_vec());
                }
                pos += len;
            }
        }
        if let (Some(s), Some(p)) = (sps, pps) {
            return Ok((s, p));
        }
    }

    // Annex B スタートコード走査
    let nals = split_annex_b_nals(data);
    let mut sps = None;
    let mut pps = None;
    for nal in nals {
        if nal.is_empty() {
            continue;
        }
        match nal[0] & 0x1F {
            7 if sps.is_none() => sps = Some(nal.to_vec()),
            8 if pps.is_none() => pps = Some(nal.to_vec()),
            _ => {}
        }
    }

    match (sps, pps) {
        (Some(s), Some(p)) => Ok((s, p)),
        _ => Err(CoreError::Codec(
            "H.264 sequence header missing SPS/PPS".into(),
        )),
    }
}

/// H.264 映像フレームを 4-byte length-prefixed AVCC 形式に変換（SPS/PPS/AUD NAL は除外）
pub fn normalize_h264_frame_to_avcc(data: &[u8]) -> CoreResult<Vec<u8>> {
    let nals = if has_annex_b_start_code(data) {
        split_annex_b_nals(data)
    } else {
        split_avcc_nals(data)?
    };

    let mut out = Vec::with_capacity(data.len() + 16);
    for nal in nals {
        if nal.is_empty() {
            continue;
        }
        let nal_type = nal[0] & 0x1F;
        // `avc1` サンプルエントリ内では SPS(7)/PPS(8)/AUD(9) を mdat に含めない
        if matches!(nal_type, 7..=9) {
            continue;
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    Ok(out)
}

fn has_annex_b_start_code(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1])
}

fn split_annex_b_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut i = 0usize;
    let mut nal_start = None;

    while i < data.len() {
        let sc_len = if i + 4 <= data.len() && data[i..i + 4] == [0, 0, 0, 1] {
            4
        } else if i + 3 <= data.len() && data[i..i + 3] == [0, 0, 1] {
            3
        } else {
            0
        };
        if sc_len > 0 {
            if let Some(start) = nal_start {
                if i > start {
                    nals.push(&data[start..i]);
                }
            }
            i += sc_len;
            nal_start = Some(i);
        } else {
            i += 1;
        }
    }

    if let Some(start) = nal_start {
        if data.len() > start {
            nals.push(&data[start..]);
        }
    }
    nals
}

fn split_avcc_nals(data: &[u8]) -> CoreResult<Vec<&[u8]>> {
    let mut nals = Vec::new();
    let mut pos = 0usize;
    while pos + 4 <= data.len() {
        let len = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;
        if len == 0 || pos + len > data.len() {
            return Err(CoreError::Codec("invalid AVCC NAL length in frame".into()));
        }
        nals.push(&data[pos..pos + len]);
        pos += len;
    }
    if pos != data.len() {
        return Err(CoreError::Codec("trailing bytes in AVCC frame".into()));
    }
    Ok(nals)
}
