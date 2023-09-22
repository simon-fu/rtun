use std::{collections::HashMap, sync::Arc, time::Duration};
use anyhow::{Result, Context, bail};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use quinn::{SendStream, RecvStream};
// use parking_lot::Mutex;
use quinn_proto::ConnectionStats;
use rtun::{switch::{invoker_ctrl::{CtrlHandler, CtrlInvoker}, session_stream::make_stream_session}, ice::{ice_quic::{QuicConn, QuicIceCert, UpgradeToQuic}, ice_peer::{IcePeer, IceConfig, IceArgs}}, proto::{QuicStats, P2PArgs, QuicSocksArgs, p2pargs::P2p_args, P2PQuicArgs, open_p2presponse::Open_p2p_rsp}, actor_service::{ActorHandle, start_actor, handle_first_none, handle_msg_none, Action, ActorEntity, AsyncHandler, Invoker}, ws::client::ws_connect_to, async_rt::spawn_with_name};
use tokio::{time::{MissedTickBehavior, Interval}, sync::mpsc};

// use super::quic_session::{QuicSession, make_quic_session, SetCtrl, StreamPair, ReqCh};

pub type StreamPair = (SendStream, RecvStream);

// pub struct AgentPool<H: CtrlHandler> {
//     shared: Arc<Mutex<Work<H>>>,
//     multi: MultiProgress,
//     prefix: String,
// }

// impl<H: CtrlHandler> Clone for AgentPool<H> {
//     fn clone(&self) -> Self {
//         Self { 
//             shared: self.shared.clone(), 
//             multi: self.multi.clone(),
//             prefix: self.prefix.clone(),
//         }
//     }
// }

// impl<H: CtrlHandler> AgentPool<H> {
//     pub fn new(multi: MultiProgress, prefix: String) -> Self {
//         Self {
//             shared: Default::default(),
//             multi,
//             prefix,
//         }
//     }

//     pub async fn set_agent(&self, agent: String, ctrl: CtrlInvoker<H>) -> Result<()> {
//         {
//             let mut work = self.shared.lock();
//             let r = work.agents.get_mut(&agent);
//             if let Some(session) = r {
//                 session.invoker().invoke(SetCtrl(ctrl)).await??;
//                 return Ok(())
//             }
//         }

//         let bar = self.multi.add(ProgressBar::new(100));
//         let style = ProgressStyle::with_template("{prefix:.bold.dim} {spinner} {wide_msg}")?
//         .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ ");
//         bar.set_style(style);
//         bar.set_prefix(self.prefix.clone());

//         let uid = gen_huid();
//         let session = make_quic_session(uid, ctrl, &agent, bar)?;

//         {
//             let mut work = self.shared.lock();
//             let r = work.agents.get_mut(&agent);
//             if r.is_none() {
//                 work.agents.insert(agent, session);
//             }
//         }

//         Ok(())
//     }

//     pub async fn get_ch(&self) -> Result<StreamPair> {
//         let invoker = {
//             let work = self.shared.lock();
//             work.agents.iter()
//             .next()
//             .map(|x|x.1.invoker().clone())
//             .with_context(||"empty session")?
//         };
//         invoker.invoke(ReqCh).await?
//     }
// }

// struct Work<H: CtrlHandler> {
//     agents: HashMap<String, QuicSession<H>>,
// }

// impl<H: CtrlHandler> Default for Work<H> {
//     fn default() -> Self {
//         Self { agents: Default::default() }
//     }
// }






pub type QuicPoolInvoker = Invoker<Entity>;
pub type QuicPool = ActorHandle<Entity>;

pub fn make_pool(name: String, multi: MultiProgress) -> Result<QuicPool> {
    
    let mut interval = tokio::time::interval(Duration::from_millis(2000));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let (event_tx, event_rx) = mpsc::channel(128);

    let entity = Entity {
        agents: Default::default(),
        selected_agent: None,
        selected_conn: None,
        multi,
        interval,
        event_rx,
        event_tx,
    };
    
    let actor = start_actor(
        name, 
        entity, 
        handle_first_none, 
        wait_next, 
        handle_next, 
        handle_msg_none,
    );

    Ok(actor)
}

