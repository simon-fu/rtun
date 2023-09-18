
use std::{net::{SocketAddr, IpAddr, Ipv4Addr}, time::Duration, collections::{HashMap, VecDeque, HashSet}, borrow::Borrow, hash::Hash, task::{Poll, self}, io};
use anyhow::{Result, Context, anyhow, bail};
use bytes::Bytes;
use futures::{stream::FuturesUnordered, StreamExt, ready};

use crate::ice::ice_candidate::Candidate;

use crate::stun::async_udp::{AsyncUdpSocket, AsUdpSocket, udp_state};
use rand::Rng;
use stun_codec::{Message, MessageClass, rfc5389::{methods::BINDING, attributes::{Software, XorMappedAddress, MappedAddress, Username, MessageIntegrity, Fingerprint}}, TransactionId, MessageEncoder, MessageDecoder, Method, rfc5245::{attributes::{IceControlling, IceControlled, Priority, UseCandidate}, errors::RoleConflict}};
use bytecodec::{EncodeExt, DecodeExt};
use tokio::{time::Instant, net::{ToSocketAddrs, lookup_host}};

use super::ice_attribute::Attribute;




pub async fn udp_run_until_done<U, O>(socket: &U, ops: &mut O) -> Result<()> 
where
    U: AsyncUdpSocket + Unpin,
    O: UdpOps,
{
    let mut buf = vec![0_u8; 1700];

    while !ops.is_done() {
        udp_run_one(socket, ops, &mut buf).await?;
    }
    udp_flush(socket, ops).await?;

    Ok(())
}

pub async fn udp_run_one<U, O>(socket: &U, ops: &mut O, buf: &mut [u8]) -> Result<()> 
where
    U: AsyncUdpSocket + Unpin,
    O: UdpOps,
{
    // tracing::debug!("try udp_flush, tx_que {}", ops.tx_que().len());
    udp_flush(socket, ops).await?;

    let socket = socket.as_socket();
    let next = ops.next_tick_time(Instant::now());
    tokio::select! {
        r = socket.recv_from(buf) => {
            // tracing::debug!("recv from {r:?}");
            let (len, from_addr) = r?;
            ops.input_data(&buf[..len], from_addr, Instant::now())?;
        }
        _r = tokio::time::sleep_until(next) => {
            ops.process_tick(Instant::now())?;
        }
    }
    Ok(())
}

pub async fn udp_flush<U, O>(socket: &U, ops: &mut O) -> Result<()> 
where
    U: AsyncUdpSocket + Unpin,
    O: UdpOps,
{
    while let Some(tx) = ops.tx_que().front() {
        let tx = tx.clone();
        match tx {
            UdpTx::Tx(packet) => {
                // tracing::debug!("send to {} bytes {}", packet.addr, packet.data.len());
                socket.as_socket().send_to(packet.data, packet.addr).await?;
            },
            UdpTx::SetTtl(ttl) => {
                // tracing::debug!("set ttl {ttl}");
                socket.set_ttl(ttl)?;
            },
        };
        ops.tx_que().pop_front();
    }
    Ok(())
}

pub trait UdpOps {
    fn is_done(&self) -> bool ;

    fn tx_que(&mut self) -> &mut UdpTxQue;

    fn next_tick_time(&mut self, now: Instant) -> Instant ;

    fn process_tick(&mut self, now: Instant) -> Result<()> ;

    fn input_data(&mut self, data: &[u8], from_addr: SocketAddr, now: Instant) -> Result<()> ;
}



#[derive(Default)]
pub struct StunResolver {
    pub software: Option<String>,
    pub tsx_timeout: Option<Duration>,
    pub all_addrs: bool,
    pub min_success_response: Option<usize>,

    tx_que: UdpTxQue,
    inflight: UdpTsxMap<TransactionId, ()>,
    timeouts: VecDeque<UdpTsx<()>>,
    mapped_addrs: MappedAddrs,
}

impl StunResolver {
    pub fn with_min_success(self, v: usize) -> Self {
        Self {
            min_success_response: Some(v),
            ..self
        }
    }

    pub fn with_resolve_all(self) -> Self {
        Self {
            all_addrs: true,
            ..self
        }
    }

    pub fn mapped_addrs(&self) -> &MappedAddrs {
        &self.mapped_addrs
    }

    pub fn into_mapped_addrs(self) -> MappedAddrs {
        self.mapped_addrs
    }

    pub fn is_empty(&self) -> bool {
        self.inflight.is_empty() && self.tx_que.is_empty()
    }

