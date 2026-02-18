use anyhow::Result;
use tokio::sync::broadcast;

pub struct CtrlGuard(broadcast::Sender<()>);

impl CtrlGuard {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(1);
        Self(tx)
    }

    pub fn watch(&self) -> CtrlWatch {
        CtrlWatch(self.0.subscribe())
    }
}

pub struct CtrlWatch(broadcast::Receiver<()>);

impl CtrlWatch {
    pub async fn watch(&mut self) {
        let _r = self.0.recv().await;
    }
}

#[derive(Debug)]
pub struct OpWatch;

pub type WatchResult = Result<CtrlWatch>;