async fn wait_next(entity: &mut Entity) -> Next {
    tokio::select! {
        _r = entity.interval.tick() => {}
        r = entity.event_rx.recv() => {
            if let Some(event) = r {
                entity.handle_event(event).await;
            }
        }
    }
}

async fn handle_next(entity: &mut Entity, _next: Next) -> Result<Action>  {
    entity.handle_next()?;
    Ok(Action::None)
}

type Next = ();

type Msg = ();

type EntityResult = ();

impl ActorEntity for Entity {
    type Next = Next;

    type Msg = Msg;

    type Result = EntityResult;

    fn into_result(self, _r: Result<()>) -> Self::Result {
        ()
    }
}

#[derive(Debug)]
pub struct AddAgent {
    pub name: String,
    pub url: String,
}


#[async_trait::async_trait]
impl AsyncHandler<AddAgent> for Entity {
    type Response = Result<()>; 

    async fn handle(&mut self, req: AddAgent) -> Self::Response {
        let name = req.name.clone();
        let agent = Arc::new(AgentShared {
            name: req.name,
            url: req.url,
        });

        let mut conns = Vec::new();
        
        for nn in 1..3 {

            let bar = self.multi.add(ProgressBar::new(100));
            let style = ProgressStyle::with_template("{prefix:.bold.dim} {spinner} {wide_msg}")?
            .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ ");
            bar.set_style(style);
            bar.set_prefix(format!("{name}-{nn}"));

            let tx = kick_connecting(nn, agent.clone(), self.event_tx.clone(), bar, "add agent");
            conns.push(ConnSlot {
                id: nn,
                state: ConnState::Connecting(tx),
                agent: agent.clone(),
                event_tx: self.event_tx.clone(),
            });
        }

        self.agents.insert(name, AgentSlot {
            shared: agent,
            conns,
            rrobin: 0,
        });

        Ok(())
    }
}

#[derive(Debug)]
pub struct GetCh;

#[async_trait::async_trait]
impl AsyncHandler<GetCh> for Entity {
    type Response = Result<Option<StreamPair>>; 

    async fn handle(&mut self, _req: GetCh) -> Self::Response {
        self.get_ch().await
    }
}


pub struct Entity {
    agents: HashMap<String, AgentSlot>,
    selected_agent: Option<String>,
    selected_conn: Option<usize>,
    multi: MultiProgress,
    interval: Interval,
    event_tx: mpsc::Sender<Event>,
    event_rx: mpsc::Receiver<Event>,
}

impl Entity {
    fn handle_next(&mut self) -> Result<()> {
        // let mut futs = FuturesUnordered::new();

        for (_name, agent) in self.agents.iter_mut() {
            for conn in agent.conns.iter_mut() {
                match &mut conn.state {
                    ConnState::Working(work) => {
                        if work.conn.is_ping_timeout() {
                            if let Some(bar) = work.bar.take() {
                                let tx = kick_connecting(conn.id, agent.shared.clone(), self.event_tx.clone(), bar, "ping timeout");
                                conn.state = ConnState::Connecting(tx);
                            }
                        } else {
                            if let Some(remote) = work.conn.try_remote_stats() {
                                let local = work.conn.stats();
                                if let Some(bar) = work.bar.as_ref() {
                                    update_bar_stats(bar, &local, &remote);
                                }
                            }
                        }
                    },
                    ConnState::Connecting(_tx) => { },
                }

            }
        }

        Ok(())
    }

    async fn handle_event(&mut self, event: Event) {
        match event {
            Event::Connected { conn_id, conn, agent, bar } => {
                if let Some(agent_slot) = self.agents.get_mut(&agent.name) {
                    for slot in agent_slot.conns.iter_mut() {
                        if slot.id == conn_id {
                            // tracing::debug!("do found conn [{}] [{}]", agent.name, conn_id);
                            slot.state = ConnState::Working(ConnWork { conn, bar: Some(bar), });
                            return;
                        }
                    }
                }
                tracing::debug!("NOT found conn [{}] [{}]", agent.name, conn_id);
            },
        } 
    }