    pub fn kick_targets<I>(&mut self, iter: I, now: Instant) -> Result<()> 
    where
        I: Iterator<Item = SocketAddr>,
    {
        for target in iter {
            self.kick_target(target, now)?;
        }
        Ok(())
    }

    pub fn kick_target(&mut self, target: SocketAddr, now: Instant) -> Result<()> {
        // tracing::debug!("kick resolve target {target}");
        let req = self.make_req()?;
        
        let tsx_id = req.transaction_id();

        let data = encode_message(req)?;

        let tsx = UdpTsx {
            deadline: now + self.get_tsx_timeout(),
            packet: UdpPacket { addr: target, data: data.into(), },
            _ext: (),
        };
        
        self.tx_que.push_back(UdpTx::Tx(tsx.packet.clone()));
        self.inflight.insert(tsx_id, tsx);
        // tracing::debug!("kick resolve target {target}, tx_que {}", self.tx_que.len());

        Ok(())
    }


    fn make_req(&mut self) -> Result<Message<Attribute>> {
        let mut req: Message<Attribute> = gen_request(BINDING);

        if let Some(software) = self.software.as_ref() {
            req.add_attribute(Attribute::Software(Software::new(software.clone())?));
        }

        Ok(req)
    }

    fn process_success_rsp(
        &mut self, 
        rsp: Message<Attribute>,
        from_addr: SocketAddr,
        _now: Instant,
    ) -> Result<()> {

        let tsx_id = rsp.transaction_id();
        
        let tsx = match self.inflight.remove(&tsx_id) {
            Some(v) => v,
            None => return Ok(()),
        };

        let mapped = get_mapped_addr(&rsp).with_context(||"no mapped addr")?;
        
        self.mapped_addrs.insert_unique(mapped, from_addr, tsx.packet.addr);
        
        Ok(())
    }

    fn get_tsx_timeout(&self) -> Duration {
        self.tsx_timeout.unwrap_or_else(||Duration::from_secs(10))
    }

    pub async fn resolve<U, I, A>(&mut self, socket: &U, servers: I) -> Result<()> 
    where
        U: AsyncUdpSocket + Unpin,
        I: Iterator<Item = A>,
        A: ToSocketAddrs,
    {
        let resolver = self;

        let mut lookup_futures = FuturesUnordered::new();
        for host in servers {
            lookup_futures.push(lookup_host(host));
        }

        // if let Some(r) = lookup_futures.next().await {
        //     let iter = r?;
        //     resolver.kick_targets(iter, Instant::now())?;
        // }

        let mut buf = vec![0_u8; 1700];

        while !resolver.is_done() {
            if lookup_futures.is_empty() && resolver.is_empty() {
                break;
            }

            // tracing::debug!("run once: lookup empty {}, resolver empty {}, tx_que {}", lookup_futures.is_empty(), resolver.is_empty(), resolver.tx_que.len());

            tokio::select! {
                r = udp_run_one(socket, resolver, &mut buf), if !resolver.is_empty() => {
                    r?;
                },
                r = lookup_futures.next(), if !lookup_futures.is_empty() => {
                    if let Some(Ok(iter)) = r {
                        resolver.kick_targets(iter, Instant::now())?;
                    }
                }
            }
        }
        Ok(())
    }
}

impl UdpOps for StunResolver {
    fn is_done(&self) -> bool {
        self.mapped_addrs.num_success >= self.min_success_response.unwrap_or(1)
    }

    fn tx_que(&mut self) -> &mut UdpTxQue {
        &mut self.tx_que
    }

    fn next_tick_time(&mut self, _now: Instant) -> Instant  {
        self.inflight.next_tick_time()
    }

    fn process_tick(&mut self, now: Instant) -> Result<()>  {
        let r = self.inflight.process_tick(now, &mut self.tx_que, &mut self.timeouts);
        if let Err(e) = r {
            tracing::warn!("process_tick failed [{e:?}]");
        }
        self.timeouts.clear();
        Ok(())
    }

