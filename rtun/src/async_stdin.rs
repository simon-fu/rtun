use std::sync::Arc;

use anyhow::Result;
use bytes::{BytesMut, Bytes};
use parking_lot::Mutex;
use tokio::sync::watch;


pub fn async_std_in() -> AsyncStdin { 

    let (tx, rx) = watch::channel(());

    let shared = Arc::new(Shared {
        tx,
        data: Default::default(),
    });
    
    spawn_read_thread(shared.clone());

    AsyncStdin {
        rx,
        shared,
    }
}

fn spawn_read_thread(shared: Arc<Shared>) {
    let _r = std::thread::spawn(move || {
        use std::io::Read;

        let mut fin = std::io::stdin();
        let mut input = vec![0_u8; 1024];

        loop{

            match fin.read(input.as_mut()) {
                Ok(0) => {
                    shared.data.lock().end_reason = Some(EndReason::EOF);
                    break;
                },
                Ok(n) => {
                    {
                        let mut data = shared.data.lock();
                        data.buf.extend_from_slice(&input[..n]);
                    }

                    let r = shared.tx.send(());
                    if r.is_err() {
                        shared.data.lock().end_reason = Some(EndReason::Dropped);
                        break;
                    }
                },
                Err(e) => {
                    // eprintln!("read stdin error : {}" , e);
                    shared.data.lock().end_reason = Some(EndReason::Error(e.into()));
                    break;
                },
            };
        }
    });
}

type Recver = watch::Receiver<()>;
type Sender = watch::Sender<()>;

#[derive(Clone)]
pub struct AsyncStdin {
    rx: Recver,
    shared: Arc<Shared>,
}

impl AsyncStdin {
    pub async fn read(&mut self) -> Result<Bytes, EndReason> {
        loop {
            {
                let mut data = self.shared.data.lock();
                if data.buf.len() > 0 {
                    return Ok(data.buf.split().freeze())
                }

                if let Some(reason) = data.end_reason.as_ref() {
                    return Err(reason.clone())
                }
            }

            let _r = self.rx.changed().await;
        }
    }
}


struct Shared {
    tx: Sender,
    data: Mutex<SharedData>,
}


#[derive(Default)]
struct SharedData {
    buf: BytesMut,
    end_reason: Option<EndReason>,
}

#[derive(Debug, Clone)]
pub enum EndReason {
    EOF,
    Error(Arc<std::io::Error>),
    Dropped,
}

impl std::fmt::Display for EndReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self, f)
    }
}

impl std::error::Error for EndReason {
    
}