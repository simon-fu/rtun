
include!(concat!(env!("OUT_DIR"), "/generated_with_pure/mod.rs"));

use crate::ice::{ice_quic::QuicIceArgs, ice_peer::IceArgs};

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
        open_ch_rsp: Some(Open_ch_rsp::Status(ResponseStatus{
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
    ResponseStatus{
        code,
        reason: reason.into().into(),
        ..Default::default()
    }
}

pub fn make_open_p2p_response_ok(args: P2PArgs) -> OpenP2PResponse {
    OpenP2PResponse {
        open_p2p_rsp: Some(Open_p2p_rsp::Args(args) ),
        ..Default::default()
    }
}

pub fn make_open_p2p_response_error<D: std::fmt::Debug>(error: D) -> OpenP2PResponse {
    OpenP2PResponse {
        open_p2p_rsp: Some(Open_p2p_rsp::Status( ResponseStatus{
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
            candidates: value.candidates.into_iter().map(|x|x.into()).collect(),
            ..Default::default()
        }
    }
}

impl From<P2PIceArgs> for IceArgs {
    fn from(value: P2PIceArgs) -> Self {
        Self {
            ufrag: value.ufrag.into(),
            pwd: value.pwd.into(),
            candidates: value.candidates.into_iter().map(|x|x.into()).collect(),
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