    async fn get_ch(&mut self) -> Result<Option<StreamPair>> {
        let r = self.open_selected().await?;
        if let Some(pair) = r {
            return Ok(Some(pair))
        }
        
        self.selected_agent = None;
        self.selected_conn = None;

        for (name, agent) in self.agents.iter_mut() {
            let r = agent.open_rand().await;
            if let Some(pair) = r {
                self.selected_agent = Some(name.clone());
                return Ok(Some(pair))
            }
        }

        Ok(None)
    }

    async fn open_selected(&mut self) -> Result<Option<StreamPair>> {
        if let Some(selected) = &self.selected_agent {
            if let Some(agent) = self.agents.get_mut(selected) {
                if let Some(index) = self.selected_conn {
                    
                    let conn_slot = &mut agent.conns[index];

                    if let Some(pair) = conn_slot.open_ch().await {
                        return Ok(Some(pair))
                    }

                    self.selected_conn = None;
                }

                let r = agent.open_rand().await;
                return Ok(r)
            }
        }

        Ok(None)

    }
}

fn kick_connecting(conn_id: u64, agent: Arc<AgentShared>, event_tx: mpsc::Sender<Event>, bar: ProgressBar, origin: &str) -> mpsc::Sender<()> {
    tracing::info!("kick connecting [{}-{}] [{origin}]", agent.name, conn_id,);

    update_bar_connecting(&bar);

    let (guard_tx, guard_rx) = mpsc::channel(1);
    spawn_with_name(format!("{}-conn-{conn_id}", agent.name), async move {
        connecting_task(conn_id, agent, event_tx, bar, guard_rx).await
    });
    guard_tx
}

async fn connecting_task(conn_id: u64, agent: Arc<AgentShared>, tx: mpsc::Sender<Event>, bar: ProgressBar, mut guard_rx: mpsc::Receiver<()>) {
    loop {
        tracing::debug!("connecting");
        let r = tokio::select! {
            r = try_connet(agent.url.as_str()) => r,
            _r = guard_rx.recv() => {
                tracing::debug!("dropped guard");
                return
            }
        };

        match r {
            Ok(conn) => {
                tracing::debug!("successfully connected");
                let _r = tx.send(Event::Connected { 
                    conn_id,
                    conn,
                    agent, 
                    bar, 
                }).await;
                return;
            },
            Err(e) => {
                tracing::debug!("connect failed [{e:?}]");
            },
        }
        tokio::time::sleep(Duration::from_millis(5000)).await;
    }
}

async fn try_connet(url_str: &str) -> Result<QuicConn> {
    tracing::debug!("connecting to {url_str}");
    let (stream, _r) = ws_connect_to(url_str).await
    .with_context(||format!("connect to agent failed"))?;
    let session = make_stream_session(stream.split(), false).await?;
    let ctrl = session.ctrl_client().clone_invoker();
    punch(ctrl).await
}

async fn punch<H: CtrlHandler>(ctrl: CtrlInvoker<H>) -> Result<QuicConn> {
    let ice_servers = vec![
        "stun:stun1.l.google.com:19302".into(),
        "stun:stun2.l.google.com:19302".into(),
        "stun:stun.qq.com:3478".into(),
    ];

    let mut peer = IcePeer::with_config(IceConfig {
        servers: ice_servers.clone(),
        ..Default::default()
    });

    tracing::debug!("kick gather candidate");
    let local_args = peer.client_gather().await?;
    tracing::debug!("local args {local_args:?}");

    let local_cert = QuicIceCert::try_new()?;
    let cert_der = local_cert.to_bytes()?.into();
    
    let rsp = ctrl.open_p2p(P2PArgs {
        p2p_args: Some(P2p_args::QuicSocks(QuicSocksArgs {
            base: Some(P2PQuicArgs {
                ice: Some(local_args.into()).into(),
                cert_der,
                ..Default::default()
            }).into(),
            ..Default::default()
        })),
        ..Default::default()
    }).await?;

    let rsp = rsp.open_p2p_rsp.with_context(||"no open_p2p_rsp")?;
    match rsp {
        Open_p2p_rsp::Args(mut remote_args) => {
            if !remote_args.has_quic_socks() {
                bail!("no quic socks")
            }

            let mut args = remote_args.take_quic_socks()
            .base.0.with_context(||"no base in quic socks")?;
            let remote_args: IceArgs = args.ice.take().with_context(||"no ice in quic args")?.into();
            
            // let local_cert = QuicIceCert::try_new()?;
            let server_cert_der = args.cert_der;

            tracing::debug!("remote args {remote_args:?}");
            let conn = peer.dial(remote_args).await?
            .upgrade_to_quic(&local_cert, server_cert_der).await?;
            
            return Ok(conn)
        },
        Open_p2p_rsp::Status(s) => {
            bail!("open p2p but {s:?}");
        },
        _ => {
            bail!("unknown Open_p2p_rsp {rsp:?}");
        }
    }  
}