    fn input_data(&mut self, data: &[u8], from_addr: SocketAddr, now: Instant) -> Result<()>  {
        let mut func = move || -> Result<()> {
            let msg = decode_message(data)?;

            match (msg.class(), msg.method()) {
                (MessageClass::SuccessResponse, BINDING) => {
                    // tracing::debug!("is binding success response");
                    self.process_success_rsp(msg, from_addr, now)?;
    
                },
                r => {
                    tracing::debug!("unknown recv msg {r:?}");
                }
            }
            Ok(())
        };

        let r = func();
        if let Err(e) = r {
            tracing::warn!("input_data failed [{e:?}]");
        }

        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum NatType {
    Symmetric,
    Cone,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RAddr {
    Srflx(SocketAddr),
    Peerflx{
        target: SocketAddr,
        from: SocketAddr,
    }
}

impl RAddr {
    pub fn new(target: SocketAddr, from: SocketAddr) -> Self {
        if target == from {
            Self::Srflx(target)
        } else {
            Self::Peerflx { target, from, }
        }
    }
}



#[derive(Debug, Clone, Default)]
pub struct MappedAddrs {
    mapped: HashMap<SocketAddr, HashSet<RAddr>>, // mapped -> (target -> from)
    total: usize,
    num_success: usize,
}

impl MappedAddrs {
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


    pub fn mapped_iter<'a>(&'a self) -> impl Iterator<Item = SocketAddr> + 'a  {
        self.mapped.iter().map(|x|x.0.clone())
    }

    pub fn select_first_mapped<'a>(&'a self) -> Result<SocketAddr>  {
        self.mapped.iter()
        .next()
        .map(|x|x.0.clone())
        .with_context(||"empty mapped")
    }

    fn insert_unique(&mut self, mapped: SocketAddr, from: SocketAddr, target: SocketAddr) {
        let raddr = RAddr::new(target, from);
        if let Some(exist) = self.mapped.get_mut(&mapped) {
            exist.insert(raddr);
        } else {
            self.mapped.insert(mapped, [raddr].into());
        }
        self.total += 1;
        self.num_success += 1;
    }
}



#[derive(Debug, Default)]
pub struct CheckerConfig {
    software: Option<String>,
    local_creds: Option<IceCreds>,
    remote_creds: Option<IceCreds>,
    tsx_timeout: Option<Duration>,
    controlled: bool,
    tie_breaker: u64,
    base: Option<SocketAddr>,
}

impl CheckerConfig {
    pub fn with_base(self, base: SocketAddr) -> Self {
        Self {
            base: Some(base),
            ..self
        }
    }

    pub fn with_software(self, software: String) -> Self {
        Self {
            software: Some(software),
            ..self
        }
    }

    pub fn with_local_creds(self, creds: IceCreds) -> Self {
        Self {
            local_creds: Some(creds),
            ..self
        }
    }

    pub fn with_remote_creds(self, creds: IceCreds) -> Self {
        Self {
            remote_creds: Some(creds),
            ..self
        }
    }

    pub fn with_controlling(mut self, v: bool) -> Self {
        while self.tie_breaker == 0 {
            self.tie_breaker = rand::thread_rng().gen();
        }

        Self {
            controlled: !v,
            ..self
        }
    }

    pub fn into_exec(self) -> IceChecker {
        IceChecker {
            config: self,
            ..Default::default()
        }
    }

