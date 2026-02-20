use anyhow::{bail, Result};

pub const UDP_RELAY_META_LEN_LEGACY: usize = 8;
pub const UDP_RELAY_META_LEN_OBFS: usize = 9;
pub const UDP_RELAY_FLOW_ID_MASK_LEGACY: u64 = (1_u64 << 48) - 1;
pub const UDP_RELAY_FLOW_ID_MASK_OBFS: u64 = u32::MAX as u64;
pub const UDP_RELAY_HEARTBEAT_FLOW_ID: u64 = 0;
pub const STUN_MAGIC_COOKIE: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];

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
            "obfs-v1"
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
    if payload.len() > u16::MAX as usize {
        bail!("payload too large [{}]", payload.len());
    }
    let header_len = codec.header_len();
    let packet_len = header_len + payload.len();
    if packet_len > buf.len() {
        bail!("packet buffer too small [{}] < [{}]", buf.len(), packet_len);
    }

    if codec.is_obfs() {
        let flow_id = flow_id & UDP_RELAY_FLOW_ID_MASK_OBFS;
        let nonce = rand::random::<u8>();
        let len = payload.len() as u16;
        let tag = udp_relay_tag(codec.obfs_seed, nonce, flow_id, len);
        let meta = (flow_id << 32) | ((len as u64) << 16) | tag as u64;
        let obfs_meta = meta ^ udp_relay_obfs_mask(codec.obfs_seed, nonce);
        buf[0] = nonce;
        buf[1..9].copy_from_slice(&obfs_meta.to_be_bytes());
    } else {
        let flow_id = flow_id & UDP_RELAY_FLOW_ID_MASK_LEGACY;
        buf[..6].copy_from_slice(&flow_id.to_be_bytes()[2..]);
        buf[6..8].copy_from_slice(&(payload.len() as u16).to_be_bytes());
    }

    buf[header_len..packet_len].copy_from_slice(payload);
    Ok(packet_len)
}

pub fn decode_udp_relay_packet(packet: &[u8], codec: UdpRelayCodec) -> Result<(u64, &[u8])> {
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
        meta_raw.copy_from_slice(&packet[1..UDP_RELAY_META_LEN_OBFS]);
        let meta = u64::from_be_bytes(meta_raw) ^ udp_relay_obfs_mask(codec.obfs_seed, nonce);
        let flow_id = (meta >> 32) & UDP_RELAY_FLOW_ID_MASK_OBFS;
        let len = ((meta >> 16) & 0xffff) as usize;
        let got_tag = (meta & 0xffff) as u16;
        let expected_tag = udp_relay_tag(codec.obfs_seed, nonce, flow_id, len as u16);
        if got_tag != expected_tag {
            bail!("tag mismatch [{}] expected [{}]", got_tag, expected_tag);
        }
        if len > packet.len() - UDP_RELAY_META_LEN_OBFS {
            bail!(
                "meta.len [{}] exceed [{}]",
                len,
                packet.len() - UDP_RELAY_META_LEN_OBFS
            );
        }
        let payload = &packet[UDP_RELAY_META_LEN_OBFS..UDP_RELAY_META_LEN_OBFS + len];
        return Ok((flow_id, payload));
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
    Ok((flow_id, payload))
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

fn udp_relay_tag(seed: u32, nonce: u8, flow_id: u64, len: u16) -> u16 {
    let mut z = ((seed as u64) << 32) | (flow_id & UDP_RELAY_FLOW_ID_MASK_OBFS);
    z ^= (nonce as u64) << 56;
    z ^= (len as u64) << 16;
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^= z >> 31;
    (z ^ (z >> 16) ^ (z >> 32) ^ (z >> 48)) as u16
}
