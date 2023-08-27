use anyhow::{Result, anyhow, Context};
use bytes::Bytes;
use protobuf::Message;
use rtun::{channel::{ChSender, ChReceiver}, proto::{PtyOutputPacket, PtyInputPacket, pty_input_packet::Pty_input_args, PtyResizeArgs, ShutdownArgs}, pty::{PtyEvent, PtySize}};

pub struct PtyChReceiver {
    ch_rx: ChReceiver,
}

impl PtyChReceiver {
    pub fn new(ch_rx: ChReceiver) -> Self {
        Self { ch_rx }
    }

    // pub async fn recv_packet(&mut self, ) -> Result<Option<PtyOutputPacket>> {
    //     let r = self.ch_rx.recv_packet().await;
    //     match r {
    //         Some(packet) => {
    //             Ok(Some(
    //                 PtyOutputPacket::parse_from_tokio_bytes(&packet.payload)
    //                 .with_context(||"invalid PtyInputPacket")?
    //             ))
    //         },
    //         None => Ok(None),
    //     }
    // }

    pub async fn recv_packet(&mut self, ) -> Result<PtyOutputPacket> {
        let packet = self.ch_rx.recv_packet().await?;
        Ok(
            PtyOutputPacket::parse_from_tokio_bytes(&packet.payload)
            .with_context(||"invalid PtyOutputPacket")?
        )
    }
}

pub struct PtyChSender {
    ch_tx: ChSender,
}

impl PtyChSender { 
    pub fn new(ch_tx: ChSender) -> Self {
        Self { ch_tx }
    }

    pub async fn send_event(&self, ev: PtyEvent) -> Result<()> {
        let data = match ev {
            PtyEvent::StdinData(data) => {
                se_pty_stdin_packet(data)?
            },
            PtyEvent::Resize(size) => {
                se_pty_resize_packet(size)?
            },
        };

        self.ch_tx.send_data(data)
        .await.map_err(|_x|anyhow!("send data failed"))
    }
}

pub async fn process_recv_result(result: Result<PtyOutputPacket>) -> Result<Option<ShutdownArgs>> {
    use std::io::Write;
    use rtun::proto::pty_output_packet::Pty_output_args;

    let args = result.with_context(||"recv packet failed")?
    // .with_context(||"channel closed")?
    .pty_output_args.with_context(||"empty pty_output_args")?;

    match args {
        Pty_output_args::StdoutData(data) => {
            // use rtun::hex::BinStrLine;
            // tracing::debug!("{}", d.bin_str());
            
            // if data.len() == 0 {
            //     return Err(std::io::Error::from(std::io::ErrorKind::WriteZero).into())
            // }

            let stdout = std::io::stdout();
            let mut stdout = stdout.lock();
            stdout.write_all(&data[..])?;
            stdout.flush()?;
        },
        Pty_output_args::Shutdown(v) => {
            tracing::debug!("recv shutdown packet {}", v);
            return Ok(Some(v))
        },
        _ => {
            tracing::debug!("recv unknown packet");
        },
    }

    Ok(None)
}

fn se_pty_stdin_packet(data: Bytes) -> Result<Bytes> {
    Ok(PtyInputPacket {
        pty_input_args: Some(Pty_input_args::StdinData(data)),
        ..Default::default()
    }
    .write_to_bytes()?
    .into())
}

fn se_pty_resize_packet(size: PtySize) -> Result<Bytes> {
    Ok(PtyInputPacket {
        pty_input_args: Some(Pty_input_args::Resize(PtyResizeArgs {
            cols: size.cols as u32,
            rows: size.rows as u32,
            ..Default::default()
        })),
        ..Default::default()
    }
    .write_to_bytes()?
    .into())
}
