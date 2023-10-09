use std::{collections::HashMap, sync::Arc, time::Duration};
use anyhow::{Result, Context, bail};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use quinn::{SendStream, RecvStream};
// use parking_lot::Mutex;
use quinn_proto::ConnectionStats;
use rtun::{switch::{invoker_ctrl::{CtrlHandler, CtrlInvoker}, session_stream::make_stream_session}, ice::{ice_quic::{QuicConn, QuicIceCert, UpgradeToQuic}, ice_peer::{IcePeer, IceConfig, IceArgs}}, proto::{QuicStats, P2PArgs, QuicSocksArgs, p2pargs::P2p_args, P2PQuicArgs, open_p2presponse::Open_p2p_rsp}, actor_service::{ActorHandle, start_actor, handle_first_none, handle_msg_none, Action, ActorEntity, AsyncHandler, Invoker}, ws::client::ws_connect_to, async_rt::spawn_with_name};
use tokio::{time::{MissedTickBehavior, Interval, Instant}, sync::mpsc};

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

const STYLE_GENERAL: &str = "{prefix:.bold.dim} {spinner} {wide_msg:}";
const STYLE_SELECTED_AGENT: &str = "{prefix:.bold.dim} {spinner} {wide_msg:.blue}";
const STYLE_SELECTED_CONN: &str = "{prefix:.bold.dim} {spinner} {wide_msg:.green}";

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
            bar.set_prefix(format!("{name}-{nn}"));
            let bar = Bar::new(bar);
            bar.update_style(STYLE_GENERAL)?;

            // let bar = self.multi.add(ProgressBar::new(100));
            // let style = ProgressStyle::with_template(STYLE_GENERAL)?
            // .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ ");
            // bar.set_style(style);
            // bar.set_prefix(format!("{name}-{nn}"));

            let check_speed = nn == 1;
            let tx = kick_connecting(nn, agent.clone(), self.event_tx.clone(), bar, check_speed, "add agent");
            conns.push(ConnSlot {
                id: nn,
                state: ConnState::Connecting(tx),
                agent: agent.clone(),
                event_tx: self.event_tx.clone(),
                check_speed,
            });
        }

        self.agents.insert(name, AgentSlot {
            // shared: agent,
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

        let mut has_timeout = false;

        for (_name, agent) in self.agents.iter_mut() {
            for conn in agent.conns.iter_mut() {
                
                match &mut conn.state {
                    ConnState::Working(work) => {
                        if work.conn.is_ping_timeout() {
                            conn.kick_connecting("ping timeout");
                            has_timeout = true;
                        } else {
                            work.update_stats();
                        }
                    },
                    ConnState::Connecting(_tx) => { },
                }

            }
        }

        if has_timeout {
            if let Some(agent) = self.selected_agent.as_ref() {
                if let Some(agent) = self.agents.get_mut(agent) {
                    let mut has_work = false;
                    for (index, conn) in agent.conns.iter_mut().enumerate() {
                        match &conn.state {
                            ConnState::Working(_) => {
                                has_work = true;
                            },
                            ConnState::Connecting(_) => {
                                if self.selected_conn == Some(index) {
                                    self.selected_conn = None;
                                }
                            },
                        }
                    }

                    if !has_work {
                        self.selected_agent = None;
                    }
                }
            }
        }

        Ok(())
    }

    async fn handle_event(&mut self, event: Event) {
        match event {
            Event::Connected { conn_id, conn, agent, bar, speed } => {
                if let Some(agent_slot) = self.agents.get_mut(&agent.name) {
                    for (index, slot) in agent_slot.conns.iter_mut().enumerate() {
                        if slot.id == conn_id {
                            // tracing::debug!("do found conn [{}] [{}]", agent.name, conn_id);
                            slot.state = ConnState::Working(ConnWork { conn, bar: Some(bar), });

                            if self.selected_agent.as_deref() == Some(agent.name.as_str()) {
                                if speed.is_some() {
                                    let _r = self.change_selected_conn(index);
                                }
                            }

                            return;
                        }
                    }
                }
                tracing::debug!("NOT found conn [{}] [{}]", agent.name, conn_id);
            },
        } 
    }

    fn change_selected_conn(&mut self, new_index: usize) -> Result<()> {
        if let Some(agent) = self.selected_agent.as_ref() {
            if let Some(agent) = self.agents.get_mut(agent) {

                if let Some(old) = self.selected_conn {
                    agent.conns[old].update_style(STYLE_SELECTED_AGENT)?;
                }

                agent.conns[new_index].update_style(STYLE_SELECTED_CONN)?;
                self.selected_conn = Some(new_index);
            }
        }

        Ok(())
    }
    
    fn clean_selected(&mut self) -> Result<()> {
        if let Some(agent) = self.selected_agent.take() {
            if let Some(agent) = self.agents.get_mut(&agent) {
                for conn in agent.conns.iter_mut() {
                    conn.update_style(STYLE_GENERAL)?;
                }
            }
        }

        self.selected_agent = None;
        self.selected_conn = None;

        Ok(())
    }

    async fn get_ch(&mut self) -> Result<Option<StreamPair>> {
        let r = self.open_selected().await?;
        if let Some(pair) = r {
            return Ok(Some(pair))
        }
        
        self.clean_selected()?;

        for (name, agent) in self.agents.iter_mut() {
            let r = agent.open_priority().await;
            if let Some((pair, index)) = r {
                self.selected_agent = Some(name.clone());
                self.selected_conn = Some(index);
                
                for (ci, conn) in agent.conns.iter_mut().enumerate() {
                    if ci == index {
                        conn.update_style(STYLE_SELECTED_CONN)?;
                    } else {
                        conn.update_style(STYLE_SELECTED_AGENT)?;
                    }
                }

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

                let r = agent.open_priority().await;
                return match r {
                    Some((pair, index)) => {
                        self.selected_conn = Some(index);
                        agent.conns[index].update_style(STYLE_SELECTED_CONN)?;
                        Ok(Some(pair))
                    },
                    None => Ok(None),
                }
            }
        }

        Ok(None)

    }
}

