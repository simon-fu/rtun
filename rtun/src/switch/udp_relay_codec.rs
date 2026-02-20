use anyhow::{bail, Result};

pub const UDP_RELAY_META_LEN_LEGACY: usize = 8;
pub const UDP_RELAY_META_LEN_OBFS: usize = 10;
pub const UDP_RELAY_FLOW_ID_MASK_LEGACY: u64 = (1_u64 << 48) - 1;
pub const UDP_RELAY_FLOW_ID_MASK_OBFS: u64 = u32::MAX as u64;
pub const UDP_RELAY_HEARTBEAT_FLOW_ID: u64 = 0;
pub const UDP_RELAY_FLAG_HAS_SEQ: u8 = 0x01;
pub const STUN_MAGIC_COOKIE: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];
const UDP_RELAY_SEQ_MAX_LEN: usize = 10;
const UDP_RELAY_OBFS_META_OFFSET_END: usize = 9;

#[derive(Debug, Clone, Copy)]
pub struct UdpRelayDecodedPacket<'a> {
    pub flow_id: u64,
    pub payload: &'a [u8],
    pub flags: u8,
    pub seq: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct UdpRelayCodec {
    pub obfs_seed: u32,
}

impl UdpRelayCodec {
    pub fn new(obfs_seed: u32) -> Self {
        Self { obfs_seed }
    }

    pub fn is_obfs(self) -> bool {
        self.obfs_seed != 0
    }

    pub fn header_len(self) -> usize {
        if self.is_obfs() {
            UDP_RELAY_META_LEN_OBFS
        } else {
            UDP_RELAY_META_LEN_LEGACY
        }
    }

    pub fn mode_name(self) -> &'static str {
        if self.is_obfs() {
            "obfs-v2"
        } else {
            "legacy"
        }
    }
}

pub fn gen_udp_relay_obfs_seed() -> u32 {
    let seed = rand::random::<u32>();
    if seed == 0 {
        1
    } else {
        seed
    }
}

pub fn encode_udp_relay_packet(
    buf: &mut [u8],
    flow_id: u64,
    payload: &[u8],
    codec: UdpRelayCodec,
) -> Result<usize> {
    encode_udp_relay_packet_with_seq(buf, flow_id, payload, None, codec)
}

pub fn encode_udp_relay_packet_with_seq(
    buf: &mut [u8],
    flow_id: u64,
    payload: &[u8],
    seq: Option<u64>,
    codec: UdpRelayCodec,
) -> Result<usize> {
    if payload.len() > u16::MAX as usize {
        bail!("payload too large [{}]", payload.len());
    }

    if codec.is_obfs() {
        let flow_id = flow_id & UDP_RELAY_FLOW_ID_MASK_OBFS;
        let nonce = rand::random::<u8>();
        let len = payload.len() as u16;
        let mut flags = 0_u8;
        let mut seq_buf = [0_u8; UDP_RELAY_SEQ_MAX_LEN];
        let seq_len = if let Some(seq) = seq {
            flags |= UDP_RELAY_FLAG_HAS_SEQ;
            encode_uvarint(seq, &mut seq_buf)
        } else {
            0
        };
        let header_len = codec.header_len();
        let packet_len = header_len + seq_len + payload.len();
        if packet_len > buf.len() {
            bail!("packet buffer too small [{}] < [{}]", buf.len(), packet_len);
        }
        let tag = udp_relay_tag(codec.obfs_seed, nonce, flow_id, len, flags, seq);
        let meta = (flow_id << 32) | ((len as u64) << 16) | tag as u64;
        let obfs_meta = meta ^ udp_relay_obfs_mask(codec.obfs_seed, nonce);
        buf[0] = nonce;
        buf[1..9].copy_from_slice(&obfs_meta.to_be_bytes());
        buf[9] = flags ^ udp_relay_flags_mask(codec.obfs_seed, nonce);
        if seq_len > 0 {
            buf[10..10 + seq_len].copy_from_slice(&seq_buf[..seq_len]);
        }
        let payload_offset = header_len + seq_len;
        buf[payload_offset..packet_len].copy_from_slice(payload);
        return Ok(packet_len);
    } else {
        if seq.is_some() {
            bail!("legacy codec does not support seq");
        }
        let flow_id = flow_id & UDP_RELAY_FLOW_ID_MASK_LEGACY;
        let header_len = codec.header_len();
        let packet_len = header_len + payload.len();
        if packet_len > buf.len() {
            bail!("packet buffer too small [{}] < [{}]", buf.len(), packet_len);
        }
        buf[..6].copy_from_slice(&flow_id.to_be_bytes()[2..]);
        buf[6..8].copy_from_slice(&(payload.len() as u16).to_be_bytes());
        buf[header_len..packet_len].copy_from_slice(payload);
        return Ok(packet_len);
    }
}

