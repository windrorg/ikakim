use core_types::{CoreError, CoreResult};
use std::io::{Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

const RTMP_VERSION: u8 = 0x03;
const HANDSHAKE_LEN: usize = 1536;

/// クライアント側 RTMP シンプルハンドシェイク（C0/C1/C2 <-> S0/S1/S2）
pub fn perform_client_handshake<S: Read + Write>(stream: &mut S) -> CoreResult<()> {
    let mut c0_c1 = vec![0u8; 1 + HANDSHAKE_LEN];
    c0_c1[0] = RTMP_VERSION;
    // C1: time(4B)=0, zero(4B)=0, random(1528B)
    fill_pseudo_random(&mut c0_c1[9..]);

    stream
        .write_all(&c0_c1)
        .map_err(|e| CoreError::Network(format!("RTMP handshake write C0/C1 failed: {}", e)))?;
    stream
        .flush()
        .map_err(|e| CoreError::Network(format!("RTMP handshake flush C0/C1 failed: {}", e)))?;

    let mut s0 = [0u8; 1];
    stream
        .read_exact(&mut s0)
        .map_err(|e| CoreError::Network(format!("RTMP handshake read S0 failed: {}", e)))?;
    if s0[0] != RTMP_VERSION {
        return Err(CoreError::Network(format!(
            "RTMP unsupported server version: {}",
            s0[0]
        )));
    }

    let mut s1 = vec![0u8; HANDSHAKE_LEN];
    stream
        .read_exact(&mut s1)
        .map_err(|e| CoreError::Network(format!("RTMP handshake read S1 failed: {}", e)))?;

    // C2 = S1 のエコーバック
    stream
        .write_all(&s1)
        .map_err(|e| CoreError::Network(format!("RTMP handshake write C2 failed: {}", e)))?;
    stream
        .flush()
        .map_err(|e| CoreError::Network(format!("RTMP handshake flush C2 failed: {}", e)))?;

    let mut s2 = vec![0u8; HANDSHAKE_LEN];
    stream
        .read_exact(&mut s2)
        .map_err(|e| CoreError::Network(format!("RTMP handshake read S2 failed: {}", e)))?;

    Ok(())
}

/// テスト用サーバー側 RTMP ハンドシェイク
pub fn perform_server_handshake<S: Read + Write>(stream: &mut S) -> CoreResult<()> {
    let mut c0 = [0u8; 1];
    stream
        .read_exact(&mut c0)
        .map_err(|e| CoreError::Network(format!("RTMP server read C0 failed: {}", e)))?;
    if c0[0] != RTMP_VERSION {
        return Err(CoreError::Network(format!(
            "RTMP unsupported client version: {}",
            c0[0]
        )));
    }

    let mut c1 = vec![0u8; HANDSHAKE_LEN];
    stream
        .read_exact(&mut c1)
        .map_err(|e| CoreError::Network(format!("RTMP server read C1 failed: {}", e)))?;

    let mut s0_s1_s2 = Vec::with_capacity(1 + HANDSHAKE_LEN * 2);
    s0_s1_s2.push(RTMP_VERSION);
    let mut s1 = vec![0u8; HANDSHAKE_LEN];
    fill_pseudo_random(&mut s1[8..]);
    s0_s1_s2.extend_from_slice(&s1);
    s0_s1_s2.extend_from_slice(&c1); // S2 = C1

    stream
        .write_all(&s0_s1_s2)
        .map_err(|e| CoreError::Network(format!("RTMP server write S0/S1/S2 failed: {}", e)))?;
    stream
        .flush()
        .map_err(|e| CoreError::Network(format!("RTMP server flush failed: {}", e)))?;

    let mut c2 = vec![0u8; HANDSHAKE_LEN];
    stream
        .read_exact(&mut c2)
        .map_err(|e| CoreError::Network(format!("RTMP server read C2 failed: {}", e)))?;
    Ok(())
}

fn fill_pseudo_random(dst: &mut [u8]) {
    let mut state = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        ^ (dst.as_ptr() as u64);

    for chunk in dst.chunks_mut(8) {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        let bytes = z.to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
}