    fn base(&self) -> SocketAddr {
        self.base.unwrap_or_else(||SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0))
    }

    fn make_req(&self, cand: &Candidate, use_cand: bool) -> Result<Message<Attribute>> {
        let mut req: Message<Attribute> = gen_request(BINDING);

        if let Some(software) = self.software.as_ref() {
            req.add_attribute(Attribute::Software(Software::new(software.clone())?));
        }

        let creds = &self.local_creds;
        // tracing::debug!("make req, {:?}, {creds:?}, target {:?}, use_cand {use_cand}", req.transaction_id(), cand.addr());
        

        if let Some(creds) = creds {
            let username = Username::new(creds.username.clone())?;
            req.add_attribute(Attribute::Username(username));
        }

        // IceControlling/IceControlled
        let attr = if !self.controlled {
            Attribute::IceControlling(IceControlling::new(self.tie_breaker))
        } else {
            Attribute::IceControlled(IceControlled::new(self.tie_breaker))
        };
        req.add_attribute(attr);
        
        if use_cand {
            let attr = Attribute::UseCandidate(UseCandidate::new());
            req.add_attribute(attr);
        }

        {
            let prio = cand.prio();
            let attr = Attribute::Priority(Priority::new(prio));
            req.add_attribute(attr);
        }
        

        if let Some(creds) = creds {
            let integrity = MessageIntegrity::new_short_term_credential(&req, &creds.password)?;
            req.add_attribute(integrity.into());
        }

        Ok(req)
    }

    fn make_rsp(&self, req: &Message<Attribute>, from_addr: SocketAddr) -> Result<Message<Attribute>> {
        let creds = &self.remote_creds;

        let mut rsp = Message::new(MessageClass::SuccessResponse, BINDING, req.transaction_id());
        // tracing::debug!("make rsp, {:?}, {creds:?}, {:?}", rsp.transaction_id(), from_addr);
        
        rsp.add_attribute(Attribute::XorMappedAddress(XorMappedAddress::new(from_addr)));

        if let Some(creds) = creds {
            let integrity = MessageIntegrity::new_short_term_credential(&rsp, &creds.password)?;
            rsp.add_attribute(integrity.into());
        }
        
        rsp.add_attribute(Attribute::Fingerprint(Fingerprint::new(&rsp)?));

        Ok(rsp)
    }

    fn try_make_rsp_if_req(&self, msg: Message<Attribute>, from_addr: &SocketAddr) -> Result<Vec<u8>> {

        match (msg.class(), msg.method()) {
            (MessageClass::Request, BINDING) => {
                let rsp = self.make_rsp(&msg, *from_addr)?;
                let data = encode_message(rsp)?;
                return Ok(data)
            },
            _ => {
                bail!("NOT binding req")
            },
        }
    }

    fn make_role_conflict_rsp(&self, req: &Message<Attribute>, from_addr: SocketAddr) -> Result<Message<Attribute>> {
        let creds = &self.local_creds;
        // tracing::debug!("make role conflict with creds {creds:?}");

        let mut rsp = Message::new(MessageClass::ErrorResponse, BINDING, req.transaction_id());
        
        
        rsp.add_attribute(Attribute::ErrorCode(RoleConflict.into()));
        rsp.add_attribute(Attribute::XorMappedAddress(XorMappedAddress::new(from_addr)));

        if let Some(creds) = creds {
            let integrity = MessageIntegrity::new_short_term_credential(&req, &creds.password)?;
            rsp.add_attribute(integrity.into());
        }
        
        rsp.add_attribute(Attribute::Fingerprint(Fingerprint::new(&rsp)?));

        Ok(rsp)
    }

    fn make_req_tsx(&self, cand: &Candidate, use_cand: bool, now: Instant) -> Result<(TransactionId, UdpTsx<()>)> {
        let req = self.make_req(&cand, use_cand)?;
        
        let tsx_id = req.transaction_id();

        let data = encode_message(req)?;

        let tsx = UdpTsx {
            deadline: now + self.get_tsx_timeout(),
            packet: UdpPacket { addr: cand.addr(), data: data.into(), },
            _ext: (),
        };

        Ok((tsx_id, tsx))
    }

    fn kick_req(&self, cand: &Candidate, use_cand: bool, now: Instant, tx_que: &mut UdpTxQue, inflight: &mut UdpTsxMap<TransactionId, ()>) -> Result<()> {

        let (tsx_id, tsx) = self.make_req_tsx(cand, use_cand, now)?;
        
        tx_que.push_back(UdpTx::Tx(tsx.packet.clone()));
        inflight.insert(tsx_id, tsx);

        Ok(())
    }
    
    fn get_tsx_timeout(&self) -> Duration {
        self.tsx_timeout.unwrap_or_else(||Duration::from_secs(10))
    }

    fn check_req_integrity(&self, msg: &Message<Attribute>) -> Result<()> {
        match &self.remote_creds {
            Some(creds) => check_integrity(msg, Some(&creds.username), Some(&creds.password)),
            None => check_integrity(msg, None, None),
        }
    }

    fn check_rsp_integrity(&self, msg: &Message<Attribute>) -> Result<()> {
        match &self.local_creds {
            Some(creds) => check_integrity(msg, None, Some(&creds.password)),
            None => check_integrity(msg, None, None),
        }
    }
}

enum CheckResult {
    Selected(SocketAddr),
    Timeout,
}

#[derive(Default)]
pub struct IceChecker {
    config: CheckerConfig,

    remote_candidates: HashMap<SocketAddr, Candidate>,
    success_candidates: HashSet<SocketAddr>, // recv success
    // local_candidates: HashMap<SocketAddr, Candidate>,

    tx_que: UdpTxQue,
    inflight: UdpTsxMap<TransactionId, ()>,
    timeouts: VecDeque<UdpTsx<()>>,

    start_at: Option<Instant>,
    nominated_tsx_id: Option<TransactionId>,
    upgraded_ttl: bool,
    result: Option<CheckResult>,
}

