
use std::{net::SocketAddr, time::Duration, collections::{HashMap, VecDeque, HashSet}, task::{self, Poll}, pin::Pin, io};
use anyhow::{Result, Context, anyhow};
use bytes::Bytes;
use futures::{stream::FuturesUnordered, StreamExt, channel::oneshot, Future, FutureExt, ready};
use quinn::udp::Transmit;

use crate::stun::async_udp::udp_state;

use super::async_udp::{AsyncUdpSocket, tokio_socket_bind, AsUdpSocket, TokioUdpSocket};
use rand::Rng;
use stun_codec::{Message, MessageClass, rfc5389::{methods::BINDING, Attribute, attributes::{Software, XorMappedAddress, MappedAddress, Username}}, TransactionId, MessageEncoder, MessageDecoder, Method};
use bytecodec::{EncodeExt, DecodeExt};
use tokio::{net::{ToSocketAddrs, lookup_host}, io::ReadBuf, time::Instant};

#[derive(Debug, Default)]
pub struct Config {
    detect_all_server: bool,
    min_success_response: Option<usize>,
    use_binding_fut: bool,
    username: Option<String>,
    password: Option<String>,
    // ttl: Option<u32>,
}

impl Config {
    pub fn with_username(self, username: String) -> Self {
        Self {
            username: Some(username),
            ..self
        }
    }

    pub fn with_password(self, password: String) -> Self {
        Self {
            password: Some(password),
            ..self
        }
    }

    pub fn gen_bind_req_bytes(&self) -> Result<Vec<u8>> {
        let msg = self.gen_bind_req()?;
        encode_message(msg)
    }

    pub fn gen_bind_req(&self) -> Result<Message<Attribute>> {
        let mut req = gen_request(BINDING);
        if let Some(username) = self.username.clone() {
            let username = Username::new(username)?;
            req.add_attribute(Attribute::Username(username));
        }
        Ok(req)
    }
}

pub struct Binding<U> {
    config: Config,
    socket: U,
}

// impl<U> Default for Binding<U> {
//     fn default() -> Binding<U> {
//         Binding { 
//             config: Default::default(), 
//             socket: Option::<U>::None,
//         }
//     }
// }

impl Binding<TokioUdpSocket> {
    pub async fn bind<A>(addr: A) -> Result<Binding<TokioUdpSocket>> 
    where
        A: tokio::net::ToSocketAddrs,
    {
        let socket = tokio_socket_bind(addr).await?;
        Ok(Self::with_socket(socket))
    }
}

impl<U> Binding<U> {

    pub fn with_socket(socket: U) -> Self {
        Self { 
            socket, 
            config: Default::default(),
        }
    }

    pub fn with_config(socket: U, config: Config) -> Binding<U> {
        Self { 
            socket, 
            config,
        }
    }

    // pub async fn bind<A>(addr: A) -> Result<Binding<BoxUdpSocket>> 
    // where
    //     A: tokio::net::ToSocketAddrs,
    // {
    //     let socket = tokio_socket_bind(addr).await?;
    //     Ok(Self::with_socket(socket))
    // }

    // fn socket<U1>(self, socket: U1) -> Binding<U1> {
    //     Binding { 
    //         socket: Some(socket), 
    //         config: self.config,
    //     }
    // }

    // pub fn username(mut self, username: String) -> Binding<U> {
    //     self.config.username = Some(username);
    //     Binding {
    //         config: self.config,
    //         socket: self.socket,
    //     }
    // }

    // pub fn password(mut self, password: String) -> Binding<U> {
    //     self.config.password = Some(password);
    //     Binding {
    //         config: self.config,
    //         socket: self.socket,
    //     }
    // }

    // pub fn ttl(mut self, ttl: Option<u32>) -> Binding<U> {
    //     self.config.ttl = ttl;
    //     Binding {
    //         config: self.config,
    //         socket: self.socket,
    //     }
    // }

    pub async fn exec<I, A>(self, servers: I) -> Result<(U, BindingOutput)> 
    where
        U: AsyncUdpSocket + Unpin,
        I: Iterator<Item = A>,
        A: ToSocketAddrs,
    {
        self.do_exec(servers).await
    }

