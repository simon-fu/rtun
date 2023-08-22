
use anyhow::{Result, Context, bail, anyhow};
use protobuf::Message as PbMessage;
use rtun::{proto::{OpenShellArgs, C2ARequest, c2arequest::C2a_req_args, C2AResponse}, channel::{ChSender, ChReceiver}};


pub struct ClientChannelCtrl {
    tx: ChSender,
    rx: ChReceiver,
}

impl ClientChannelCtrl {
    pub fn new((tx, rx): (ChSender, ChReceiver)) -> Self {
        Self {
            tx,
            rx,
        }
    }

    pub async fn open_shell(&mut self, args: OpenShellArgs) -> Result<()> {
        let data = C2ARequest {
            c2a_req_args: Some(C2a_req_args::OpenSell(args)),
            ..Default::default()
        }.write_to_bytes()?;
    
        self.tx.send_data(data.into()).await.map_err(|_e|anyhow!("send open shell failed"))?;
    
        let data = self.rx.recv_data().await.with_context(||"recv open shell response failed")?;
        let rsp = C2AResponse::parse_from_bytes(&data).with_context(||"parse open shell response failed")?;
        if rsp.status != 0 {
            bail!("open shell response bad {:?}", rsp)
        }

        Ok(())
    }
}