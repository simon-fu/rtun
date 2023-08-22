

use anyhow::{Result, anyhow, bail};

use bytes::Bytes;
use rtun::{async_stdin::async_std_in, channel::{ChSender, ChReceiver}, pty::PtyEvent};

use crate::client_ch_pty::{PtyChSender, PtyChReceiver, process_recv_result};

pub async fn run(tx: ChSender, rx: ChReceiver) -> Result<()> {
    crossterm::terminal::enable_raw_mode()?;
    let result = do_run(tx, rx).await;
    crossterm::terminal::disable_raw_mode()?;
    result
}

pub async fn do_run(ch_tx: ChSender, ch_rx: ChReceiver) -> Result<()> {
    
    let tx = PtyChSender::new(ch_tx);
    let mut rx = PtyChReceiver::new(ch_rx);

    let mut detector = PatternDetector::new(Bytes::from(
        vec![0x1a, 0x01, 0x1a, 0x01], // ctrl + 'zaza'
    ));

    let mut fin = async_std_in();
    
    loop {
        tokio::select! {
            r = fin.read() => {
                let data = r?;

                if data.len() > 0 {
                    if detector.detect(&data) {
                        bail!("force exit")
                    }
                    tx.send_event(PtyEvent::StdinData(data.to_vec().into())).await.map_err(|_e|anyhow!("send_data fail"))?; 
                }
            },
            r = rx.recv_packet() => {
                process_recv_result(r).await?;
            }
        }
    }
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
