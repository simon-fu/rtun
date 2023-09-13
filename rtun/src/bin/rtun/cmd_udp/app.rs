use std::{time::Duration, sync::Arc, net::SocketAddr, fmt::Write};

use anyhow::{Result, bail, Context};
use console::Term;
use parking_lot::Mutex;
use tokio::{sync::mpsc, net::UdpSocket};
use tracing::{error, info};
use super::tui::footer::{FooterApp, Action as FooterAction, Event};
use rtun::{actor_service::{ActorEntity, handle_first_none, Action, AsyncHandler, ActorHandle, handle_msg_none, ActorBuilder, Invoker}, hex::BinStrLine};

pub fn make_app(event_tx: mpsc::Sender<Event>) -> Result<(AppSync, AppAsync)> {

    let shared = Arc::new(Shared {
        state: Default::default(),
        event_tx,
    });

    let builder = UdpSessionBuilder::new();

    Ok((
        AppSync {
            shared: shared.clone(),
            invoker: builder.build_invoker(),
        },
        AppAsync{ 
            shared,
            builder,
        },
    ))
}

pub struct AppAsync {
    shared: Arc<Shared>,
    builder: UdpSessionBuilder,
}

impl AppAsync {
    pub async fn run(self) -> Result<()> {
        // let socket = UdpSocket::bind("0.0.0.0:0").await?;

        let entity = Entity {
            buf: vec![0;1700],
            socket: None,
            shared: self.shared,
        };
    
        let handle = self.builder.builder.build(
            "udp".into(), // format!("udp"),
            entity, 
            handle_first_none,
            wait_next, 
            handle_next, 
            handle_msg_none,
        );

        let mut session = UdpSession {
            handle,
        };

        // tokio::spawn(async move {
        //     for _n in 0..100 {
        //         tokio::time::sleep(Duration::from_secs(1)).await;
        //         tracing::debug!("async log {_n}");
        //     }
        // });
        
        let _r = session.handle.wait_for_completed().await?;
        Ok(())
    }
}

pub struct AppSync {
    shared: Arc<Shared>,
    invoker: UdpSessionInvoker,
}


impl FooterApp for AppSync {
    fn on_paint(&mut self, term: &Term) -> Result<usize> {
        lazy_static::lazy_static! {
            static ref LONG_DIV: String = {
                let mut s = String::with_capacity(2048);
                for _ in 0..s.capacity() {
                    s.push('-')
                }
                s
            };
        }
        
        let (_h, w) = term.size();
        let div = &(*LONG_DIV)[..w as usize];

        let mut lines = 0;

        term.clear_line()?;
        
        term.write_line(div)?;
        lines += 1;

        term.clear_line()?;
        {
            let state = self.shared.state.lock();
            let mut s = format!("local: {:?}    target: {:?}", state.local, state.target);
            if let Some(ttl) = state.ttl {
                write!(s, "    ttl: {ttl}")?;
            }
            term.write_line(
                s.as_str(),
            )?;
            lines += 1;    
        }
        
        term.clear_line()?;
        term.write_line(div)?;
        lines += 1;

        Ok(lines)
    }

    fn on_input(&mut self, input: String) -> Result<FooterAction> {
        self.invoker.invoker.blocking_invoke(ReqCommand(input))??;
        Ok(FooterAction::None)
    }
}

struct Shared {
    event_tx: mpsc::Sender<Event>,
    state: Mutex<State>,
}

#[derive(Default, Debug, Clone)]
struct State {
    local: Option<SocketAddr>,
    target: Option<SocketAddr>,
    ttl: Option<u32>,
}

impl State {
    fn set_local(&mut self, addr: Option<SocketAddr>) {
        self.local = addr;
        if self.local.is_none() {
            self.ttl = None;
            self.target = None;
        }
    }

    fn set_target(&mut self, addr: Option<SocketAddr>) {
        self.target = addr;
    }
}




pub struct UdpSessionInvoker {
    invoker: Invoker<Entity>,
}

impl  UdpSessionInvoker {

    pub async fn shutdown(&mut self) {
        self.invoker.shutdown().await;
    }
}

pub struct UdpSessionBuilder {
    builder: ActorBuilder<Entity>,
}

impl UdpSessionBuilder {
    pub fn new() -> Self {
        Self {
            builder: ActorBuilder::new(),
        }
    }

    pub fn build_invoker(&self) -> UdpSessionInvoker {
        UdpSessionInvoker {
            invoker: self.builder.build_invoker(),
        }
    }
}

pub struct UdpSession {
    handle: ActorHandle<Entity>,
}

impl  UdpSession {

    pub async fn shutdown_and_waitfor(&mut self) -> Result<()> {
        self.handle.invoker().shutdown().await;
        self.handle.wait_for_completed().await?;
        Ok(())
    }

    pub async fn wait_for_completed(&mut self) -> Result<Option<SwitchSinkResult>> {
        self.handle.wait_for_completed().await
    }
}

pub type SwitchSinkResult = EntityResult;

#[derive(Debug)]
struct ReqCommand(String);

type ReqCommandResult = Result<()>;

#[async_trait::async_trait]
impl AsyncHandler<ReqCommand> for Entity {
    type Response = ReqCommandResult; 

    async fn handle(&mut self, req: ReqCommand) -> Self::Response {
        let r = self.exec_cmd(req.0.as_str()).await;
        match r {
            Ok(_r) => {
                let _r = self.shared.event_tx.send(Event::PaintApp).await;
            },
            Err(e) => {
                error!("{e:?}");
            }
        }
        Ok(())
    }
}