    async fn do_exec<I, A>(self, servers: I) -> Result<(U, BindingOutput)> 
    where
        U: AsyncUdpSocket + Unpin,
        I: Iterator<Item = A>,
        A: ToSocketAddrs,
    {
        // if let Some(ttl) = self.config.ttl {
        //     self.socket.set_ttl(ttl)?;
        // }
        let socket = self.socket;

        let mut detect = BindingExec::from_socket(socket);
        detect.set_config(self.config);
    
        for server in servers {
            detect.lookup_futures.push(lookup_host(server));
        }
    
        while !detect.is_done() {
            detect.run_one().await?;
        }
    
        detect.into_result()
    }
}





pub const STUN_SERVERS: [&'static str; 3] =  [
    "stun1.l.google.com:19302",
    "stun2.l.google.com:19302",
    "stun.qq.com:3478",
];


pub async fn detect_nat_type1<U: AsyncUdpSocket + Unpin>(socket: U) -> Result<(U, BindingOutput)> {
    run_detect(socket, STUN_SERVERS.iter(), Default::default()).await
}

pub async fn detect_nat_type2<I, A>(servers: I, config: Config) -> Result<BindingOutput> 
where
    I: Iterator<Item = A>,
    A: ToSocketAddrs,
{
    let socket = tokio_socket_bind("0.0.0.0:0").await?;
    run_detect(socket, servers, config).await.map(|x|x.1)
}


pub async fn detect_nat_type3<U, I, A>(socket: U, servers: I, config: Config) -> Result<(U, BindingOutput)> 
where
    U: AsyncUdpSocket + Unpin,
    I: Iterator<Item = A>,
    A: ToSocketAddrs,
{
    // let socket = Arc::new(UdpSocketExt::bind("0.0.0.0:0").await?);
    run_detect(socket, servers,  config).await
    // let mut detect = DetectNat::from_socket(socket);
    // detect.set_config(config);

    // for server in servers {
    //     detect.lookup_futures.push(lookup_host(server));
    // }

    // detect.run().await
}

async fn run_detect<U, I, A>(socket: U, servers: I, mut config: Config) -> Result<(U, BindingOutput)> 
where
    U: AsyncUdpSocket + Unpin,
    I: Iterator<Item = A>,
    A: ToSocketAddrs,
{
    if config.min_success_response.is_none() {
        config.min_success_response = Some(2);
    }

    // let socket = Arc::new(UdpSocketExt::bind("0.0.0.0:0").await?);
    let mut detect = BindingExec::from_socket(socket);
    detect.set_config(config);

    for server in servers {
        detect.lookup_futures.push(lookup_host(server));
    }

    while !detect.is_done() {
        detect.run_one().await?;
    }

    detect.into_result()
}

pub struct BindingExec<U, Fut1> {
    client: StunClient<U>,

    ctx: DetectContext,
    
    lookup_futures: FuturesUnordered<Fut1>,

    binding_futures: FuturesUnordered<TransactionFuture>,

    config: Config,
}