pub fn decode_udp_relay_packet(packet: &[u8], codec: UdpRelayCodec) -> Result<(u64, &[u8])> {
    let pkt = decode_udp_relay_packet_ex(packet, codec)?;
    Ok((pkt.flow_id, pkt.payload))
}

pub fn decode_udp_relay_packet_ex(
    packet: &[u8],
    codec: UdpRelayCodec,
) -> Result<UdpRelayDecodedPacket<'_>> {
    if codec.is_obfs() {
        if packet.len() < UDP_RELAY_META_LEN_OBFS {
            bail!(
                "packet as least [{}] but [{}]",
                UDP_RELAY_META_LEN_OBFS,
                packet.len()
            );
        }

        let nonce = packet[0];
        let mut meta_raw = [0_u8; 8];
        meta_raw.copy_from_slice(&packet[1..UDP_RELAY_OBFS_META_OFFSET_END]);
        let meta = u64::from_be_bytes(meta_raw) ^ udp_relay_obfs_mask(codec.obfs_seed, nonce);
        let flow_id = (meta >> 32) & UDP_RELAY_FLOW_ID_MASK_OBFS;
        let len = ((meta >> 16) & 0xffff) as usize;
        let got_tag = (meta & 0xffff) as u16;
        let flags = packet[9] ^ udp_relay_flags_mask(codec.obfs_seed, nonce);
        let mut payload_offset = UDP_RELAY_META_LEN_OBFS;
        let seq = if flags & UDP_RELAY_FLAG_HAS_SEQ != 0 {
            let (seq, seq_len) = decode_uvarint(&packet[payload_offset..])?;
            payload_offset += seq_len;
            Some(seq)
        } else {
            None
        };
        if flags & !UDP_RELAY_FLAG_HAS_SEQ != 0 {
            bail!("unknown relay flags [{:#04x}]", flags);
        }
        let expected_tag = udp_relay_tag(codec.obfs_seed, nonce, flow_id, len as u16, flags, seq);
        if got_tag != expected_tag {
            bail!("tag mismatch [{}] expected [{}]", got_tag, expected_tag);
        }
        if len > packet.len().saturating_sub(payload_offset) {
            bail!(
                "meta.len [{}] exceed [{}]",
                len,
                packet.len().saturating_sub(payload_offset)
            );
        }
        let payload = &packet[payload_offset..payload_offset + len];
        return Ok(UdpRelayDecodedPacket {
            flow_id,
            payload,
            flags,
            seq,
        });
    }

    if packet.len() < UDP_RELAY_META_LEN_LEGACY {
        bail!(
            "packet as least [{}] but [{}]",
            UDP_RELAY_META_LEN_LEGACY,
            packet.len()
        );
    }

    let flow_id = u64::from_be_bytes([
        0, 0, packet[0], packet[1], packet[2], packet[3], packet[4], packet[5],
    ]);
    let len = u16::from_be_bytes([packet[6], packet[7]]) as usize;
    if len > packet.len() - UDP_RELAY_META_LEN_LEGACY {
        bail!(
            "meta.len [{}] exceed [{}]",
            len,
            packet.len() - UDP_RELAY_META_LEN_LEGACY
        );
    }

    let payload = &packet[UDP_RELAY_META_LEN_LEGACY..UDP_RELAY_META_LEN_LEGACY + len];
    Ok(UdpRelayDecodedPacket {
        flow_id,
        payload,
        flags: 0,
        seq: None,
    })
}

pub fn packet_has_stun_magic(packet: &[u8]) -> bool {
    packet.len() >= 8 && packet[4..8] == STUN_MAGIC_COOKIE
}

pub fn is_udp_relay_tag_mismatch(err: &anyhow::Error) -> bool {
    err.to_string().contains("tag mismatch")
}

