
include!(concat!(env!("OUT_DIR"), "/generated_with_pure/mod.rs"));

pub use self::app::*;

use self::open_channel_response::Open_ch_rsp;


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

