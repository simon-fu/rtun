/* 
TODO:
    - 动态调整 终端 大小
    - 合并 resize 事件 
    - 调用系统默认 shell，不是写死 bash
    - 支持 wss
*/


use anyhow::{Result, Context, bail};
use futures::StreamExt;
use protobuf::Message as PbMessage;
use rtun::{proto::OpenShellArgs, channel::ChId, huid::gen_huid::gen_huid};
use tokio_tungstenite::{connect_async, tungstenite::{Message as WsMessage, self}};

use crate::{ws_client_session::make_ws_client_session, client_ch_ctrl::ClientChannelCtrl};



pub async fn run() -> Result<()> {
    let url = "ws://127.0.0.1:3000/ws";
    
    let (stream, response) = connect_async(url).await?;
    tracing::debug!("connected to [{}]", url);
    tracing::debug!("first response was {:?}", response);

    // handle_handshake(&mut stream).await?;

    let uid = gen_huid();

    let session = make_ws_client_session(uid, stream).await?;

    
    let invoker = session.invoker();
    let pair = invoker.add_channel(ChId(0)).await?;
    let mut ctrl = ClientChannelCtrl::new(pair);

    let ch_id = ChId(1);
    let size = super::term_termwiz::get_size().await?;
    ctrl.open_shell(OpenShellArgs {
        ch_id: ch_id.0,
        agent: "".into(),
        // term: "xterm-256color".into(),
        cols: size.cols as u32,
        rows: size.rows as u32,
        ..Default::default()
    }).await?;

    let (tx, rx) = invoker.add_channel(ch_id).await?;

    // super::term_std::run(tx, rx).await
    // super::term_crossterm::run(tx, rx).await
    super::term_termwiz::run(tx, rx).await
    
}



// pub async fn handle_handshake<S>(stream: &mut S) -> Result<()> 
// where
//     S: 'static 
//         + Send
//         + StreamExt<Item = Result<WsMessage, tungstenite::Error>> 
//         + SinkExt<WsMessage, Error = tungstenite::Error> 
//         + Unpin,
// {
//     let packet = HanshakeRequestPacket {
//         version: Version::VER_1.into(),
//         device_type: DeviceType::DEV_CLIENT.into(),
//         nonce: 0,
//         ..Default::default()
//     }.write_to_bytes()?;

//     stream.send(WsMessage::Binary(packet)).await?;
//     tracing::debug!("sent handshake request");

//     let rsp = ws_client_recv_binary(stream).await?;
//     let rsp = HanshakeResponsePacket::parse_from_bytes(&rsp)?;
//     tracing::debug!("recv handshake response {rsp:?}");

//     Ok(())
// }


pub async fn ws_client_recv_packet<S, P>(socket: &mut S) -> Result<P> 
where
    S: StreamExt<Item = Result<WsMessage, tungstenite::Error>> 
        + Unpin,
    P: PbMessage,
{
    let data = ws_client_recv_binary(socket).await?;
    P::parse_from_bytes(&data[..])
    .with_context(||"decode ws packet failed")
}

pub async fn ws_client_recv_binary<S>(socket: &mut S) -> Result<Vec<u8>> 
where
    S: StreamExt<Item = Result<WsMessage, tungstenite::Error>> 
        + Unpin,
{
    loop {
        let msg = socket.next().await
        .with_context(|| "recving handshake but closed")?
        .with_context(|| "recving handshake but failed")?;
    
        if let Some(data) = extract_ws_binary(msg)? {
            return Ok(data)
        }
    }
}

pub fn extract_ws_binary(msg: WsMessage) -> Result<Option<Vec<u8>>> {
    match msg {
        WsMessage::Text(s) => bail!("got msg::text {s:?}"),
        WsMessage::Ping(_ping) => Ok(None), // bail!("got msg::ping"),
        WsMessage::Pong(_pong) => Ok(None), // bail!("got msg::pong"),
        WsMessage::Close(c) => bail!("got msg::close {c:?}"),
        WsMessage::Binary(d) => {
            Ok(Some(d))
        },
        WsMessage::Frame(_v) => bail!("got msg::frame"),
    }
}
