use bytes::Bytes;
use tokio::sync::mpsc;


#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChId(pub u64);

pub struct ChData {
    pub ch_id: ChId,
    pub payload: Bytes,
}

pub struct ChSender {
    ch_id: ChId,
    outgoing_tx: mpsc::Sender<ChData>,
}

impl ChSender {
    pub fn new(ch_id: ChId, outgoing_tx: mpsc::Sender<ChData>) -> Self {
        Self {
            ch_id,
            outgoing_tx,
        }
    }

    pub fn ch_id(&self) -> ChId {
        self.ch_id
    }

    pub async fn send_data(&self, data: Bytes) -> Result<(), Bytes> {
        self.outgoing_tx.send(ChData { 
            ch_id: self.ch_id, 
            payload: data, 
        }).await
        .map_err(|x|x.0.payload)
    }
}

pub struct ChReceiver {
    rx: mpsc::Receiver<Bytes>,
}

impl ChReceiver {
    pub fn new(rx: mpsc::Receiver<Bytes>) -> Self {
        Self {
            rx,
        }
    }

    pub async fn recv_data(&mut self) -> Option<Bytes> {
        self.rx.recv().await
    }
}


// pub struct OpAddChannel;

// #[async_trait::async_trait]
// impl AsyncHandler<OpAddChannel> for Entity {
//     type Response = Result<(ChId, ChSender, ChReceiver)>; 

//     async fn handle(&mut self, _req: OpAddChannel) -> Self::Response {
//         let ch_id = self.next_ch_id();
//         let (tx, rx) = mpsc::channel(256);
//         self.channels.insert(ch_id, ChannelItem { tx });
        
//         Ok((
//             ch_id, 
//             ChSender::new(ch_id, self.outgoing_tx.clone()),
//             ChReceiver::new(rx),
//         ))
//     }
// }


