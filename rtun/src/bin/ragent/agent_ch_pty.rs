use anyhow::{Result, anyhow, Context};
use bytes::Bytes;
use protobuf::Message;
use rtun::swtich::{SwitchInvoker, AgentEntity};
use rtun::{async_rt::spawn_with_name, huid::gen_huid::gen_huid, proto::{OpenShellArgs, PtyOutputPacket, pty_output_packet::Pty_output_args, PtyInputPacket, pty_input_packet::Pty_input_args, ShutdownArgs}, channel::{ChSender, ChReceiver, ChId}, async_pty_process::{Sender as PtySender, Receiver as PtyRecver, make_async_pty_process}, term::get_shell_program};


pub async fn open_agent_shell<E: AgentEntity>(agent: &SwitchInvoker<E>, args: OpenShellArgs) -> Result<ChId> {
    let program = get_shell_program();
                
    let (pty_sender, pty_recver) = make_async_pty_process(
        &program, &["-i"], 
        args.rows as u16, 
        args.cols as u16,
    ).await?;

    // let ch_id = ChId(args.ch_id);
    let (tx, rx) = agent.alloc_channel().await?.split();
    let ch_id = tx.ch_id();

    let uid = gen_huid();
    tracing::debug!("open shell {:?}, [{}]", tx.ch_id(), uid);

    let weak = agent.downgrade();
    
    spawn_with_name(format!("shell-{}", uid), async move {
        let r = copy_pty_channel(pty_sender, pty_recver, tx, rx).await;
        tracing::debug!("finished with [{:?}]", r);
        if let Some(invoker) = weak.upgrade() {
            let _r = invoker.remote_channel(ch_id).await;
        }
    });

    Ok(ch_id)
}

async fn copy_pty_channel(
    pty_sender: PtySender,
    mut pty_recver: PtyRecver,
    ch_tx: ChSender,
    mut ch_rx: ChReceiver,
) -> Result<()> {
    loop {
        tokio::select! {
            r = pty_recver.recv() => {
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
            r = ch_rx.recv_data() => {
                match r {
                    Some(data) => {
                        if let Some(_shutdown) = process_pty_input_packet(&pty_sender, data).await? {
                            break;
                        }
                    },
                    None => break,
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
        _ => {
            tracing::debug!("unknown pty_input_args")
        },
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