fn udp_relay_obfs_mask(seed: u32, nonce: u8) -> u64 {
    let mut z = ((seed as u64) << 8) | nonce as u64;
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn udp_relay_flags_mask(seed: u32, nonce: u8) -> u8 {
    let mut z = ((seed as u64) << 8) | nonce as u64;
    z ^= 0xa5a5_5a5a_d3d3_c3c3;
    z = (z ^ (z >> 29)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^= z >> 32;
    (z & 0xff) as u8
}

fn udp_relay_tag(seed: u32, nonce: u8, flow_id: u64, len: u16, flags: u8, seq: Option<u64>) -> u16 {
    let mut z = ((seed as u64) << 32) | (flow_id & UDP_RELAY_FLOW_ID_MASK_OBFS);
    z ^= (nonce as u64) << 56;
    z ^= (len as u64) << 16;
    z ^= (flags as u64) << 8;
    if let Some(seq) = seq {
        z ^= seq.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        z = z.rotate_left((seq as u32 & 31) + 1);
    }
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^= z >> 31;
    (z ^ (z >> 16) ^ (z >> 32) ^ (z >> 48)) as u16
}

fn encode_uvarint(mut v: u64, out: &mut [u8; UDP_RELAY_SEQ_MAX_LEN]) -> usize {
    let mut i = 0usize;
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out[i] = b;
        i += 1;
        if v == 0 {
            break;
        }
    }
    i
}

fn decode_uvarint(buf: &[u8]) -> Result<(u64, usize)> {
    let mut x = 0_u64;
    let mut s = 0_u32;
    for (i, b) in buf.iter().copied().take(UDP_RELAY_SEQ_MAX_LEN).enumerate() {
        let v = (b & 0x7f) as u64;
        if s >= 64 || (s == 63 && v > 1) {
            bail!("seq varint overflow");
        }
        x |= v << s;
        if b & 0x80 == 0 {
            let mut check = [0_u8; UDP_RELAY_SEQ_MAX_LEN];
            let n = encode_uvarint(x, &mut check);
            if n != i + 1 {
                bail!("non-canonical seq varint");
            }
            return Ok((x, i + 1));
        }
        s += 7;
    }
    bail!("seq varint incomplete")
}

#[cfg(test)]
mod tests {
    use super::{
        decode_udp_relay_packet_ex, encode_udp_relay_packet, encode_udp_relay_packet_with_seq,
        UdpRelayCodec, UDP_RELAY_FLAG_HAS_SEQ,
    };

    #[test]
    fn seq_roundtrip_obfs() {
        let codec = UdpRelayCodec::new(1234);
        let mut buf = [0_u8; 256];
        let n =
            encode_udp_relay_packet_with_seq(&mut buf, 7, b"hello", Some(300), codec).unwrap();
        let pkt = decode_udp_relay_packet_ex(&buf[..n], codec).unwrap();
        assert_eq!(pkt.flow_id, 7);
        assert_eq!(pkt.payload, b"hello");
        assert_eq!(pkt.seq, Some(300));
        assert_eq!(pkt.flags & UDP_RELAY_FLAG_HAS_SEQ, UDP_RELAY_FLAG_HAS_SEQ);
    }

    #[test]
    fn seq_not_allowed_in_legacy() {
        let codec = UdpRelayCodec::new(0);
        let mut buf = [0_u8; 64];
        let err =
            encode_udp_relay_packet_with_seq(&mut buf, 7, b"hello", Some(1), codec).unwrap_err();
        assert!(
            err.to_string().contains("legacy codec does not support seq"),
            "unexpected err: {err}"
        );
    }

    #[test]
    fn decode_default_api_ignores_seq() {
        let codec = UdpRelayCodec::new(5678);
        let mut buf = [0_u8; 256];
        let n =
            encode_udp_relay_packet_with_seq(&mut buf, 9, b"payload", Some(99), codec).unwrap();
        let (flow_id, payload) = encode_then_decode_default(&buf[..n], codec);
        assert_eq!(flow_id, 9);
        assert_eq!(payload, b"payload");
    }

    fn encode_then_decode_default(packet: &[u8], codec: UdpRelayCodec) -> (u64, Vec<u8>) {
        let (flow_id, payload) = super::decode_udp_relay_packet(packet, codec).unwrap();
        (flow_id, payload.to_vec())
    }

    #[test]
    fn default_encode_without_seq() {
        let codec = UdpRelayCodec::new(777);
        let mut buf = [0_u8; 128];
        let n = encode_udp_relay_packet(&mut buf, 1, b"a", codec).unwrap();
        let pkt = decode_udp_relay_packet_ex(&buf[..n], codec).unwrap();
        assert_eq!(pkt.seq, None);
        assert_eq!(pkt.flags & UDP_RELAY_FLAG_HAS_SEQ, 0);
    }
}