fn kick_connecting(conn_id: u64, agent: Arc<AgentShared>, event_tx: mpsc::Sender<Event>, bar: Bar, thrput: bool, origin: &str) -> mpsc::Sender<()> {
    tracing::info!("kick connecting [{}-{}] [{origin}]", agent.name, conn_id,);

    let (guard_tx, guard_rx) = mpsc::channel(1);
    spawn_with_name(format!("{}-conn-{conn_id}", agent.name), async move {
        connecting_task(conn_id, agent, event_tx, bar, guard_rx, thrput).await
    });
    guard_tx
}

async fn connecting_task(conn_id: u64, agent: Arc<AgentShared>, tx: mpsc::Sender<Event>, mut bar: Bar, mut guard_rx: mpsc::Receiver<()>, thrput: bool) {
    
    let _r = bar.update_style(STYLE_GENERAL);

    loop {
        tracing::debug!("connecting");
        bar.update_msg("connecting");

        let r = tokio::select! {
            r = try_connet(&mut bar, agent.url.as_str(), thrput) => r,
            _r = guard_rx.recv() => {
                tracing::debug!("dropped guard");
                return
            }
        };

        match r {
            Ok((conn, speed)) => {
                let _r = tx.send(Event::Connected { 
                    conn_id,
                    conn,
                    agent, 
                    bar, 
                    speed,
                }).await;
                return;
            },
            Err(e) => {
                tracing::debug!("connect failed [{e:?}]");
                bar.update_msg("connect failed, will try next");
            },
        }
        tokio::time::sleep(Duration::from_millis(5000)).await;
    }
}