impl UdpOps for IceChecker {
    fn is_done(&self) -> bool  {
        self.result.is_some()
    }

    fn tx_que(&mut self) -> &mut UdpTxQue {
        &mut self.tx_que
    }

    fn next_tick_time(&mut self, _now: Instant) -> Instant  {
        self.inflight.next_tick_time()
    }

    fn process_tick(&mut self, now: Instant) -> Result<()>  {

        let r = self.inflight.process_tick(now, &mut self.tx_que, &mut self.timeouts);
        if let Err(e) = r {
            tracing::debug!("process_tick faild [{e:?}]");
        }
        self.timeouts.clear();

        if let Some(start) = self.start_at {
            if start.elapsed() >= self.config.get_tsx_timeout() && self.result.is_none() {
                self.result = Some(CheckResult::Timeout);
            }
        }

        Ok(())
    }

    fn input_data(&mut self, data: &[u8], from_addr: SocketAddr, now: Instant) -> Result<()>  {
        let r = self.do_input_data(data, from_addr, now);
        if let Err(e) = r {
            tracing::warn!("input_data faild [{e:?}]");
        }
        Ok(())
    }
}

impl IceChecker {

    pub fn into_socket<U>(self, socket: U, ) -> IceSocket<U> {
        IceSocket {
            socket,
            config: self.config,
        }
    }

    pub fn config(&self) -> &CheckerConfig {
        &self.config
    }

    pub fn add_remote_candidate(&mut self, cand: Candidate) {
        self.remote_candidates.insert(cand.addr(), cand);
    }

    pub fn selected_addr(&self) -> Option<SocketAddr> {
        match self.result {
            Some(CheckResult::Selected(addr)) => Some(addr),
            _ => None,
        }
    }

    pub fn prepare_checking(&mut self) -> Result<()> {
        if self.config.controlled {
            for ttl in 1..4 {
                self.tx_que.push_back(UdpTx::SetTtl(ttl));
                for (_addr, cand) in self.remote_candidates.iter() {
                    let req = self.config.make_req(cand, false)?;
                    let data = encode_message(req)?;
                    self.tx_que.push_back(UdpTx::Tx(UdpPacket { 
                        addr: cand.addr(),
                        data: data.into(), 
                    }));
                }
            }
        }
        Ok(())
    }

    pub fn kick_checking(&mut self, now: Instant) -> Result<()> {
        self.start_at = Some(now);
        for (_addr, cand) in self.remote_candidates.iter() {
            self.config.kick_req(cand, false, now, &mut self.tx_que, &mut self.inflight)?;
        }
        Ok(())
    }

    fn do_input_data(&mut self, data: &[u8], from_addr: SocketAddr, now: Instant) -> Result<()> {
        let msg = decode_message(data)?;

        match (msg.class(), msg.method()) {
            (MessageClass::Request, BINDING) => {
                // tracing::debug!("is binding request");
                self.config.check_req_integrity(&msg)?;
                self.process_req(msg, from_addr, now)?;

            },
            (MessageClass::SuccessResponse, BINDING) => {
                // tracing::debug!("is binding success response");
                self.config.check_rsp_integrity(&msg)?;
                self.process_success_rsp(msg, from_addr, now)?;

            },
            // (MessageClass::ErrorResponse, BINDING) => {
            //     tracing::debug!("is binding error response");
            //     self.config.check_rsp_integrity(&msg)?;
            //     self.process_error_rsp(msg, from_addr, now)?;

            // },
            r => {
                tracing::debug!("unknown recv msg {r:?}");

            }
        }

        Ok(())
    }