type Next = Result<()>;


#[inline]
async fn wait_next(entity: &mut Entity) -> Next {
    match &entity.socket.as_mut() {
        Some(socket) => {
            let (len, addr) = socket.recv_from(&mut entity.buf).await?;
            let data = &entity.buf[..len];

            let b = {
                let mut state = entity.shared.state.lock();
                if state.target.is_none() {
                    state.set_target(Some(addr));
                    info!("set target {addr}");
                    true
                } else {
                    false
                }
            };
            if b {
                let _r = entity.shared.event_tx.send(Event::PaintApp).await;
            }

            
            if let Ok(s) = std::str::from_utf8(data) {
                info!("<= [{addr}, {len}]: [{s}]");
            } else {
                info!("<= [{addr}, {len}]: {}", data.dump_bin());
            }
        },
        None => {
            tokio::time::sleep(Duration::MAX/2).await;
        },
    }
    
    Ok(())
}

async fn handle_next(entity: &mut Entity, next: Next) -> Result<Action>  {
    match next {
        Ok(_r) => Ok(Action::None),
        Err(e) => {
            let _r = entity.shared.event_tx.send(Event::Exit).await;
            Err(e)
        },
    }
}


pub struct Entity {
    socket: Option<UdpSocket>,
    buf: Vec<u8>,
    shared: Arc<Shared>,
}

impl Entity {
    async fn exec_cmd(&mut self, cmd: &str) -> Result<()> {

        let iter1 = Some("udp").iter().map(|x|*x);
        let iter2 = cmd.trim().split(" ").map(|x|x.trim());

        let r = CmdArgs::try_parse_from(iter1.chain(iter2))?;
        match r.cmd {
            SubCmd::Bind(args) => {
                if self.socket.is_some() {
                    bail!("already bind")
                }
                
                let socket = UdpSocket::bind(&args.listen).await
                .with_context(||format!("failed to bind socket addr [{}]", args.listen))?;
                
                {
                    self.shared.state.lock().set_local(Some(socket.local_addr()?));
                }
                
                self.socket = Some(socket);
            },
            SubCmd::Close(_args) => {
                let socket = self.socket.take();
                get_socket(&socket)?;
                drop(socket);
                {
                    self.shared.state.lock().set_local(None);
                }
                info!("closed socket")
            },
            SubCmd::Send(args) => {
                let socket = get_socket(&self.socket)?;
                
                let target = match args.target.as_ref() {
                    Some(target) => {
                        let target: SocketAddr = target.parse().with_context(||"parse target addr failed")?;
                        target
                    },
                    None => {
                        self.shared.state.lock().target.with_context(||"no target addr")?
                    },
                };
                
                let text = args.content.as_str();
                let bytes = text.as_bytes();
                let n = socket.send_to(bytes, target).await
                .with_context(||"send failed")?;
                info!("sent bytes {n}");
                if n == bytes.len() {
                    info!("=> [{target}, {n}]: [{text}]");
                } else {
                    error!("sent partial {n} < {}", bytes.len());
                }
                
            },
            SubCmd::Set(args) => {
                match args.key.as_str() {
                    "target" => {
                        let target: SocketAddr = args.value.parse()
                        .with_context(||format!("failed to parse target addr [{}]", args.value))?;

                        self.shared.state.lock().set_target(Some(target));
                    },
                    "ttl" => {
                        let ttl: u32 = args.value.parse()
                        .with_context(||format!("failed to parse ttl [{}]", args.value))?;
                        self.set_socket_ttl(ttl)?;
                    }
                    _ => {
                        bail!("unknown key [{}]", args.key)
                    }
                }
            },
        }
        Ok(())
    }

    fn set_socket_ttl(&mut self, ttl: u32) -> Result<()> {
        let socket = get_socket(&self.socket)?;
        socket.set_ttl(ttl).with_context(||"failed to set socket ttl")?;
        {
            self.shared.state.lock().ttl = Some(ttl);
        }
        info!("set socket ttl {ttl}");
        Ok(())
    }
}

#[inline]
fn get_socket(socket: &Option<UdpSocket>) -> Result<&UdpSocket> {
    socket.as_ref().with_context(||"NOT bind socket yet")
}

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

use clap::Parser;



// refer https://github.com/clap-rs/clap/tree/master/clap_derive/examples
#[derive(Parser, Debug)]
#[clap(name = "udp")]
struct CmdArgs {
    #[clap(subcommand)]
    cmd: SubCmd,
}

#[derive(Parser, Debug)]
enum SubCmd {
    Bind(BindCmdArgs),
    Close(CloseCmdArgs),
    Send(SendCmdArgs),
    Set(SetCmdArgs),
}


#[derive(Parser, Debug)]
#[clap(name = "bind")]
pub struct BindCmdArgs {
    #[clap(
        // short = 'l',
        // long = "listen",
        long_help = "listen address",
        default_value = "0.0.0.0:0",
    )]
    listen: String,
}

#[derive(Parser, Debug)]
#[clap(name = "close")]
pub struct CloseCmdArgs {
}

#[derive(Parser, Debug)]
#[clap(name = "send")]
pub struct SendCmdArgs {
    #[clap(
        long_help = "content for sending",
    )]
    content: String,

    #[clap(
        short = 't',
        long = "target",
        long_help = "target address",
    )]
    target: Option<String>,
}

#[derive(Parser, Debug)]
#[clap(name = "set")]
pub struct SetCmdArgs {
    #[clap(
        long_help = "key",
    )]
    key: String,

    #[clap(
        long_help = "value",
    )]
    value: String,
}

