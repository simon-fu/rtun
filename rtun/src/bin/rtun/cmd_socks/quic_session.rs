use std::time::Duration;
use anyhow::{Result, bail, Context, anyhow};
use quinn::{SendStream, RecvStream};
use tokio::{sync::{mpsc, oneshot}, task::JoinHandle, time::Instant};
use tracing::{debug, info};
use rtun::{actor_service::{ActorEntity, handle_first_none, Action, AsyncHandler, ActorHandle, handle_msg_none, start_actor}, huid::HUId, switch::invoker_ctrl::{CtrlInvoker, CtrlHandler}, ice::{ice_quic::{QuicConn, UpgradeToQuic, QuicIceCert}, ice_peer::{IcePeer, IceConfig, IceArgs}}, async_rt::spawn_with_inherit, proto::{open_p2presponse::Open_p2p_rsp, P2PArgs, p2pargs::P2p_args, QuicSocksArgs, P2PQuicArgs}};



pub fn make_quic_session<H: CtrlHandler>(uid: HUId, ctrl: CtrlInvoker<H>, agent: &str) -> Result<QuicSession<H>> {

    let entity = Entity {
        state: State::Disconnected(Instant::now()), 
        ctrl,
    };
    
    let handle = start_actor (
        format!("quic-{agent}-{uid}", ),
        entity, 
        handle_first_none,
        wait_next, 
        handle_next, 
        handle_msg_none,
    );

    Ok(handle)
}

pub type QuicSession<H> = ActorHandle<Entity<H>>;


pub type StreamPair = (SendStream, RecvStream);


pub struct SetCtrl<H: CtrlHandler>(pub CtrlInvoker<H>);

#[async_trait::async_trait]
impl<H: CtrlHandler> AsyncHandler<SetCtrl<H>> for Entity<H> {
    type Response = Result<()>; 

    async fn handle(&mut self, req: SetCtrl<H>) -> Self::Response {
        self.ctrl = req.0;
        match &mut self.state {
            State::Working(_) => {}
            State::Punching(_) => {}
            State::Disconnected(_) => {
                let punch = kick_punch(self.ctrl.clone())?;
                self.state = State::Punching(punch);
            }
        }
        Ok(())
    }
}


#[derive(Debug)]
pub struct ReqCh;


#[async_trait::async_trait]
impl<H: CtrlHandler> AsyncHandler<ReqCh> for Entity<H> {
    type Response = Result<StreamPair>; 

    async fn handle(&mut self, _req: ReqCh) -> Self::Response {
        match &mut self.state {
            State::Working(work) => {
                let r = work.conn.open_bi().await;
                match r {
                    Ok(r) => return Ok(r),
                    Err(e) => {
                        info!("disconnected by [opening ch failed {e:?}]");
                        self.state = State::Disconnected(next_retry());
                    }
                }
            }
            State::Punching(_) => {}
            State::Disconnected(_) => {}
        }
        bail!("quic NOT connected") 
    }
}


fn next_retry() -> Instant {
    const RETRY_SECS: u64 = 3;
    Instant::now() + Duration::from_secs(RETRY_SECS)
}


type Next = Result<()>;


#[inline]
async fn wait_next<H: CtrlHandler>(entity: &mut Entity<H>) -> Next {
    entity.wait_next().await
}

async fn handle_next<H: CtrlHandler>(_entity: &mut Entity<H>, _next: Next) -> Result<Action>  {
    Ok(Action::None)
}

enum State {
    Punching(PunchSession),
    Working(Work),
    Disconnected(Instant),
}

struct Work {
    conn: QuicConn,
}

pub struct Entity<H: CtrlHandler> {
    ctrl: CtrlInvoker<H>,
    state: State,
}

impl <H: CtrlHandler> Entity<H> {
    async fn wait_next(&mut self) -> Next {
        match &mut self.state {
            State::Punching(punch) => {
                let r = punch.rx.recv().await;
                self.state = match r {
                    Some(conn) => {
                        info!("tunnel connected");
                        State::Working(Work {
                            conn,
                        })
                    },
                    None => State::Disconnected(next_retry()),
                };
            },
            State::Working(work) => {
                tokio::time::sleep(work.conn.ping_interval()).await;
                if work.conn.is_ping_timeout() {
                    info!("ping timeout, try punch again");
                    let punch = kick_punch(self.ctrl.clone())?;
                    self.state = State::Punching(punch);
                }
            },
            State::Disconnected(deadline) => {
                tokio::time::sleep_until(*deadline).await;
                let punch = kick_punch(self.ctrl.clone())?;
                self.state = State::Punching(punch);
            },
        }
        Ok(())
    }
}

fn kick_punch<H: CtrlHandler>(ctrl: CtrlInvoker<H>) -> Result<PunchSession> {
    let (tx, rx) = mpsc::channel(1);
    let (guard_tx, guard_rx) = oneshot::channel();
    let task = spawn_with_inherit("punch", async move {
        let r = tokio::select! {
            r = punch_task(ctrl, tx) => r,
            _r = guard_rx => Err(anyhow!("punch cancel")),
        };
        debug!("finished {r:?}");
        r
    });
    Ok(PunchSession {
        rx,
        _guard: guard_tx,
        _task: task,
    })
}

struct PunchSession {
    rx: mpsc::Receiver<QuicConn>,
    _guard: oneshot::Sender<()>,
    _task: JoinHandle<Result<()>>,
}

async fn punch_task<H: CtrlHandler>(ctrl: CtrlInvoker<H>, tx: mpsc::Sender<QuicConn>) -> Result<()> {
    debug!("try punching");

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

    let ice_cert = QuicIceCert::try_new()?;
    let cert_der = ice_cert.to_bytes()?.into();
    
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
            
            let local_cert = QuicIceCert::try_new()?;
            let server_cert_der = args.cert_der;

            tracing::debug!("remote args {remote_args:?}");
            let conn = peer.dial(remote_args).await?
            .upgrade_to_quic2(&local_cert, Some(server_cert_der)).await?;
            let _r = tx.send(conn).await;
            
            return Ok(())
        },
        Open_p2p_rsp::Status(s) => {
            bail!("open p2p but {s:?}");
        },
        _ => {
            bail!("unknown Open_p2p_rsp {rsp:?}");
        }
    }     
    
}

type Msg = ();

type EntityResult = ();

impl<H: CtrlHandler> ActorEntity for Entity<H> {
    type Next = Next;

    type Msg = Msg;

    type Result = EntityResult;

    fn into_result(self, _r: Result<()>) -> Self::Result {
        ()
    }
}