impl<U, Fut1, I1> BindingExec<U, Fut1> 
where
    U: AsyncUdpSocket + Unpin,
    Fut1: Future<Output = io::Result<I1>>,
    I1: Iterator<Item = SocketAddr>,
{
    pub fn from_socket(socket: U) -> Self {
        Self {
            client: StunClient::from_socket(socket),
            ctx: DetectContext::default(),
            lookup_futures: FuturesUnordered::new(),
            binding_futures: FuturesUnordered::new(),
            config: Default::default(),
        }
    }

    pub fn set_config(&mut self, config: Config) {
        self.config = config;
    }

    pub fn is_empty(&self) -> bool {
        self.lookup_futures.is_empty() && self.binding_futures.is_empty() && self.client.is_empty()
    }

    pub fn is_done(&self) -> bool {
        if self.is_empty() {
            return true
        }

        if !self.config.detect_all_server {
            let min_success = self.config.min_success_response.unwrap_or(1);
            if self.ctx.output.num_success >= min_success {
                return true
            }
        }
        
        false
    }

    pub fn into_result(self) -> Result<(U, BindingOutput)> {
        if self.ctx.output.is_empty() {
            match self.ctx.error {
                Some(e) => return Err(e),
                None => return Err(anyhow!("lookup host but empty addr list")),
            }
        } else {
            Ok((self.client.into_socket(), self.ctx.output))
        }
    }

    fn process_lookup_result(&mut self, r: Option<io::Result<I1>>) -> Result<()> {
        match r {
            Some(next) => {
                let iter = match next {
                    Ok(v) => v,
                    Err(e) => {
                        self.ctx.error = Some(e.into());
                        // continue;
                        return Ok(())
                    },
                };

                if self.config.use_binding_fut {
                    for target in iter {
                        let req = self.config.gen_bind_req()?;
                        let fut = self.client.transaction(req, target);
                        self.binding_futures.push(fut);
                    }
                } else {
                    for target in iter {
                        let req = self.config.gen_bind_req()?;
                        let _r = self.client.req_transaction(req, target);
                    }
                }
            },
            None => {},
        }
        return Ok(())
    }

    pub async fn run_one(&mut self) -> Result<()> {
        tokio::select! {
            r = self.lookup_futures.next(), if !self.lookup_futures.is_empty() => {
                self.process_lookup_result(r)?;
            },
            r = self.binding_futures.next(), if !self.binding_futures.is_empty() => {
                match r {
                    Some(bind_result) => {
                        self.ctx.handle_bind_result(bind_result);
                    },
                    None => {},
                }
            },
            r = self.client.exec(), if !self.client.is_empty() => {
                // println!("exec result {r:?}");
                let output = r?;
                self.ctx.handle_bind_result(output.result);
            }
        }

        Ok(())
    }

}


#[derive(Debug, Default)]
struct DetectContext {
    error: Option<anyhow::Error>,
    output: BindingOutput,
}

impl DetectContext {
    fn handle_bind_result(&mut self, result: TransactionResult) {
        match self.handle_bind_result_(result) {
            Ok(_r) => {},
            Err(e) => self.error = Some(e),
        }
    }

    fn handle_bind_result_(&mut self, result: TransactionResult) -> Result<()> {
        let rsp = result?;
        let map_addr1 = rsp.response.get_attribute::<XorMappedAddress>().map(|x|x.address());
        let map_addr3 = rsp.response.get_attribute::<MappedAddress>().map(|x|x.address());
        let map_addr = map_addr1.or(map_addr3)
        .with_context(||"no mapped addr")?;
        self.output.insert_unique(ReflexiveAddr { target: rsp.remote_addr, mapped: map_addr });
        Ok(())
    }
}


#[derive(Debug, Clone)]
pub struct ReflexiveAddr {
    pub target: SocketAddr,
    pub mapped: SocketAddr,
}

#[derive(Debug, Default, Clone)]
pub struct BindingOutput {
    // addrs: Vec<ReflexiveAddr>, 
    mapped: HashMap<SocketAddr, HashSet<SocketAddr>>, // reflexive -> targets
    total: usize,
    num_success: usize,
    // num_differences: usize, // num of different mapped address
}

impl BindingOutput {
    pub fn is_empty(&self) -> bool {
        self.mapped.is_empty()
    }

    pub fn nat_type(&self) -> Option<NatType> {
        if self.total >= 2 {
            if self.mapped.len() > 1 {
                Some(NatType::Symmetric)
            } else {
                Some(NatType::Cone)
            }
        } else {
            None
        }
    }

    // pub fn into_mapped(self) -> Option<SocketAddr> {
    //     self.mapped.into_iter().next().map(|x|x.0)
    //     // self.addrs.pop().map(|x|x.mapped)
    // }

    pub fn mapped_iter<'a>(&'a self) -> impl Iterator<Item = SocketAddr> + 'a  {
        self.mapped.iter().map(|x|x.0.clone())
    }

    pub fn target_iter<'a>(&'a self) -> impl Iterator<Item = impl Iterator<Item = SocketAddr>  + 'a > + 'a  {
        let rrr = self.mapped.iter()
        .map(|x|x.1.iter().map(|x|x.clone()));
        rrr
    }

    fn insert_unique(&mut self, addr: ReflexiveAddr) {

        if let Some(exist) = self.mapped.get_mut(&addr.mapped) {
            exist.insert(addr.target);
        } else {
            self.mapped.insert(addr.mapped, [addr.target].into());
        }
        self.total += 1;
        self.num_success += 1;
    }

    // fn insert_unique(&mut self, addr: ReflexiveAddr) {
    //     match self.addrs.iter().find(|x|x.mapped == addr.mapped) {
    //         Some(x) if x.target == addr.target => return,

    //         Some(_x) => { }

    //         None => {
    //             self.num_differences += 1; 
    //         },
    //     }

    //     self.addrs.push(addr);
    //     self.total += 1;
    // }

    #[cfg(test)]
    fn is_nat_detect_done(&self) -> bool {
        self.num_success >= 2
    }

    // fn merge(&mut self, other: Self) {
    //     for addr in other.addrs {
    //         self.insert_unique(addr);
    //     }
    // }

}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum NatType {
    Symmetric,
    Cone,
}