    fn process_req(
        &mut self, 
        req: Message<Attribute>,
        from_addr: SocketAddr,
        now: Instant,
    ) -> Result<()> {
        
        if !self.upgraded_ttl {
            self.upgraded_ttl = true;
            self.tx_que.push_back(UdpTx::SetTtl(64));
        }

        if !self.config.controlled {
            /* 
            o  If the agent is in the controlling role, and the ICE-CONTROLLING
               attribute is present in the request:
                *  If the agent's tie-breaker is larger than or equal to the
                   contents of the ICE-CONTROLLING attribute, the agent generates
                   a Binding error response and includes an ERROR-CODE attribute
                   with a value of 487 (Role Conflict) but retains its role.

                *  If the agent's tie-breaker is less than the contents of the
                   ICE-CONTROLLING attribute, the agent switches to the controlled
                   role.
            */

            let r = req.get_attribute::<IceControlling>().map(|x|x.prio());
            if let Some(remote_tie_breaker) = r {
                if self.config.tie_breaker >= remote_tie_breaker {
                    tracing::warn!("conflict role controllig, local {}, remote {remote_tie_breaker}, local win", self.config.tie_breaker);
                    let rsp = self.config.make_role_conflict_rsp(&req, from_addr)?;
                    let data = encode_message(rsp)?;
                    self.tx_que.push_back(UdpTx::Tx(UdpPacket { 
                        addr: from_addr, 
                        data: data.into() 
                    }));
                    return Ok(())
                } else {
                    tracing::warn!("conflict role controllig, local {}, remote {remote_tie_breaker}, remote win", self.config.tie_breaker);
                    self.config.controlled = true;
                }
            }
            
        } else {
            /*
            o  If the agent is in the controlled role, and the ICE-CONTROLLED
                attribute is present in the request:

                *  If the agent's tie-breaker is larger than or equal to the
                    contents of the ICE-CONTROLLED attribute, the agent switches to
                    the controlling role.

                *  If the agent's tie-breaker is less than the contents of the
                    ICE-CONTROLLED attribute, the agent generates a Binding error
                    response and includes an ERROR-CODE attribute with a value of
                    487 (Role Conflict) but retains its role.
            */

            let r = req.get_attribute::<IceControlled>().map(|x|x.prio());
            if let Some(remote_tie_breaker) = r {
                if self.config.tie_breaker >= remote_tie_breaker {
                    self.config.controlled = false;
                    tracing::warn!("conflict role controlled, local {}, remote {remote_tie_breaker}, local win", self.config.tie_breaker);
                    let rsp = self.config.make_role_conflict_rsp(&req, from_addr)?;
                    let data = encode_message(rsp)?;
                    self.tx_que.push_back(UdpTx::Tx(UdpPacket { 
                        addr: from_addr, 
                        data: data.into() 
                    }));
                    return Ok(())
                } else {
                    tracing::warn!("conflict role controlled, local {}, remote {remote_tie_breaker}, remote win", self.config.tie_breaker);
                }
            }

        }

        let prio = req.get_attribute::<Priority>().with_context(||"no priority in req")?.prio();

        let use_cand = req.get_attribute::<UseCandidate>();

        if self.config.controlled {            
            if use_cand.is_some() {
                if !self.success_candidates.contains(&from_addr) {
                    // ignore nominated until recv response
                    return Ok(())
                } else {
                    // tracing::debug!("got use_cand, selected {from_addr}");
                    self.result = Some(CheckResult::Selected(from_addr));
                }
            }
        }

        let rsp = self.config.make_rsp(&req, from_addr)?;
        let data = encode_message(rsp)?;
        self.tx_que.push_back(UdpTx::Tx(UdpPacket { 
            addr: from_addr, 
            data: data.into() 
        }));

        if let Some(_cand) = self.remote_candidates.get(&from_addr) {

        } else {
            let new_cand = Candidate::peer_reflexive(from_addr, self.config.base(), prio, None, None);
        
            // self.kick_req(&new_cand, false, now)?;
            self.config.kick_req(&new_cand, false, now, &mut self.tx_que, &mut self.inflight)?;
    
            self.remote_candidates.insert(new_cand.addr(), new_cand);
        }

        Ok(())
    }

    fn process_success_rsp(
        &mut self, 
        rsp: Message<Attribute>,
        from_addr: SocketAddr,
        now: Instant,
    ) -> Result<()> {


        let tsx_id = rsp.transaction_id();
        
        let tsx = match self.inflight.remove(&tsx_id) {
            Some(v) => v,
            None => return Ok(()),
        };


        let _mapped = get_mapped_addr(&rsp).with_context(||"no mapped addr");
        // let prio = req.get_attribute::<Priority>().with_context(||"no priority in req")?.prio();

        if let Some(cand) = self.remote_candidates.get(&from_addr) {
            
            self.success_candidates.insert(from_addr);

            if !self.config.controlled {
                match &self.nominated_tsx_id {
                    Some(nominated_tsx_id) => {
                        if tsx_id == *nominated_tsx_id {
                            // tracing::debug!("recv nominate reponse, addr {from_addr}, {tsx_id:?}");
                            self.result = Some(CheckResult::Selected(from_addr));
                        }
                    },
                    None => {
                        let (tsx_id, tsx) = self.config.make_req_tsx(cand, true, now)?;
                        // tracing::debug!("nominate addr {from_addr}, {tsx_id:?}");
                        self.nominated_tsx_id = Some(tsx_id);
        
                        self.tx_que.push_back(UdpTx::Tx(tsx.packet.clone()));
                        self.inflight.insert(tsx_id, tsx);
                    },
                }
            }
            return Ok(())
        }

        if let Some(cand) = self.remote_candidates.get(&tsx.packet.addr) {
            let new_cand = Candidate::peer_reflexive(from_addr, self.config.base(), cand.prio(), None, None);
        
            // self.kick_req(&new_cand, false, now)?;
            self.config.kick_req(&new_cand, false, now, &mut self.tx_que, &mut self.inflight)?;
    
            self.remote_candidates.insert(new_cand.addr(), new_cand);
            
        }
        
        Ok(())
    }

}



