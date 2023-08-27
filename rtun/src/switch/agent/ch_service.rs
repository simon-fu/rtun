use anyhow::{Result, Context, anyhow};
use protobuf::Message;
use crate::{channel::{ChSender, ChReceiver}, proto::{C2ARequest, c2arequest::C2a_req_args, make_open_shell_response_ok, make_open_channel_response_error}, async_rt::spawn_with_name, huid::gen_huid::gen_huid};

use super::ch_shell::open_shell;


pub(super) async fn channel_service(tx: ChSender, mut rx: ChReceiver) -> Result<()> {
    let packet = rx.recv_packet().await?;

    let cmd = C2ARequest::parse_from_bytes(&packet.payload)?
    .c2a_req_args
    .with_context(||"no c2a_req_args")?;
    match cmd {
        C2a_req_args::OpenSell(args) => {
            
            let r = open_shell(args).await;

            let (rsp, shell) = match r {
                Ok(shell) => {
                    (make_open_shell_response_ok(0), Some(shell))
                },
                Err(e) => (make_open_channel_response_error(e), None),
            };

            tx.send_data(rsp.write_to_bytes()?.into()).await
            .map_err(|_x|anyhow!("send data fail"))?;

            if let Some(shell) = shell {

                let uid = gen_huid();
                tracing::debug!("open shell [{}]", uid);
                
                let name = format!("shell-{}", uid);
                spawn_with_name(name, async move {
                    
                    let r = shell.run(tx, rx).await;
                    tracing::debug!("finished with [{:?}]", r);
        
                    // TODO: remove channel
        
                });

            }
        },
        _ => {
            todo!() // aaa
        }
    }
    Ok(())
}