type TransactionMap = HashMap<TransactionId, Option<TransactionReq>>;

pub struct StunClient<U> {
    software: Option<Software>,
    socket: U,
    buf: Vec<u8>,
    next_retry_time: Instant,
    tsx_timeout: Duration,
    delay: Pin<Box<tokio::time::Sleep>>,
    
    transactions: TransactionMap,
    exec_outputs: VecDeque<ExecOutput>,
}

impl<U: AsyncUdpSocket + Unpin > StunClient<U> {
    pub fn from_socket(socket: U) -> Self {
        Self {
            delay: Box::pin(tokio::time::sleep(Duration::MAX/2)), 
            tsx_timeout: Duration::from_secs(10),
            next_retry_time: Instant::now(),
            buf: vec![0; 1700],
            transactions: Default::default(),
            exec_outputs: Default::default(),
            software: None,
            socket,
        }
    }

    // pub async fn bind<A: ToSocketAddrs>(addr: A) -> Result<Self> {
    //     let socket = tokio_socket_bind(addr).await?;
    //     Ok(Self::from_socket(socket))
    // }

    pub fn into_socket(self) -> U {
        self.socket
    }

    pub fn software(mut self, software: String) -> Result<Self> {
        self.software = Some(Software::new(software)?);
        Ok(self)
    }
    
    pub fn transaction(&mut self, req: Message<Attribute>, target: SocketAddr) -> TransactionFuture {
        let (tx, rx) = oneshot::channel(); 
        let r = self.kick_transaction(req, target);
        match r {
            Ok(mut req) => { 
                req.tx = Some(tx);
                self.transactions.insert(req.request.transaction_id(), Some(req));
            },
            Err(e) => {
                let _r = tx.send(Err(e.into()));
            },
        }

        TransactionFuture {
            rx,
        }
    }

    pub fn req_transaction(&mut self, req: Message<Attribute>, target: SocketAddr) -> Result<()> {
        let req = self.kick_transaction(req, target)?;
        self.transactions.insert(req.request.transaction_id(), Some(req));
        Ok(())
    }

    // pub fn binding(&mut self, target: SocketAddr) -> TransactionFuture {
    //     let req = gen_request(BINDING);
    //     self.transaction(req, target)
    // }

    // pub fn req_binding(&mut self, target: SocketAddr) -> Result<()> {
    //     let req = gen_request(BINDING);
    //     self.req_transaction(req, target)
    // }

    // pub fn req_binding2(&mut self, target: SocketAddr, username: String) -> Result<()> {
    //     let username = Username::new(username)?;
    //     let mut req: Message<Attribute> = gen_request(BINDING);
    //     req.add_attribute(Attribute::Username(username));
    //     let req = self.kick_transaction(req, target)?;
    //     self.transactions.insert(req.request.transaction_id(), Some(req));
    //     Ok(())
    // }

    fn kick_transaction(&mut self, mut req: Message<Attribute>, target: SocketAddr) -> Result<TransactionReq> {

        // let tsx_id = rand::thread_rng().gen::<[u8; 12]>();
        // let tsx_id = TransactionId::new(tsx_id);
        
        // let mut req = Message::new(MessageClass::Request, BINDING, tsx_id);

        if let Some(software) = self.software.as_ref() {
            req.add_attribute(Attribute::Software(software.clone()));
        }
        
        let mut encoder = MessageEncoder::new();
        let data: Bytes = encoder.encode_into_bytes(req.clone())?.into();

        let _r = self.socket.as_socket().try_send_to(data.clone(), target);
        let req = TransactionReq {
            deadline: Instant::now() + self.tsx_timeout,
            tx: None,
            data,
            target,
            request: req,
        };

        if self.transactions.len() == 1 {
            self.update_next_retry_time();
        }

        Ok(req)

    }

