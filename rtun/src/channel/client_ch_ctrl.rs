
// use anyhow::{Result, Context, bail, anyhow};
// use protobuf::Message as PbMessage;
// use crate::{proto::{OpenShellArgs, OpenChannelResponse, open_channel_response::Open_ch_rsp, OpenChannelRequest}, channel::{ChPair, ChId}, switch::{agent_invoker::{AgentInvoker, AgentEntity}, ctrl_client::c2a_open_shell}};


// pub struct ClientChannelCtrl<E: AgentEntity> {
//     pair: ChPair,
//     invoker: AgentInvoker<E>,
// }

// impl<E: AgentEntity> ClientChannelCtrl<E> {
//     pub fn new(pair: ChPair, invoker: AgentInvoker<E> ) -> Self {
//         Self {
//             pair,
//             invoker,
//         }
//     }

//     pub async fn open_shell(&mut self, args: OpenShellArgs) -> Result<ChPair> {
//         // let data = C2ARequest {
//         //     c2a_req_args: Some(C2a_req_args::OpenSell(args)),
//         //     ..Default::default()
//         // }.write_to_bytes()?;
    
//         // self.pair.tx.send_data(data.into()).await.map_err(|_e|anyhow!("send open shell failed"))?;
    
//         // let packet = self.pair.rx.recv_data().await.with_context(||"recv open shell response failed")?;

//         // // let rsp = C2AResponse::parse_from_bytes(&data).with_context(||"parse open shell response failed")?;
//         // let rsp = OpenChannelResponse::parse_from_bytes(&packet.payload)
//         // .with_context(||"parse open shell response failed")?
//         // .open_ch_rsp.with_context(||"has no response")?;

//         // let shell_ch_id = match rsp {
//         //     Open_ch_rsp::ChId(v) => ChId(v),
//         //     Open_ch_rsp::Status(status) => bail!("open shell response status {:?}", status),
//         //     // _ => bail!("unknown"),
//         // };

//         let shell_ch_id = c2a_open_shell(&mut self.pair, args).await?;
//         let pair = self.invoker.add_channel(shell_ch_id).await?;
//         Ok(pair)
//     }

//     pub async fn open_channel(&mut self) -> Result<ChPair> {
//         let data = OpenChannelRequest {
//             open_ch_req: None,
//             ..Default::default()
//         }.write_to_bytes()?;

//         self.pair.tx.send_data(data.into()).await.map_err(|_e|anyhow!("send failed"))?;

//         let packet = self.pair.rx.recv_data().await.with_context(||"recv failed")?;

//         let rsp = OpenChannelResponse::parse_from_bytes(&packet.payload)
//         .with_context(||"parse response failed")?
//         .open_ch_rsp.with_context(||"has no response")?;
        
//         let ch_id = match rsp {
//             Open_ch_rsp::ChId(v) => ChId(v),
//             Open_ch_rsp::Status(status) => bail!("response status {:?}", status),
//             // _ => bail!("unknown"),
//         };

//         let pair = self.invoker.add_channel(ch_id).await?;
//         Ok(pair)

//     }
// }


