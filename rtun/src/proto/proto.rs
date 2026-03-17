include!(concat!(env!("OUT_DIR"), "/generated_with_pure/mod.rs"));

use crate::ice::{ice_peer::IceArgs, ice_quic::QuicIceArgs};

pub use self::app::*;

use self::{open_channel_response::Open_ch_rsp, open_p2presponse::Open_p2p_rsp};

pub fn make_open_shell_response_ok(ch_id: u64) -> OpenChannelResponse {
    make_open_channel_response_ok(ch_id)
}

pub fn make_open_shell_response_error<D: std::fmt::Debug>(error: D) -> OpenChannelResponse {
    make_open_channel_response_error(error)
}

pub fn make_open_channel_response_ok(ch_id: u64) -> OpenChannelResponse {
    OpenChannelResponse {
        open_ch_rsp: Some(Open_ch_rsp::ChId(ch_id)),
        ..Default::default()
    }
}

pub fn make_open_channel_response_error<D: std::fmt::Debug>(error: D) -> OpenChannelResponse {
    OpenChannelResponse {
        open_ch_rsp: Some(Open_ch_rsp::Status(ResponseStatus {
            code: -1,
            reason: format!("{error:?}").into(),
            ..Default::default()
        })),
        ..Default::default()
    }
}

pub fn make_response_status_ok() -> ResponseStatus {
    // be careful that code 0 and reason "" will generate zero bytes,
    make_response_status_raw(0, "ok")
}

pub fn make_response_status_raw<I: Into<String>>(code: i32, reason: I) -> ResponseStatus {
    ResponseStatus {
        code,
        reason: reason.into().into(),
        ..Default::default()
    }
}

pub fn make_open_p2p_response_ok(args: P2PArgs) -> OpenP2PResponse {
    OpenP2PResponse {
        open_p2p_rsp: Some(Open_p2p_rsp::Args(args)),
        ..Default::default()
    }
}

pub fn make_open_p2p_response_error<D: std::fmt::Debug>(error: D) -> OpenP2PResponse {
    OpenP2PResponse {
        open_p2p_rsp: Some(Open_p2p_rsp::Status(ResponseStatus {
            code: -1,
            reason: format!("{error:?}").into(),
            ..Default::default()
        })),
        ..Default::default()
    }
}

impl From<IceArgs> for P2PIceArgs {
    fn from(value: IceArgs) -> Self {
        Self {
            ufrag: value.ufrag.into(),
            pwd: value.pwd.into(),
            candidates: value.candidates.into_iter().map(|x| x.into()).collect(),
            ..Default::default()
        }
    }
}

impl From<P2PIceArgs> for IceArgs {
    fn from(value: P2PIceArgs) -> Self {
        Self {
            ufrag: value.ufrag.into(),
            pwd: value.pwd.into(),
            candidates: value.candidates.into_iter().map(|x| x.into()).collect(),
        }
    }
}

impl From<QuicIceArgs> for P2PQuicArgs {
    fn from(value: QuicIceArgs) -> Self {
        Self {
            ice: ::protobuf::MessageField::some(value.ice.into()),
            cert_der: value.cert_der,
            // cert_der: value.cert_der.map(|x|x.into()),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::Message;

    fn roundtrip<M>(msg: &M) -> M
    where
        M: Message + Default,
    {
        M::parse_from_bytes(&msg.write_to_bytes().expect("encode")).expect("decode")
    }

    #[test]
    fn hardnat_control_envelope_roundtrip_all_variants() {
        let cases = vec![
            HardNatControlEnvelope {
                session_id: 7,
                seq: 11,
                role_from: 2,
                msg: Some(hard_nat_control_envelope::Msg::StartBatch(
                    HardNatStartBatch {
                        batch_id: 3,
                        nat3_addr_index: 1,
                        nat4_ip_index: 2,
                        ports: vec![40001, 40002, 40003],
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
            HardNatControlEnvelope {
                session_id: 7,
                seq: 12,
                role_from: 2,
                msg: Some(hard_nat_control_envelope::Msg::AdvanceNat4Ip(
                    HardNatAdvanceNat4Ip {
                        batch_id: 3,
                        next_nat4_ip_index: 4,
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
            HardNatControlEnvelope {
                session_id: 7,
                seq: 13,
                role_from: 2,
                msg: Some(hard_nat_control_envelope::Msg::AdvanceNat3Addr(
                    HardNatAdvanceNat3Addr {
                        batch_id: 3,
                        next_nat3_addr_index: 5,
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
            HardNatControlEnvelope {
                session_id: 7,
                seq: 14,
                role_from: 2,
                msg: Some(hard_nat_control_envelope::Msg::NextBatch(
                    HardNatNextBatch {
                        next_batch_id: 4,
                        nat3_addr_index: 0,
                        nat4_ip_index: 0,
                        ports: vec![41001, 41002],
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
            HardNatControlEnvelope {
                session_id: 7,
                seq: 15,
                role_from: 2,
                msg: Some(hard_nat_control_envelope::Msg::LeaseKeepAlive(
                    HardNatLeaseKeepAlive {
                        lease_timeout_ms: 12_000,
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
            HardNatControlEnvelope {
                session_id: 7,
                seq: 16,
                role_from: 2,
                msg: Some(hard_nat_control_envelope::Msg::Connected(
                    HardNatConnected {
                        selected_nat3_addr: "203.0.113.10:40001".into(),
                        selected_nat4_ip: "198.51.100.20".into(),
                        selected_port: 40001,
                        restore_ttl: 64,
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
            HardNatControlEnvelope {
                session_id: 7,
                seq: 17,
                role_from: 2,
                msg: Some(hard_nat_control_envelope::Msg::Abort(HardNatAbort {
                    reason: "timed out".into(),
                    ..Default::default()
                })),
                ..Default::default()
            },
            HardNatControlEnvelope {
                session_id: 7,
                seq: 18,
                role_from: 1,
                msg: Some(hard_nat_control_envelope::Msg::Ack(HardNatAck {
                    acked_seq: 16,
                    state: 3,
                    ..Default::default()
                })),
                ..Default::default()
            },
        ];

        for case in cases {
            let got = roundtrip(&case);
            assert_eq!(got, case);
        }
    }

    #[test]
    fn ctrl_channel_packet_roundtrip_rpc_and_hardnat_control() {
        let rpc = CtrlChannelPacket {
            body: Some(ctrl_channel_packet::Body::RpcResponse(CtrlRpcResponse {
                body: Some(ctrl_rpc_response::Body::OpenP2p(OpenP2PResponse {
                    open_p2p_rsp: Some(open_p2presponse::Open_p2p_rsp::Status(ResponseStatus {
                        code: 0,
                        reason: "ok".into(),
                        ..Default::default()
                    })),
                    ..Default::default()
                })),
                ..Default::default()
            })),
            ..Default::default()
        };
        assert_eq!(roundtrip(&rpc), rpc);

        let hardnat = CtrlChannelPacket {
            body: Some(ctrl_channel_packet::Body::HardNatControl(
                HardNatControlEnvelope {
                    session_id: 9,
                    seq: 99,
                    role_from: 2,
                    msg: Some(hard_nat_control_envelope::Msg::LeaseKeepAlive(
                        HardNatLeaseKeepAlive {
                            lease_timeout_ms: 5000,
                            ..Default::default()
                        },
                    )),
                    ..Default::default()
                },
            )),
            ..Default::default()
        };
        assert_eq!(roundtrip(&hardnat), hardnat);
    }
}
