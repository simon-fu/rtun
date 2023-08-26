

use anyhow::{Result, anyhow, bail};

use bytes::Bytes;
use futures::StreamExt;
use rtun::{channel::{ChSender, ChReceiver, ChPair}, pty::PtyEvent, term::async_input::make_async_input};


use crate::client_ch_pty::{PtyChSender, PtyChReceiver, process_recv_result};




pub async fn run_term(pair: ChPair) -> Result<()> {
    
    crossterm::terminal::enable_raw_mode()?;

    let result = do_run(pair.tx, pair.rx).await;

    crossterm::terminal::disable_raw_mode()?;

    result
}

pub async fn do_run(ch_tx: ChSender, ch_rx: ChReceiver) -> Result<()> {
    
    let tx = PtyChSender::new(ch_tx);
    let mut rx = PtyChReceiver::new(ch_rx);

    let mut detector = PatternDetector::new(Bytes::from(
        vec![0x1a, 0x01, 0x1a, 0x01], // ctrl + 'zaza'
    ));

    let mut input = make_async_input()?;
    tracing::debug!("running input loop\r");

    loop {
        tokio::select! {
            r = input.next() => {
                let ev = match r {
                    Some(r) => r?,
                    None => break,
                };

                if let PtyEvent::StdinData(data) = &ev {
                    if detector.detect(&data) {
                        bail!("match input pattern, force exit")
                    }
                }
                tx.send_event(ev).await.map_err(|_e|anyhow!("send_data fail"))?; 
            },
            r = rx.recv_packet() => {
                if let Some(_shutdown) = process_recv_result(r).await? {
                    break;
                }
            }
        }
    }

    Ok(())
    // let mut fin = async_std_in();
    
    // loop {
    //     tokio::select! {
    //         r = fin.read() => {
    //             let data = r?;

    //             if data.len() > 0 {
    //                 if detector.detect(&data) {
    //                     bail!("force exit")
    //                 }
    //                 tx.send_event(PtyEvent::StdinData(data.to_vec().into())).await.map_err(|_e|anyhow!("send_data fail"))?; 
    //             }
    //         },
    //         r = rx.recv_packet() => {
    //             process_recv_result(r).await?;
    //         }
    //     }
    // }
}

#[derive(Debug)]
struct PatternDetector {
    pattern: Bytes,
    index: usize,
}

impl PatternDetector  {
    pub fn new(pattern: Bytes) -> Self {
        Self {
            pattern,
            index: 0,
        }
    }

    pub fn detect(&mut self, data: &[u8]) -> bool {

        // const MAGIC: &[u8] = b"zaza";

        for item in data.iter() {
            if self.pattern[self.index] == *item {
                self.index += 1;
                if self.index >= self.pattern.len() {
                    self.index = 0;
                    return true;
                } else {
                    return false
                }
            }
        }

        self.index = 0;
        false
    }
}


// use termwiz::{terminal::{ScreenSize, new_terminal, Terminal}, caps::Capabilities};
// pub async fn get_terminal_size() -> Result<ScreenSize> {
//     let size = tokio::task::spawn_blocking(|| {
//         let caps = Capabilities::new_from_env()?;
//         let mut terminal = new_terminal(caps)?;
//         let size  = terminal.get_screen_size()?;
//         Result::<_>::Ok(size)
//     }).await??;
//     Ok(size)
// }
