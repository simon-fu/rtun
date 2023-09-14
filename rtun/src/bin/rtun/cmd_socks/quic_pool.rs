use std::{collections::HashMap, sync::Arc};
use anyhow::{Result, Context};
use parking_lot::Mutex;
use rtun::{switch::invoker_ctrl::{CtrlHandler, CtrlInvoker}, huid::gen_huid::gen_huid};

use super::quic_session::{QuicSession, make_quic_session, SetCtrl, StreamPair, ReqCh};



pub struct AgentPool<H: CtrlHandler> {
    shared: Arc<Mutex<Work<H>>>,
}

impl<H: CtrlHandler> Clone for AgentPool<H> {
    fn clone(&self) -> Self {
        Self { shared: self.shared.clone() }
    }
}

impl<H: CtrlHandler> AgentPool<H> {
    pub fn new() -> Self {
        Self {
            shared: Default::default(),
        }
    }

    pub async fn set_agent(&self, agent: String, ctrl: CtrlInvoker<H>) -> Result<()> {
        {
            let mut work = self.shared.lock();
            let r = work.sessions.get_mut(&agent);
            if let Some(session) = r {
                session.invoker().invoke(SetCtrl(ctrl)).await??;
                return Ok(())
            }
        }

        let uid = gen_huid();
        let session = make_quic_session(uid, ctrl, &agent)?;

        {
            let mut work = self.shared.lock();
            let r = work.sessions.get_mut(&agent);
            if r.is_none() {
                work.sessions.insert(agent, session);
            }
        }

        Ok(())
    }

    pub async fn get_ch(&self) -> Result<StreamPair> {
        let invoker = {
            let work = self.shared.lock();
            work.sessions.iter()
            .next()
            .map(|x|x.1.invoker().clone())
            .with_context(||"empty session")?
        };
        invoker.invoke(ReqCh).await?
    }
}


struct Work<H: CtrlHandler> {
    sessions: HashMap<String, QuicSession<H>>,
}

impl<H: CtrlHandler> Default for Work<H> {
    fn default() -> Self {
        Self { sessions: Default::default() }
    }
}

#[derive(Debug, Clone)]
pub struct ConnStats {
    pub tx: UniStats,
    pub rx: UniStats,
    pub latency: i64,
    pub update_ts: i64,
}

#[derive(Debug, Clone)]
pub struct UniStats {
    pub bytes: u64,
    pub rate: u64, // bytes per second,
}

// pub struct ConnShared {
//     state: Mutex<ConnState>,
// }

// impl ConnShared {
//     pub fn pop_ch(&self) -> Option<(SendStream, RecvStream)> {
//         self.state.lock().ch_que.pop_front()
//     }

//     pub fn push_ch(&self, ch: (SendStream, RecvStream)) {
//         self.state.lock().ch_que.push_back(ch);
//     }

//     pub fn get_stats(&self) -> ConnStats {
//         self.state.lock().stats.clone()
//     }

//     pub fn set_stats(&self, stats: ConnStats) {
//         self.state.lock().stats = stats;
//     }
// }

// struct ConnState {
//     ch_que: VecDeque<(SendStream, RecvStream)>,
//     stats: ConnStats,
// }



// struct ConnItem {
//     shared: Arc<ConnShared>,
//     need_more_tx: mpsc::Sender<()>,
// }