async fn try_connet(bar: &mut Bar, url_str: &str, thrput: bool) -> Result<(QuicConn, Option<u64>)> {
    tracing::debug!("connecting to {url_str}");
    let (stream, _r) = ws_connect_to(url_str).await
    .with_context(||format!("connect to agent failed"))?;
    let session = make_stream_session(stream.split(), false).await?;
    let ctrl = session.ctrl_client().clone_invoker();
    let conn = punch(ctrl).await?;
    
    tracing::debug!("successfully connected");
    bar.update_msg("connected");

    if thrput {
        bar.update_msg("check speed...");
        let (writer, reader) = conn.open_bi().await?;
        let speed = echo_throughput(bar, writer, reader).await?;
        if speed < 300_000 {
            bail!("low speed [{speed}] < 300K")
        }
        Ok((conn, Some(speed)))
    } else {
        Ok((conn, None))
    }
}

async fn echo_throughput(bar: &mut Bar, mut writer: SendStream, mut reader: RecvStream) -> Result<u64> {
    
    let start_time = Instant::now();

    writer.write_all(&[0x09, 0x00, 0x00]).await?;

    let total_bytes = 1024 * 1024_u64;
    let send_buf = vec![0_u8; 1024];
    let mut recv_buf = vec![0_u8; 1024];

    let mut sent_bytes = 0;
    let mut recv_bytes = 0;

    while recv_bytes < total_bytes {
        let sbytes = ((total_bytes - sent_bytes)as usize).min(send_buf.len());
        
        tokio::select! {
            r = writer.write(&send_buf[..sbytes]), if sbytes > 0 => {
                let n = r?;
                if n == 0 {
                    bail!("write zero")
                }
                sent_bytes += n as u64;
            }
            r = reader.read(&mut recv_buf[..]) => {
                let n = r?.with_context(||"closed")?;
                if n == 0 {
                    bail!("read zero")
                }
                recv_bytes += n as u64;
            }
        }
    }

    let elapsed = start_time.elapsed();

    let speed = if elapsed > Duration::ZERO {
        let milli = elapsed.as_millis() as u64;
        total_bytes * 1000 / milli
    } else {
        total_bytes
    };

    let msg = format!("throughput [{}/s], total [{}/{elapsed:?}]", indicatif::HumanBytes(speed), indicatif::HumanBytes(total_bytes));
    tracing::debug!("{msg}");
    bar.update_msg(msg);

    Ok(speed)
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
    // shared: Arc<AgentShared>,
    conns: Vec<ConnSlot>,
    rrobin: usize,
}

impl AgentSlot {

    async fn open_priority(&mut self) -> Option<(StreamPair, usize)> {
        for index in 0..self.conns.len() {
            let conn_slot = &mut self.conns[index];
            if conn_slot.check_speed {
                if let Some(pair) = conn_slot.open_ch().await {
                    return Some((pair, index));
                }
            }
        }

        self.open_random().await
    }