#[derive(Debug)]
pub struct IceSocket<U> {
    socket: U,
    config: CheckerConfig,
}

// impl<U> IceSocket<U> {
//     pub fn new(socket: U) -> Self {
//         Self { 
//             socket, 
//         }
//     }
// }

impl<U: AsyncUdpSocket> AsyncUdpSocket for IceSocket<U> {
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
                            let r = self.config.try_make_rsp_if_req(msg, addr).ok();
                            if let Some(data) = r {
                                let transmits = [quinn::udp::Transmit {
                                    destination: addr.clone(),
                                    ecn: None,
                                    contents: data.into(),
                                    segment_size: None,
                                    src_ip: None,
                                }];
                                // tracing::debug!("got binding req, response it");
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



#[derive(Debug)]
pub struct IceCreds {
    pub username: String,
    pub password: String,
}


struct UdpTsx<V> {
    deadline: Instant,
    packet: UdpPacket,
    _ext: V,
}


struct UdpTsxMap<K, V> {
    next_tick_time: Instant,
    map: HashMap<K, Option<UdpTsx<V>>>,
}

impl<K, V> Default for UdpTsxMap<K, V> {
    fn default() -> Self {
        Self { next_tick_time: Instant::now(), map: Default::default() }
    }
}

impl<K, V> UdpTsxMap<K, V> {

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn insert(&mut self, k: K, tsx: UdpTsx<V> ) -> Option<UdpTsx<V>> 
    where
        K: Eq + Hash,
    {
        
        match self.map.insert(k, Some(tsx)) {
            Some(old) => old,
            None => None,
        }
    }

    pub fn remove<Q>(&mut self, k: &Q) -> Option<UdpTsx<V>> 
    where
        K: Eq + Hash,
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        
        self.map.remove(k).unwrap_or(None)
    }

    pub fn process_tick(&mut self, now: Instant, io_que: &mut UdpTxQue, timeouts: &mut VecDeque<UdpTsx<V>>,) -> Result<()> {

        self.map.retain(|_k, tsx| {
            if let Some(tsx) = tsx {
                if now < tsx.deadline {
                    io_que.push_back(UdpTx::Tx(tsx.packet.clone()));
                    return true
                } 
            }
            
            if let Some(tsx) = tsx.take() {
                timeouts.push_back(tsx); 
            }

            false

        });

        self.update_next_tick_time(now);

        Ok(())
    }

    pub fn next_tick_time(&self) -> Instant {
        self.next_tick_time
    }

    fn update_next_tick_time(&mut self, now: Instant) {
        self.next_tick_time =  now + Duration::from_millis(200);
    }
}


#[derive(Debug, Clone)]
pub enum UdpTx {
    Tx(UdpPacket),
    SetTtl(u32),
}

pub type UdpTxQue = VecDeque<UdpTx>;


#[derive(Debug, Clone)]
pub struct UdpPacket {
    addr: SocketAddr,
    data: Bytes,
}



fn check_integrity(msg: &Message<Attribute>, username: Option<&str>, password: Option<&str>) -> Result<()> {
    // tracing::debug!("check_integrity: {:?}, user {username:?}, pass {password:?}", msg.transaction_id());
    if let Some(username) = username {
        if let Some(attr) = msg.get_attribute::<Username>() {
            if attr.name() != username {
                bail!("expect username [{username}] but [{}]", attr.name())
            }
        } else {
            bail!("no username")
        }
    }

    if let Some(password) = password {
        if let Some(integrity) = msg.get_attribute::<MessageIntegrity>() {
            integrity.check_short_term_credential(password).map_err(|e|anyhow!("integrity failed: [{e:?}]"))?;
        } else {
            bail!("no integrity")
        }
    }

    Ok(())
}

fn get_mapped_addr(rsp: &Message<Attribute>) -> Option<SocketAddr> {
    let map_addr1 = rsp.get_attribute::<XorMappedAddress>().map(|x|x.address());
    let map_addr3 = rsp.get_attribute::<MappedAddress>().map(|x|x.address());
    let map_addr = map_addr1.or(map_addr3);
    // mapp_addr.with_context(||"no mapped addr in rsp")?;
    map_addr
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


#[cfg(test)]
mod test {
    use crate::stun::async_udp::tokio_socket_bind;

    use super::*;

    #[tokio::test]
    async fn test_resolve_basic() {
        tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_env_filter(tracing_subscriber::EnvFilter::from("rtun=debug"))
        .with_target(false)
        .init();

        let socket = tokio_socket_bind("0.0.0.0:0").await.unwrap();

        let mut resolver = StunResolver::default();
        resolver.resolve(&socket, [
            // "100.110.119.120:3478", // none exist ip:port
            "none-exist-host-zytYad132.com:3478", // none exist domain
            "stun1.l.google.com:19302",
            "stun2.l.google.com:19302",
            "stun.qq.com:3478",
        ].into_iter()).await.unwrap();

        let mapped = resolver.into_mapped_addrs();
        
        tracing::debug!("{mapped:?}");
    }

    #[tokio::test]
    async fn test_resolve_nat() {
        tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_env_filter(tracing_subscriber::EnvFilter::from("rtun=debug"))
        .with_target(false)
        .init();

        let socket = tokio_socket_bind("0.0.0.0:0").await.unwrap();

        let mut resolver = StunResolver::default()
        .with_min_success(2);

        resolver.resolve(&socket, [
            // "100.110.119.120:3478", // none exist ip:port
            "none-exist-host-zytYad132.com:3478", // none exist domain
            "stun1.l.google.com:19302",
            "stun2.l.google.com:19302",
            "stun.qq.com:3478",
        ].into_iter()).await.unwrap();

        let mapped = resolver.into_mapped_addrs();
        tracing::debug!("{mapped:?}");
        tracing::debug!("{:?}", mapped.nat_type());
    }

    #[tokio::test]
    async fn test_resolve_inexist() {    
        tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_env_filter(tracing_subscriber::EnvFilter::from("rtun=debug"))
        .with_target(false)
        .init();

        let start = Instant::now();
    
        let socket = tokio_socket_bind("0.0.0.0:0").await.unwrap();

        let mut resolver = StunResolver::default();
        
        resolver.resolve(&socket, ["111.222.111.222:11111"].into_iter()).await.unwrap();

        // let e = BindingConfig::default()
        // .resolve_mapped_addr(["111.222.111.222:11111"].into_iter()).await.unwrap_err();

        assert!(resolver.mapped_addrs().is_empty(), "{:?}", resolver.mapped_addrs());
        assert!(start.elapsed() > Duration::from_secs(8), "elapsed {:?}", start.elapsed());
    }

    // #[tokio::test]
    // async fn test_binding_basic() {
    //     tracing_subscriber::fmt()
    //     .with_max_level(tracing::Level::INFO)
    //     .with_env_filter(tracing_subscriber::EnvFilter::from("rtun=debug"))
    //     .with_target(false)
    //     .init();

    //     BindingConfig::default()
    //     .resolve_mapped_addr([
    //         // "100.110.119.120:3478", // none exist ip:port
    //         "none-exist-host-zytYad132.com:3478", // none exist domain
    //         "stun1.l.google.com:19302",
    //         "stun2.l.google.com:19302",
    //         "stun.qq.com:3478",
    //     ].into_iter()).await.unwrap();
    // }

    // #[tokio::test]
    // async fn test_binding_inexist() {    
    //     let start = Instant::now();
    
    //     let e = BindingConfig::default()
    //     .resolve_mapped_addr(["111.222.111.222:11111"].into_iter()).await.unwrap_err();

    //     assert_eq!(format!("{e:?}"), "timed out");
    //     assert!(start.elapsed() > Duration::from_secs(8), "elapsed {:?}", start.elapsed());
    // }

    // #[tokio::test]
    // async fn test_binding_empty() {    
    //     let empty: [SocketAddr; 0] = [];
    //     let (_socket, output) = BindingConfig::default()
    //     .resolve_mapped_addr(empty.into_iter()).await.unwrap();

    //     assert!(output.is_empty(), "{output:?}");
    // }
}


