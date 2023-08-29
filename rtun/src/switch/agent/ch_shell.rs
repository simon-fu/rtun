use std::{ops::Deref, collections::HashMap};

use anyhow::{Result, anyhow, Context};
use bytes::Bytes;
use protobuf::Message;

use crate::{proto::{OpenShellArgs, PtyOutputPacket, pty_output_packet::Pty_output_args, PtyInputPacket, pty_input_packet::Pty_input_args, ShutdownArgs}, channel::{ChSender, ChReceiver}, async_pty_process::{Sender as PtySender, Receiver as PtyRecver, make_async_pty_process}, term::get_shell_program};


pub async fn open_shell(args: OpenShellArgs) -> Result<ChShell> {
    let program = get_shell_program();
    
    let (pty_sender, pty_recver) = match args.program_args.0 {
        Some(v) => {
            // v.env_vars.iter()
            // .map(|x|(x.0.deref(), x.1.deref()));
            make_async_pty_process(
                &program, &["-i"], 
                v.rows as u16, 
                v.cols as u16,
                v.env_vars.iter().map(|x|(x.0.deref(), x.1.deref())),
            ).await?
        },
        None => {
            make_async_pty_process(
                &program, &["-i"], 
                args.rows as u16, 
                args.cols as u16,
                HashMap::<String, String>::new().iter(),
            ).await?
        },
    };
                
    // let (pty_sender, pty_recver) = make_async_pty_process(
    //     &program, &["-i"], 
    //     args.rows as u16, 
    //     args.cols as u16,
    // ).await?;

    // let uid = gen_huid();
    // tracing::debug!("open shell [{}]", uid);
    
    Ok(ChShell {
        // uid,
        pty_sender,
        pty_recver,
    } )
}

pub struct ChShell {
    // uid: HUId,
    pty_sender: PtySender,
    pty_recver: PtyRecver,
}

impl ChShell {
    pub async fn run(self, tx: ChSender, rx: ChReceiver ) -> Result<()> {
        copy_pty_channel(self,  tx, rx).await
    }

    // pub fn spawn<H: CtrlHandler>(self, name: Option<String>, tx: ChSender, rx: ChReceiver, weak: Option<CtrlWeak<H>>, local_ch_id: ChId)  {
    //     let name = name.unwrap_or_else(||format!("shell-{}", self.uid));
    //     spawn_with_name(name, async move {

    //         let r = copy_pty_channel(self,  tx, rx).await;
    //         tracing::debug!("finished with [{:?}]", r);

    //         if let Some(weak) = weak {
    //             if let Some(ctrl) = weak.upgrade() {
    //                 let _r = ctrl.close_channel(local_ch_id).await;
    //             }
    //         }

    //     });
    // }
}

async fn copy_pty_channel(
    mut shell: ChShell,
    ch_tx: ChSender,
    mut ch_rx: ChReceiver,
) -> Result<()> {
    loop {
        tokio::select! {
            r = shell.pty_recver.recv() => {
                match r {
                    Some(data) => {
                        ch_tx.send_data(se_pty_stdout_packet(data)?)
                        .await.map_err(|_x|anyhow!("send pty output failed"))?
                    },
                    None => {
                        // shutdown
                        tracing::debug!("send shutdown");
                        let _r = ch_tx.send_data(se_shutdown_packet()?).await;

                        // // send zero for indicating shutdown
                        // let _r = ch_tx.send_data(se_pty_stdout_packet(Bytes::new())?).await;

                        break
                    },
                }
            },
            r = ch_rx.recv_packet() => {
                let packet = r?;
                if let Some(_shutdown) = process_pty_input_packet(&shell.pty_sender, packet.payload).await? {
                    break;
                }
            }
        }
    }
    Ok(())
}

async fn process_pty_input_packet(pty_sender: &PtySender, data: Bytes) -> Result<Option<ShutdownArgs>> {

    let args = PtyInputPacket::parse_from_tokio_bytes(&data)?
    .pty_input_args.with_context(||"empty pty_input_args")?;

    match args {
        Pty_input_args::StdinData(data) => {
            // use rtun::hex::BinStrLine;
            // tracing::debug!("stdin data {}", data.dump_bin());
            pty_sender.send_data(data).await?;
        },
        Pty_input_args::Resize(args) => {
            pty_sender.send_resize(args.cols as u16, args.rows as u16).await?;
        },
        Pty_input_args::Shutdown(args) => {
            tracing::debug!("recv shutdown {}", args);
            return Ok(Some(args))
        }
    }

    Ok(None)
}

fn se_pty_stdout_packet(data: Bytes) -> Result<Bytes> {
    Ok(PtyOutputPacket {
        pty_output_args: Some(Pty_output_args::StdoutData(data)),
        ..Default::default()
    }
    .write_to_bytes()?
    .into())
}

fn se_shutdown_packet() -> Result<Bytes> {
    Ok(PtyOutputPacket {
        pty_output_args: Some(Pty_output_args::Shutdown(ShutdownArgs{ 
            code: 0,
            ..Default::default() 
        })),
        ..Default::default()
    }
    .write_to_bytes()?
    .into())
}

