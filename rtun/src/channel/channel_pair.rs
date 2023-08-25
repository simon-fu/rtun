use bytes::Bytes;
use tokio::sync::mpsc;


#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChId(pub u64);

impl std::fmt::Display for ChId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

pub struct ChPacket {
    pub ch_id: ChId,
    pub payload: Bytes,
}

pub struct ChPair {
    pub tx: ChSender,
    pub rx: ChReceiver,
}

impl ChPair {
    pub fn new(ch_id: ChId) -> Self {
        let (tx, rx) = mpsc::channel(CHANNEL_SIZE);
        Self { 
            tx: ChSender::new(ch_id, tx), 
            rx: ChReceiver::new(rx),
        }
    }
}

impl ChPair {
    #[inline]
    pub fn split(self) -> (ChSender, ChReceiver) {
        (self.tx, self.rx)
    }
}

pub type ChTx = mpsc::Sender<ChPacket>;
pub type ChRx = mpsc::Receiver<ChPacket>;

#[derive(Debug)]
pub struct ChSender {
    ch_id: ChId,
    outgoing_tx: ChTx,
}

impl ChSender {
    pub fn new(ch_id: ChId, outgoing_tx: ChTx) -> Self {
        Self {
            ch_id,
            outgoing_tx,
        }
    }

    pub fn ch_id(&self) -> ChId {
        self.ch_id
    }

    pub async fn send_data(&self, data: Bytes) -> Result<(), Bytes> {
        self.outgoing_tx.send(ChPacket { 
            ch_id: self.ch_id, 
            payload: data, 
        }).await
        .map_err(|x|x.0.payload)
    }
}

pub struct ChReceiver {
    rx: ChRx,
}

impl ChReceiver {
    pub fn new(rx: ChRx) -> Self {
        Self {
            rx,
        }
    }

    pub async fn recv_data(&mut self) -> Option<ChPacket> {
        self.rx.recv().await
    }
}

pub const CHANNEL_SIZE: usize = 256;

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