    pub fn is_empty(&self) -> bool {
        self.transactions.is_empty() && self.exec_outputs.is_empty()
    }

    pub fn exec<'a>(&'a mut self) -> ExecFuture<'a, U> {
        let next = self.next_retry_time();
        self.delay.as_mut().reset(next);
        ExecFuture { 
            // delay: Box::pin(self.next_retry_sleep()),
            client: self, 
        }
    }

    fn poll_recv(&mut self, cx: &mut task::Context<'_>) -> Poll<io::Error> {

        loop {
            let mut buf = ReadBuf::new(&mut self.buf);

            let r = self.socket.as_socket().poll_recv_from(cx, &mut buf);
            match r {
                Poll::Ready(r) => {
                    match r {
                        Ok(remote_addr) => {
                            let data = buf.filled();
    
                            if data.len() == 0 {
                                return Poll::Ready(io::Error::from(io::ErrorKind::UnexpectedEof))
                            }
    
                            let msg: Message<Attribute> = match decode_message(data) {
                                Ok(msg) => msg,
                                Err(_e) => return Poll::Pending, // drop invalid packet
                            };
    
                            if let Some(data) = try_binding_response_bytes(&msg, &remote_addr) {
                                let _r = self.socket.set_ttl(64);
                                let _r = self.socket.as_socket().try_send_to(data.into(), remote_addr);
    
                            } else {
                                let r = process_recv_data(&mut self.transactions, msg, remote_addr);
                                match r {
                                    Some(output) => {
                                        self.exec_outputs.push_back(output);
                                    },
                                    None => {},
                                }
                            }

                            continue;
                        },
                        Err(e) => return Poll::Ready(e),
                    }
                },
                Poll::Pending => return Poll::Pending,
        }

        
        }
    }

    fn process_tick(&mut self,) {
        let now = Instant::now();

        self.transactions.retain(|_k, tsx| {
            if let Some(tsx) = tsx {
                if now < tsx.deadline {
                    let _r = self.socket.as_socket().try_send_to(tsx.data.clone(), tsx.target);
                    return true
                } 
            }
            
            if let Some(tsx) = tsx.take() {
                if let Some(output) =  tsx.finish(Err(anyhow!("transaction timeout"))) {
                    self.exec_outputs.push_back(output);
                }
            }

            false

        });
        // for (_id, tsx) in self.transactions.iter() {
        //     let _r = self.socket.try_send_to_with(&tsx.data, tsx.target, false);
        // }
        self.update_next_retry_time();
    }

    fn update_next_retry_time(&mut self) {
        self.next_retry_time =  Instant::now() + Duration::from_millis(200);
    }

    fn next_retry_time(&self) -> Instant {
        self.next_retry_time
    }

    // fn next_retry_sleep(&self) -> tokio::time::Sleep {
    //     tokio::time::sleep(self.next_retry_duration())
    // }

    // fn next_retry_duration(&self) -> Duration {
    //     if self.transactions.len() > 0 {
    //         let now = Instant::now();
    //         if self.next_retry_time > now {
    //             self.next_retry_time - now
    //         } else {
    //             Duration::ZERO
    //         }
    //     } else {
    //         Duration::MAX/2
    //     }
    // }

}

type TransactionResult = Result<TransactionRsp>;

#[derive(Debug)]
pub struct ExecOutput {
    pub result: TransactionResult,
    pub request: Message<Attribute>,
}

pub struct TransactionFuture {
    rx: oneshot::Receiver<TransactionResult>,
}