struct AgentSlot {
    shared: Arc<AgentShared>,
    conns: Vec<ConnSlot>,
    rrobin: usize,
}

impl AgentSlot {
    async fn open_rand(&mut self) -> Option<StreamPair> {
        for _ in 0..self.conns.len() {
            let index = self.next_robin();
            let conn_slot = &mut self.conns[index];

            if let Some(pair) = conn_slot.open_ch().await {
                return Some(pair)
            }
        }
        None
    }

    fn next_robin(&mut self) -> usize {
        let index = self.rrobin;
        self.rrobin += 1;
        if self.rrobin >= self.conns.len() {
            self.rrobin = 0;
        }
        index
    }
}

struct AgentShared {
    name: String,
    url: String,
}


struct ConnSlot {
    id: u64,
    state: ConnState,
    event_tx: mpsc::Sender<Event>,
    agent: Arc<AgentShared>,
}

impl ConnSlot {
    async fn open_ch(&mut self) -> Option<StreamPair> {
        if let ConnState::Working(work) = &mut self.state {
            let r = work.conn.open_bi().await;
            match r {
                Ok(pair) => {
                    return Some(pair)
                },
                Err(_e) => {
                    // tracing::info!("kick connecting for open ch failed [{e:?}]");
                    if let Some(bar) = work.bar.take() {
                        let tx = kick_connecting(self.id, self.agent.clone(), self.event_tx.clone(), bar, "open_bi failed");
                        self.state = ConnState::Connecting(tx);
                    }
                },
            }
        }
        None
    }
}

enum ConnState {
    Working(ConnWork),
    Connecting(mpsc::Sender<()>),
}

enum Event {
    Connected {
        conn_id: u64,
        conn: QuicConn,
        agent: Arc<AgentShared>,
        bar: ProgressBar,
    }
}


struct ConnWork {
    conn: QuicConn,
    bar: Option<ProgressBar>,
}



fn update_bar_connecting(bar: &ProgressBar) {
    bar.set_message("connecting...");
}

fn update_bar_stats(bar: &ProgressBar, local: &ConnectionStats, remote: &QuicStats, ) {
    let remote_rtt = remote.path().rtt();
    let remote_cwnd = remote.path().cwnd();

    let path = &local.path;
    bar.set_message(format!(
        "rtt {}/{}, cwnd {}/{}, tx {}, rx {}", 
        path.rtt.as_millis(),
        remote_rtt,
        indicatif::HumanBytes(path.cwnd),
        indicatif::HumanBytes(remote_cwnd),
        indicatif::HumanBytes(local.udp_tx.bytes),
        indicatif::HumanBytes(local.udp_rx.bytes),
    ));

    // bar.set_message(format!(
    //     "rtt {}/{remote_rtt}, cwnd {}/{remote_cwnd}, ev {}, lost {}/{}, sent {}, pl {}/{}, black {}", 
    //     path.rtt.as_millis(),
    //     path.cwnd,
    //     path.congestion_events,
    //     path.lost_packets,
    //     path.lost_bytes,
    //     path.sent_packets,
    //     path.sent_plpmtud_probes,
    //     path.lost_plpmtud_probes,
    //     path.black_holes_detected,
    // ));
}

// #[derive(Debug, Clone)]
// pub struct ConnStats {
//     pub tx: UniStats,
//     pub rx: UniStats,
//     pub latency: i64,
//     pub update_ts: i64,
// }

// #[derive(Debug, Clone)]
// pub struct UniStats {
//     pub bytes: u64,
//     pub rate: u64, // bytes per second,
// }

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