    async fn open_random(&mut self) -> Option<(StreamPair, usize)> {
        for _ in 0..self.conns.len() {
            let index = self.next_robin();
            let conn_slot = &mut self.conns[index];

            if let Some(pair) = conn_slot.open_ch().await {
                return Some((pair, index));
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
    check_speed: bool,
}

impl ConnSlot {
    fn kick_connecting(&mut self, origin: &str) {
        let conn = self;

        if let ConnState::Working(work) = &mut conn.state {    
            if let Some(bar) = work.bar.take() {
                let tx = kick_connecting(conn.id, conn.agent.clone(), conn.event_tx.clone(), bar, conn.check_speed, origin);
                conn.state = ConnState::Connecting(tx);
            }
        }
    }

    async fn open_ch(&mut self) -> Option<StreamPair> {
        if let ConnState::Working(work) = &mut self.state {
            let r = work.conn.open_bi().await;
            match r {
                Ok(pair) => {
                    return Some(pair)
                },
                Err(_e) => {
                    // tracing::info!("kick connecting for open ch failed [{e:?}]");
                    self.kick_connecting("open_bi failed");
                    // if let Some(bar) = work.bar.take() {
                    //     let tx = kick_connecting(self.id, self.agent.clone(), self.event_tx.clone(), bar, true, "open_bi failed");
                    //     self.state = ConnState::Connecting(tx);
                    // }
                },
            }
        }
        None
    }

    fn update_style(&mut self, template: &str) -> Result<()> {
        if let ConnState::Working(work) = &mut self.state {
            if let Some(bar) = work.bar.as_mut() {
                bar.update_style(template)?;
            }
        }
        Ok(())
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
        bar: Bar,
        speed: Option<u64>,
    }
}

struct Bar {
    bar: ProgressBar,
    msg: String,
}

impl Bar {
    pub fn new(bar: ProgressBar) -> Self {
        Self { bar, msg: "".into(), }
    }

    pub fn update_msg<S: Into<String>>(&mut self, msg: S) {
        self.msg = msg.into();
        self.bar.set_message(self.msg.clone());
    }

    pub fn update_style(&self, template: &str) -> Result<()> {
        // tracing::debug!("update style {template}");
    
        let style = ProgressStyle::with_template(template)?
        .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ ");

        self.bar.set_style(style);

        self.bar.set_message(self.msg.clone());
        Ok(())
    }

    fn update_stats(&mut self, local: &ConnectionStats, remote: &QuicStats, ) {

        let remote_rtt = remote.path().rtt();
        let remote_cwnd = remote.path().cwnd();
    
        let path = &local.path;
        self.update_msg(format!(
            "rtt {}|{}, cwnd {}|{}, bytes {}|{}", 
            path.rtt.as_millis(),
            remote_rtt,
            indicatif::HumanBytes(path.cwnd),
            indicatif::HumanBytes(remote_cwnd),
            indicatif::HumanBytes(local.udp_tx.bytes),
            indicatif::HumanBytes(local.udp_rx.bytes),
        ));
        // tracing::debug!("updated stats");
    
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
    
}

struct ConnWork {
    conn: QuicConn,
    bar: Option<Bar>,
}

impl ConnWork {
    // fn update_style(&self, template: &str) -> Result<()> {
    //     if let Some(bar) = self.bar.as_ref() {
    //         bar.update_style(template)?;
    //     }

    //     // tracing::debug!("update style {template}");
    //     // if let Some(bar) = self.bar.as_ref() {
    //     //     update_bar_style(bar, template)?;
    //     // }
    //     // self.update_stats();
    //     Ok(())
    // }

    fn update_stats(&mut self) {
        if let Some(remote) = self.conn.try_remote_stats() {
            let local = self.conn.stats();
            if let Some(bar) = self.bar.as_mut() {
                bar.update_stats(&local, &remote);
            }
        }
    }
}

// fn update_bar_style(bar: &ProgressBar, template: &str) -> Result<()> {
//     let style = ProgressStyle::with_template(template)?
//     .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ ");
//     bar.set_style(style);
//     Ok(())
// }


// fn update_bar_stats(bar: &ProgressBar, local: &ConnectionStats, remote: &QuicStats, ) {
//     let remote_rtt = remote.path().rtt();
//     let remote_cwnd = remote.path().cwnd();

//     let path = &local.path;
//     bar.set_message(format!(
//         "rtt {}/{}, cwnd {}/{}, tx {}, rx {}", 
//         path.rtt.as_millis(),
//         remote_rtt,
//         indicatif::HumanBytes(path.cwnd),
//         indicatif::HumanBytes(remote_cwnd),
//         indicatif::HumanBytes(local.udp_tx.bytes),
//         indicatif::HumanBytes(local.udp_rx.bytes),
//     ));
//     // tracing::debug!("updated stats");

//     // bar.set_message(format!(
//     //     "rtt {}/{remote_rtt}, cwnd {}/{remote_cwnd}, ev {}, lost {}/{}, sent {}, pl {}/{}, black {}", 
//     //     path.rtt.as_millis(),
//     //     path.cwnd,
//     //     path.congestion_events,
//     //     path.lost_packets,
//     //     path.lost_bytes,
//     //     path.sent_packets,
//     //     path.sent_plpmtud_probes,
//     //     path.lost_plpmtud_probes,
//     //     path.black_holes_detected,
//     // ));
// }

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