impl Future for TransactionFuture {
    type Output = TransactionResult;

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        let r = self.rx.poll_unpin(cx);
        match r {
            Poll::Ready(r) => {
                match r {
                    Ok(r) => Poll::Ready(r),
                    Err(e) => Poll::Ready(Err(e.into())),
                }
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

struct TransactionReq {
    deadline: Instant,
    tx: Option<oneshot::Sender<TransactionResult>>,
    request: Message<Attribute>,
    data: Bytes,
    target: SocketAddr,
}

impl TransactionReq {
    fn finish(self, result: TransactionResult) -> Option<ExecOutput> {
        let tsx = self;
        if let Some(tx) = tsx.tx {
            let _r = tx.send(result);
            return None
        } else  {
            return Some(ExecOutput {
                result,
                request: tsx.request,
            })
        }
    }
}

#[derive(Debug)]
pub struct TransactionRsp {
    // pub request: Message<Attribute>,
    pub response: Message<Attribute>,
    pub remote_addr: SocketAddr,
}

pub struct ExecFuture<'a, U> {
    client: &'a mut StunClient<U>,
    // delay: Pin<Box<tokio::time::Sleep>>,
}

type PollOutput = Result<ExecOutput>;

impl<'a, U> Future for ExecFuture<'a, U> 
where
    U: AsyncUdpSocket + Unpin,
{
    type Output = PollOutput;

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {

        let self0 = self.get_mut();

        if let Some(output) = self0.client.exec_outputs.pop_front() {
            return Poll::Ready(Ok(output))
        }

        let r = self0.client.poll_recv(cx);
        match r {
            Poll::Ready(e) => return Poll::Ready(Err(e.into())),
            Poll::Pending => {},
        }

        match self0.client.delay.poll_unpin(cx) {
            Poll::Ready(_r) => {
                self0.client.process_tick();

                let next = self0.client.next_retry_time();
                self0.client.delay.as_mut().reset(next);
            },
            Poll::Pending => {},
        }

        if let Some(output) = self0.client.exec_outputs.pop_front() {
            return Poll::Ready(Ok(output))
        }

        Poll::Pending
    }
}

fn process_recv_data(transactions: &mut TransactionMap, msg: Message<Attribute>, remote_addr: SocketAddr) -> Option<ExecOutput> {

    // let msg: Message<Attribute> = match decode_message(data) {
    //     Ok(msg) => msg,
    //     Err(_e) => return None, // drop invalid packet
    // };

    if let Some(tsx) = transactions.remove(&msg.transaction_id()) {
        if let Some(tsx) = tsx {
            let rsp = TransactionRsp {
                response: msg,
                remote_addr,
            };

            return tsx.finish(Ok(rsp))
        }
    }
    None
}

pub fn try_binding_response_bytes(req: &Message<Attribute>, remote_addr: &SocketAddr) -> Option<Vec<u8>> {
    if let Some(rsp) = try_binding_response(req, remote_addr) {
        return encode_message(rsp).ok()
    } 
    None
}

fn try_binding_response(req: &Message<Attribute>, remote_addr: &SocketAddr) -> Option<Message<Attribute>> {
    if req.class() == MessageClass::Request {
        if req.method() == BINDING {
            let mut rsp = Message::new(MessageClass::SuccessResponse, BINDING, req.transaction_id());
            for attr in req.attributes() {
                rsp.add_attribute(attr.clone());
            }
            rsp.add_attribute(Attribute::XorMappedAddress(XorMappedAddress::new(*remote_addr)));
            return Some(rsp)
        }
    }
    None
}

fn gen_request(method: Method) -> Message<Attribute> {
    let tsx_id = rand::thread_rng().gen::<[u8; 12]>();
    let tsx_id = TransactionId::new(tsx_id);
    Message::new(MessageClass::Request, method, tsx_id)
}

fn encode_message(msg: Message<Attribute>) -> Result<Vec<u8>> {
    let mut encoder = MessageEncoder::new();
    let r = encoder.encode_into_bytes(msg)?;
    Ok(r)
}

pub fn decode_message(data: &[u8]) -> Result<Message<Attribute>> {
    let mut decoder = MessageDecoder::<Attribute>::new();

    decoder.decode_from_bytes(data)?
    .map_err(|e|anyhow!("{e:?}"))
}

fn try_parse_message(data: &[u8]) -> Option<Message<Attribute>> {
    if data.len() == 0 {
        return None;
    }

    // >  0                   1                   2                   3
    // >  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
    // > +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
    // > |0 0|     STUN Message Type     |         Message Length        |
    // > +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
    if data[0] & 0xC0 != 0 {
        return None
    }

    decode_message(data).ok()
}


#[derive(Debug)]
pub struct StunSocket<U> {
    socket: U,
}

impl<U> StunSocket<U> {
    pub fn new(socket: U) -> Self {
        Self { 
            socket, 
        }
    }
}

impl<U: AsyncUdpSocket> AsyncUdpSocket for StunSocket<U> {
    fn poll_send(
        &self,
        state: &quinn::udp::UdpState,
        cx: &mut task::Context,
        transmits: &[quinn::udp::Transmit],
    ) -> Poll<std::result::Result<usize, io::Error>> {
        let r = self.socket.poll_send(state, cx, transmits);
        r
    }

    fn poll_recv(
        &self,
        cx: &mut task::Context,
        bufs: &mut [io::IoSliceMut<'_>],
        meta: &mut [quinn::udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {

        loop {
            let r = ready!(self.socket.poll_recv(cx, bufs, meta));
            if let Ok(num) = &r {
                let mut index = 0_usize;
                for n in 0..*num {
                    let len = meta[n].len;
                    let data = &bufs[n][..len];
                    let addr = &meta[n].addr;
                    
                    let r = try_parse_message(data);
                    match r {
                        Some(msg) => {

                            if let Some(data) = try_binding_response_bytes(&msg, addr) {

                                let transmits = [Transmit {
                                    destination: addr.clone(),
                                    ecn: None,
                                    contents: data.into(),
                                    segment_size: None,
                                    src_ip: None,
                                }];
                        
                                let r = self.poll_send(udp_state(), cx, &transmits);
                                let _r = ready!(r);
                            } 
                        }, 
                        None => {
                            if index != n {
                                meta.swap(index, n);
                                bufs.swap(index, n);
                            }
                            index += 1;
                        },
                    }
                }
                if index > 0 {
                    return Poll::Ready(Ok(index))
                }
            } else {
                return Poll::Ready(r)
            }
        }
        
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        self.socket.set_ttl(ttl)
    }
}


#[cfg(test)]
mod test {
    use stun_codec::rfc5389::attributes::MessageIntegrity;

    use super::*;

    // use super::NatType;

    // use super::{BindingOutput, ReflexiveAddr};

    #[test]
    fn test_nat_detect_output_cone() {
        let mut obj = BindingOutput::default();
        assert!(obj.is_empty(), "{obj:?}") ;
        assert!(!obj.is_nat_detect_done(), "{obj:?}") ;
        assert_eq!(obj.nat_type(), None, "{obj:?}") ;

        obj.insert_unique(ReflexiveAddr {
            target: "1.1.1.1:1".parse().unwrap(),
            mapped: "9.9.9.9:9".parse().unwrap(),
        });

        assert!(!obj.is_empty(), "{obj:?}") ;
        assert!(!obj.is_nat_detect_done(), "{obj:?}") ;
        assert_eq!(obj.nat_type(), None, "{obj:?}") ;


        obj.insert_unique(ReflexiveAddr {
            target: "2.2.2.2:2".parse().unwrap(),
            mapped: "9.9.9.9:9".parse().unwrap(),
        });

        assert!(!obj.is_empty(), "{obj:?}") ;
        assert!(obj.is_nat_detect_done(), "{obj:?}") ;
        assert_eq!(obj.nat_type(), Some(NatType::Cone), "{obj:?}") ;

    }

    #[test]
    fn test_nat_detect_output_symmetric() {
        let mut obj = BindingOutput::default();
        assert!(obj.is_empty(), "{obj:?}") ;
        assert!(!obj.is_nat_detect_done(), "{obj:?}") ;
        assert_eq!(obj.nat_type(), None, "{obj:?}") ;

        obj.insert_unique(ReflexiveAddr {
            target: "1.1.1.1:1".parse().unwrap(),
            mapped: "9.9.9.9:9".parse().unwrap(),
        });

        assert!(!obj.is_empty(), "{obj:?}") ;
        assert!(!obj.is_nat_detect_done(), "{obj:?}") ;
        assert_eq!(obj.nat_type(), None, "{obj:?}") ;


        obj.insert_unique(ReflexiveAddr {
            target: "3.3.3.3:3".parse().unwrap(),
            mapped: "8.8.8.8:8".parse().unwrap(),
        });

        assert!(!obj.is_empty(), "{obj:?}") ;
        assert!(obj.is_nat_detect_done(), "{obj:?}") ;
        assert_eq!(obj.nat_type(), Some(NatType::Symmetric), "{obj:?}") ;
    }

    #[test]
    fn test_nat_detect_output_combine() {
        let mut obj = BindingOutput::default();
        assert!(obj.is_empty(), "{obj:?}") ;
        assert!(!obj.is_nat_detect_done(), "{obj:?}") ;
        assert_eq!(obj.nat_type(), None, "{obj:?}") ;

        obj.insert_unique(ReflexiveAddr {
            target: "1.1.1.1:1".parse().unwrap(),
            mapped: "9.9.9.9:9".parse().unwrap(),
        });

        assert!(!obj.is_empty(), "{obj:?}") ;
        assert!(!obj.is_nat_detect_done(), "{obj:?}") ;
        assert_eq!(obj.nat_type(), None, "{obj:?}") ;


        obj.insert_unique(ReflexiveAddr {
            target: "2.2.2.2:2".parse().unwrap(),
            mapped: "9.9.9.9:9".parse().unwrap(),
        });

        assert!(!obj.is_empty(), "{obj:?}") ;
        assert!(obj.is_nat_detect_done(), "{obj:?}") ;
        assert_eq!(obj.nat_type(), Some(NatType::Cone), "{obj:?}") ;

        obj.insert_unique(ReflexiveAddr {
            target: "3.3.3.3:3".parse().unwrap(),
            mapped: "8.8.8.8:8".parse().unwrap(),
        });

        assert!(!obj.is_empty(), "{obj:?}") ;
        assert!(obj.is_nat_detect_done(), "{obj:?}") ;
        assert_eq!(obj.nat_type(), Some(NatType::Symmetric), "{obj:?}") ;

    }

    #[test]
    fn test_integrity() {
        use super::Config;
        let pwd = "a123".to_string();
        let config = Config {
            username: Some("mike".into()),
            password: Some(pwd.clone()),
            ..Default::default()
        };

        let right_data = {
            let password = &pwd;
            let mut msg = config.gen_bind_req().unwrap();
            let integrity = MessageIntegrity::new_short_term_credential(&msg, password).unwrap();
            msg.add_attribute(integrity.into());
            encode_message(msg).unwrap()
        };

        let wrong_data = {
            let password = "wrong password";
            let mut msg = config.gen_bind_req().unwrap();
            let integrity = MessageIntegrity::new_short_term_credential(&msg, password).unwrap();
            msg.add_attribute(integrity.into());
            encode_message(msg).unwrap()
        };


        {
            let data = &right_data[..];
            let decoded_msg = decode_message(data).unwrap();
            let decode_integrity = decoded_msg.get_attribute::<MessageIntegrity>().unwrap();
            let r = decode_integrity.check_short_term_credential(&pwd);
            assert!(r.is_ok(), "{r:?}");
        }

        {
            let data = &wrong_data[..];
            let decoded_msg = decode_message(data).unwrap();
            let decode_integrity = decoded_msg.get_attribute::<MessageIntegrity>().unwrap();
            let r = decode_integrity.check_short_term_credential(&pwd);
            assert!(r.is_err(), "{r:?}");
        }

    }


}


#[tokio::test]
async fn test_stun() {

    let stun_servers = [
        // "100.110.119.120:3478", // none exist ip:port
        "none-exist-host-zytYad132.com:3478", // none exist domain
        "stun1.l.google.com:19302",
        "stun2.l.google.com:19302",
        "stun.qq.com:3478",
    ];

    println!("stun_servers: {stun_servers:?}");

    // let socket = Arc::new(UdpSocketExt::bind("0.0.0.0:0").await.unwrap());

    let addrs = detect_nat_type2(stun_servers.into_iter(), Config::default()).await.unwrap();
    println!("");
    println!("detect quick: {addrs:?}");
    println!("nat type: {:?}", addrs.nat_type());

    let addrs = detect_nat_type2(stun_servers.into_iter(), Config { 
        detect_all_server: true, 
        use_binding_fut: false,
        ..Default::default()
    }).await.unwrap();
    println!("");
    println!("detect all: {addrs:?}");
    println!("nat type: {:?}", addrs.nat_type());
}
