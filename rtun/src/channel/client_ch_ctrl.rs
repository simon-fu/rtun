
use anyhow::{Result, Context, bail, anyhow};
use protobuf::Message as PbMessage;
use crate::{proto::{OpenShellArgs, C2ARequest, c2arequest::C2a_req_args, OpenChannelResponse, open_channel_response::Open_ch_rsp}, channel::{ChSender, ChReceiver, ChPair, ChId}};


pub struct ClientChannelCtrl {
    tx: ChSender,
    rx: ChReceiver,
}

impl ClientChannelCtrl {
    pub fn new(pair: ChPair) -> Self {
        Self {
            tx: pair.tx,
            rx: pair.rx,
        }
    }

    pub async fn open_shell(&mut self, args: OpenShellArgs) -> Result<ChId> {
        let data = C2ARequest {
            c2a_req_args: Some(C2a_req_args::OpenSell(args)),
            ..Default::default()
        }.write_to_bytes()?;
    
        self.tx.send_data(data.into()).await.map_err(|_e|anyhow!("send open shell failed"))?;
    
        let data = self.rx.recv_data().await.with_context(||"recv open shell response failed")?;

        // let rsp = C2AResponse::parse_from_bytes(&data).with_context(||"parse open shell response failed")?;
        let rsp = OpenChannelResponse::parse_from_bytes(&data)
        .with_context(||"parse open shell response failed")?
        .open_ch_rsp.with_context(||"has no response")?;

        match rsp {
            Open_ch_rsp::ChId(v) => Ok(ChId(v)),
            Open_ch_rsp::Status(status) => bail!("open shell response status {:?}", status),
            // _ => bail!("unknown"),
        }
    }
}
