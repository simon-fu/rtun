use std::{collections::HashMap, fmt, io, net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{bail, Context, Result};
use chrono::Local;

use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use parking_lot::Mutex;
use rtun::{
    async_rt::spawn_with_name,
    channel::{ch_stream::ChStream, ChId, ChPair},
    p2p::hard_nat::{
        HARD_NAT_MAX_BATCH_INTERVAL_MS, HARD_NAT_MAX_INTERVAL_MS, HARD_NAT_MAX_SCAN_COUNT,
        HARD_NAT_MAX_SOCKET_COUNT, HARD_NAT_MAX_TTL,
    },
    proto::OpenSocksArgs,
    switch::{
        agent::ch_socks::run_socks5_conn_bridge,
        invoker_ctrl::{CtrlHandler, CtrlInvoker},
        next_ch_id::NextChId,
        session_stream::{make_stream_session, StreamSession},
        switch_sink::PacketSink,
        switch_source::PacketSource,
    },
    ws::client::ws_connect_to,
};
use shadowsocks_service::local::socks::config::Socks5AuthConfig;
use tokio::net::{TcpListener, TcpStream};

use super::{
    p2p_throughput::kick_p2p,
    quic_pool::{QuicPool, QuicPoolInvoker},
};
use crate::{
    client_utils::{client_select_url, query_new_agents},
    cmd_socks::quic_pool::{
        make_pool, AddAgent, GetCh, QuicSocksHardNatConfig, SocksHardNatModeCli,
        SocksHardNatRoleCli, DEFAULT_P2P_HARDNAT_BATCH_INTERVAL_MS,
        DEFAULT_P2P_HARDNAT_INTERVAL_MS, DEFAULT_P2P_HARDNAT_SCAN_COUNT,
        DEFAULT_P2P_HARDNAT_SOCKET_COUNT,
    },
    rest_proto::{get_agent_from_url, make_sub_url, make_ws_scheme, AgentInfo},
    secret::token_gen,
};

fn resolve_quic_socks_hard_nat_config(args: &CmdArgs) -> Result<QuicSocksHardNatConfig> {
    let interval_ms = u32::try_from(args.p2p_hardnat_interval).with_context(|| {
        format!(
            "p2p hardnat interval too large [{}ms]",
            args.p2p_hardnat_interval
        )
    })?;
    let batch_interval_ms = u32::try_from(args.p2p_hardnat_batch_interval).with_context(|| {
        format!(
            "p2p hardnat batch interval too large [{}ms]",
            args.p2p_hardnat_batch_interval
        )
    })?;

    if args.p2p_hardnat_socket_count == 0 {
        bail!("p2p hardnat socket count must be >= 1");
    }
    if args.p2p_hardnat_socket_count > HARD_NAT_MAX_SOCKET_COUNT {
        bail!(
            "p2p hardnat socket count too large [{}], max [{}]",
            args.p2p_hardnat_socket_count,
            HARD_NAT_MAX_SOCKET_COUNT
        );
    }
    if args.p2p_hardnat_scan_count == 0 {
        bail!("p2p hardnat scan count must be >= 1");
    }
    if args.p2p_hardnat_scan_count > HARD_NAT_MAX_SCAN_COUNT {
        bail!(
            "p2p hardnat scan count too large [{}], max [{}]",
            args.p2p_hardnat_scan_count,
            HARD_NAT_MAX_SCAN_COUNT
        );
    }
    if interval_ms == 0 {
        bail!("p2p hardnat interval must be >= 1ms");
    }
    if interval_ms > HARD_NAT_MAX_INTERVAL_MS {
        bail!(
            "p2p hardnat interval too large [{}ms], max [{}ms]",
            interval_ms,
            HARD_NAT_MAX_INTERVAL_MS
        );
    }
    if batch_interval_ms == 0 {
        bail!("p2p hardnat batch interval must be >= 1ms");
    }
    if batch_interval_ms > HARD_NAT_MAX_BATCH_INTERVAL_MS {
        bail!(
            "p2p hardnat batch interval too large [{}ms], max [{}ms]",
            batch_interval_ms,
            HARD_NAT_MAX_BATCH_INTERVAL_MS
        );
    }
    if matches!(args.p2p_hardnat_ttl, Some(0)) {
        bail!("p2p hardnat ttl must be >= 1");
    }
    if let Some(ttl) = args.p2p_hardnat_ttl {
        if ttl > HARD_NAT_MAX_TTL {
            bail!(
                "p2p hardnat ttl too large [{}], max [{}]",
                ttl,
                HARD_NAT_MAX_TTL
            );
        }
    }

    Ok(QuicSocksHardNatConfig {
        mode: args.p2p_hardnat,
        role: args.p2p_hardnat_role,
        socket_count: args.p2p_hardnat_socket_count,
        scan_count: args.p2p_hardnat_scan_count,
        interval_ms,
        batch_interval_ms,
        ttl: args.p2p_hardnat_ttl,
        no_ttl: args.p2p_hardnat_no_ttl,
    })
}

pub fn run(args: CmdArgs) -> Result<()> {
    // init_log_and_run(do_run(args))?

    let multi = MultiProgress::new();
    {
        let multi = multi.clone();
        crate::init_log2(move || LogWriter::new(multi.clone()));
    }

    crate::async_rt::run_multi_thread(async move { do_run(args, multi).await })??;
    Ok(())
}

async fn do_run(args: CmdArgs, multi: MultiProgress) -> Result<()> {
    let quic_socks_hard_nat = resolve_quic_socks_hard_nat_config(&args)?;
    if let Some(_socks_ws) = &args.socks_ws {
        kick_ws_socks(args.clone()).await?;
    }

    // let agent_pool = AgentPool::new();
    let pool = make_pool("pool".into(), multi.clone())?;

    let url = url::Url::parse(&args.url).with_context(|| "invalid url")?;

    let listen_addr = &args.listen;
    let listen_addr: SocketAddr = listen_addr
        .parse()
        .with_context(|| format!("invalid addr {listen_addr:?}"))?;

    let mut bars = Vec::new();
    const STYLE_GENERAL: &str = "{prefix:.bold.dim} {spinner} {wide_msg:}";

    let mut _clash_server = None;
    {
        for nn in 0..1 {
            let port = listen_addr.port() + nn;
            let listen_addr = SocketAddr::new(listen_addr.ip(), port);
            let listener = TcpListener::bind(listen_addr)
                .await
                .with_context(|| format!("fail to bind address [{listen_addr}]"))?;
            tracing::info!("socks5(quic) listen on [{listen_addr}]");

            let bar = multi.add(ProgressBar::new(100));
            bar.set_style(ProgressStyle::with_template(STYLE_GENERAL)?);
            bar.set_message(format!("socks: {listen_addr}"));
            bars.push(bar);

            let pool = pool.invoker().clone();

            spawn_with_name(format!("local_socks-{port}"), async move {
                let r = run_socks_via_quic(pool, listener).await;
                r
            });
        }
    }

    if args.tls_key.is_some() || args.tls_cert.is_some() {
        // let key_path = "/Users/simon/simon/src/my_keys/simon.home/simon.home.rtcsdk.com.key";
        // let cert_path = "/Users/simon/simon/src/my_keys/simon.home/simon.home.rtcsdk.com.pem";

        // let key_path = "/tmp/pem/key.pem";
        // let cert_path = "/tmp/pem/cert.pem";

        // let verifier = tls_util::SpecificClientCertVerifier::try_load("/tmp/pem/cert.pem").await?;
        // let verifier = Arc::new(verifier);
        // let verifier = tls_util::client_cert_verifier("/tmp/pem/ca.key.pem").await?;

        let (key_path, cert_path) = match (args.tls_key.as_ref(), args.tls_cert.as_ref()) {
            (Some(k), Some(c)) => (k, c),
            _ => bail!("must have tls_key and tls_cert"),
        };

        let mut opt_username = None;
        let mut opt_password = None;

        let auth = {
            let mut cfg = Socks5AuthConfig::new();
            for ss in args.tls_user.iter() {
                let mut split = ss.split(":");
                let user_name = split.next().with_context(|| "no tls username")?;
                let password = split.next().with_context(|| "no tls password")?;
                cfg.passwd.add_user(user_name, password);

                opt_username = Some(user_name.to_string());
                opt_password = Some(password.to_string());
                // cfg.passwd.add_user("rtun", "123");
            }

            // if let Some(ss) = args.tls_user.as_ref() {
            //     let mut split = ss.split(":");
            //     let user_name = split.next().with_context(||"no tls username")?;
            //     let password = split.next().with_context(||"no tls password")?;
            //     cfg.passwd.add_user(user_name, password);

            //     opt_username = Some(user_name.to_string());
            //     opt_password = Some(password.to_string());
            //     // cfg.passwd.add_user("rtun", "123");
            // }

            Arc::new(cfg)
        };

        let raw_cert = tokio::fs::read(cert_path).await?;
        let raw_key = tokio::fs::read(key_path).await?;

        let tls_cfg = tls_util::raw_cert_to_server_cfg(raw_key, raw_cert, None)?;
        let acceptor = tls_util::acceptor_from_cfg(tls_cfg.clone())?;

        let listen_addr: SocketAddr = match &args.tls_listen {
            Some(v) => v.parse().with_context(|| format!("invalid addr {v:?}"))?,
            None => SocketAddr::new(listen_addr.ip(), listen_addr.port() + 1),
        };

        // let mut listen_addr: SocketAddr = listen_addr.parse().with_context(|| format!("invalid addr {listen_addr:?}"))?;
        // listen_addr.set_port(listen_addr.port() + 1);

        let mut addrs = Vec::new();

        for nn in 0..1 {
            let port = listen_addr.port() + nn;
            let listen_addr = SocketAddr::new(listen_addr.ip(), port);
            let listener = TcpListener::bind(listen_addr)
                .await
                .with_context(|| format!("fail to bind address [{listen_addr}]"))?;
            addrs.push(listen_addr);
            tracing::info!("tls_socks5(quic) listen on [{listen_addr}]");

            let bar = multi.add(ProgressBar::new(100));
            bar.set_style(ProgressStyle::with_template(STYLE_GENERAL)?);
            bar.set_message(format!(
                "socks5-over-tls: {listen_addr} {:?}",
                args.tls_user
            ));
            bars.push(bar);

            let pool = pool.invoker().clone();
            let acceptor = acceptor.clone();
            let auth = auth.clone();
            spawn_with_name(format!("tls_socks-{port}"), async move {
                let r = run_tls_socks_via_quic(pool, listener, acceptor, auth).await;
                r
            });
        }

        if let Some(listen_addr) = &args.clash_listen {
            let listen_addr = listen_addr.parse()?;
            let web_path = "/clash/rtun.yml";
            let dns_name = tls_util::try_load_cert_dns_name(cert_path).await?;

            let content = ClashContent {
                addrs: &addrs,
                server: &dns_name,
                username: opt_username.as_deref(),
                password: opt_password.as_deref(),
            }
            .to_string();

            let server =
                clash_rest::server_for_clash(listen_addr, web_path, Some(tls_cfg), content).await?;
            _clash_server = Some(server);

            let clash_text = format!("clash: https://{dns_name}:{}{web_path}", listen_addr.port());

            let bar = multi.add(ProgressBar::new(100));
            bar.set_style(ProgressStyle::with_template(STYLE_GENERAL)?);
            bar.set_message(clash_text);
            bars.push(bar);
        }
    }

    {
        let agent_expr = match &args.agent {
            Some(expr) => {
                if !contains_regex_chars(expr) {
                    tracing::info!("agent name is simple string");
                    add_agents(
                        &pool,
                        &url,
                        &args,
                        quic_socks_hard_nat,
                        [expr.clone()].into_iter(),
                    )
                    .await?;
                    tokio::time::sleep(Duration::MAX / 2).await;
                    return Ok(());
                }
                expr
            }
            None => ".*",
        };

        tracing::info!("agent name expr: [{agent_expr}]");

        let agent_regex =
            regex::Regex::new(agent_expr).with_context(|| "invalid agent name regular expr")?;

        let mut agents = HashMap::new();

        loop {
            let r = if url.scheme().eq_ignore_ascii_case("quic") {
                query_new_agents_quic(&url, &agent_regex, &mut agents, args.quic_insecure).await
            } else {
                query_new_agents(&url, &agent_regex, &mut agents).await
            };
            match r {
                Ok(agents) => {
                    // tracing::debug!("query_new_agents success [{agents:?}]");
                    let iter = agents.into_iter().map(|x| x.name);
                    add_agents(&pool, &url, &args, quic_socks_hard_nat, iter).await?;
                    // for agent in agents {
                    //     let mut url = url.clone();

                    //     make_sub_url(&mut url, Some(&agent.name), args.secret.as_deref())?;
                    //     make_ws_scheme(&mut url)?;

                    //     tracing::info!("new agent [{}]", agent.name);
                    //     pool.invoker().invoke(AddAgent {
                    //         name: agent.name,
                    //         url: url.to_string(),
                    //     }).await??;
                    // }
                }
                Err(e) => {
                    tracing::debug!("query_new_agents failed [{e:?}]");
                }
            }
            tokio::time::sleep(Duration::from_millis(1_000)).await
        }
    }
}

fn contains_regex_chars(s: &str) -> bool {
    let regex_chars = r".^$*+?()[]{}\|";
    s.chars().any(|c| regex_chars.contains(c))
}

struct ClashContent<'a> {
    addrs: &'a Vec<SocketAddr>,
    server: &'a str,
    username: Option<&'a str>,
    password: Option<&'a str>,
}

impl<'a> fmt::Display for ClashContent<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // https://dreamacro.github.io/clash/configuration/introduction.html
        // port: 7890
        // socks-port: 7891
        // allow-lan: false
        // mode: Rule
        // log-level: info
        // external-controller: '127.0.0.1:9090'

        // let content: String = indoc::indoc!{ r#"
        // proxies:
        //   - name: "rtun1"
        //     type: socks5
        //     server: simon.home.rtcsdk.com
        //     port: 22080
        //     username: rtun
        //     password: alliwym
        //     tls: true
        // proxy-groups:
        //   - name: "rtunproxy"
        //     type: select
        //     proxies:
        //       - rtun1
        // rules:
        //   - MATCH,rtunproxy
        // "#}.into();

        writeln!(f, "proxies:")?;
        for (nn, addr) in self.addrs.iter().enumerate() {
            let number = nn + 1;
            writeln!(f, "  - name: \"rtun{number}\"")?;
            writeln!(f, "    type: socks5")?;
            writeln!(f, "    server: {}", self.server)?;
            writeln!(f, "    port: {}", addr.port())?;
            writeln!(f, "    tls: {}", true)?;

            if let Some(s) = &self.username {
                writeln!(f, "    username: {s}")?;
            }

            if let Some(s) = &self.password {
                writeln!(f, "    password: {s}")?;
            }
        }

        writeln!(f, "proxy-groups:")?;
        writeln!(f, "  - name: \"rtunproxy\"")?;
        writeln!(f, "    type: select")?;
        writeln!(f, "    proxies: ")?;
        for (nn, _addr) in self.addrs.iter().enumerate() {
            let number = nn + 1;
            writeln!(f, "      - rtun{number}")?;
        }

        Ok(())
    }
}

async fn add_agents<I>(
    pool: &QuicPool,
    url: &::url::Url,
    args: &CmdArgs,
    hard_nat: QuicSocksHardNatConfig,
    iter: I,
) -> Result<()>
where
    I: Iterator<Item = String>,
{
    for agent in iter {
        let mut url = url.clone();

        if url.scheme().eq_ignore_ascii_case("quic") {
            make_sub_quic_url(&mut url, Some(&agent), args.secret.as_deref())?;
        } else {
            make_sub_url(&mut url, Some(&agent), args.secret.as_deref())?;
            make_ws_scheme(&mut url)?;
        }

        tracing::info!("new agent [{}]", agent);
        pool.invoker()
            .invoke(AddAgent {
                name: agent,
                url: url.to_string(),
                quic_insecure: args.quic_insecure,
                hard_nat,
            })
            .await??;
    }
    Ok(())
}

fn make_sub_quic_url(
    url: &mut url::Url,
    agent_name: Option<&str>,
    secret: Option<&str>,
) -> Result<()> {
    if let Some(agent_name) = agent_name {
        url.query_pairs_mut().append_pair("agent", agent_name);
    }

    let token = token_gen(secret, Local::now().timestamp_millis() as u64)?;
    url.query_pairs_mut().append_pair("token", token.as_str());
    Ok(())
}

async fn query_new_agents_quic(
    url: &url::Url,
    agent_regex: &regex::Regex,
    exist: &mut HashMap<String, AgentInfo>,
    quic_insecure: bool,
) -> Result<Vec<AgentInfo>> {
    let mut agents = crate::quic_signal::query_sessions_with_opts(url, quic_insecure).await?;

    let mut pos = 0;
    for nn in 0..agents.len() {
        let name = &agents[nn].name;
        if !agent_regex.is_match(name) {
            continue;
        }

        if !exist.contains_key(name) {
            exist.insert(name.clone(), agents[nn].clone());
            if nn > pos {
                agents.swap(nn, pos);
            }
            pos += 1;
        }
    }

    agents.resize(
        pos,
        AgentInfo {
            name: "non-exist".into(),
            addr: "".into(),
            expire_at: 0,
            instance_id: None,
            ver: None,
        },
    );
    Ok(agents)
}

async fn kick_ws_socks(args: CmdArgs) -> Result<()> {
    if let Some(socks_ws) = &args.socks_ws {
        let listen_addr = if socks_ws == "0" || socks_ws == "1" || socks_ws == "true" {
            "0.0.0.0:13080"
        } else {
            socks_ws
        };

        let listener = TcpListener::bind(listen_addr)
            .await
            .with_context(|| format!("fail to bind address [{listen_addr}]"))?;
        tracing::info!("socks5(ws) listen on [{listen_addr}]");

        spawn_with_name("ws_sock", async move {
            let r = connect_loop(args, listener).await;
            tracing::debug!("finished {r:?}");
        });
    }

    Ok(())
}

async fn connect_loop(args: CmdArgs, listener: TcpListener) -> Result<()> {
    let shared = Arc::new(Shared {
        data: Mutex::new(SharedData { ctrl: None }),
    });

    if let Some(_socks_ws) = &args.socks_ws {
        let shared = shared.clone();
        spawn_with_name("local_sock", async move {
            let r = run_socks_via_ctrl(shared, listener).await;
            r
        });
    }

    loop {
        let (mut session, _name) = repeat_connect(&args).await;
        // let mut session = make_stream_session(stream).await?;

        // agent_pool.set_agent(name, session.ctrl_client().clone_invoker()).await?;

        if let Some(ptype) = args.mode {
            tracing::info!("kick p2p [{ptype:?}]");
            let invoker = session.ctrl_client().clone_invoker();
            let _r = kick_p2p(invoker, ptype).await?;
        }

        {
            shared.data.lock().ctrl = Some(session.ctrl_client().clone_invoker());
        }

        tracing::info!("wait for session completed");
        let r = session.wait_for_completed().await;
        tracing::info!("session finished {r:?}");

        tokio::time::sleep(Duration::from_millis(1000)).await;
    }
}

// async fn do_run1(args: CmdArgs, multi: MultiProgress) -> Result<()> {

//     let shared = Arc::new(Shared {
//         data: Mutex::new(SharedData {
//             ctrl: None,
//         }),
//     });

//     // let agent_pool = AgentPool::new();
//     let mut pools = Vec::new();

//     let (mut session, agent_name) = repeat_connect(&args).await;
//     // agent_pool.set_agent(agent_name.clone(), session.ctrl_client().clone_invoker()).await?;

//     {
//         let listen_addr = &args.listen;
//         let listen_addr: SocketAddr = listen_addr.parse().with_context(|| format!("invalid addr {listen_addr:?}"))?;

//         for nn in 0..3 {
//             let port = listen_addr.port()+nn;
//             let listen_addr = SocketAddr::new(listen_addr.ip(), port);
//             let listener = TcpListener::bind(listen_addr).await
//             .with_context(||format!("fail to bind address [{listen_addr}]"))?;
//             tracing::info!("socks5(quic) listen on [{listen_addr}]");

//             // let pool = agent_pool.clone();
//             let pool = AgentPool::new(multi.clone(), port.to_string());
//             pool.set_agent(agent_name.clone(), session.ctrl_client().clone_invoker()).await?;
//             pools.push(pool.clone());

//             spawn_with_name(format!("local_sock-{port}"), async move {
//                 let r = run_socks_via_quic(pool, listener).await;
//                 r
//             });
//         }

//     }

//     if let Some(socks_ws) = &args.socks_ws {
//         let listen_addr = if socks_ws == "0" || socks_ws == "1" || socks_ws == "true" {
//             "0.0.0.0:13080"
//         } else {
//             socks_ws
//         };
//         let listener = TcpListener::bind(listen_addr).await
//         .with_context(||format!("fail to bind address [{listen_addr}]"))?;
//         tracing::info!("socks5(ws) listen on [{listen_addr}]");

//         let shared = shared.clone();
//         spawn_with_name("local_sock", async move {
//             let r = run_socks_via_ctrl(shared, listener).await;
//             r
//         });
//     }

//     let r = session.wait_for_completed().await;
//     tracing::info!("session finished {r:?}");

//     loop {

//         let (mut session, name) = repeat_connect(&args).await;
//         // let mut session = make_stream_session(stream).await?;

//         // agent_pool.set_agent(name, session.ctrl_client().clone_invoker()).await?;
//         for pool in pools.iter() {
//             pool.set_agent(name.clone(), session.ctrl_client().clone_invoker()).await?;
//         }

//         if let Some(ptype) = args.mode {
//             let invoker = session.ctrl_client().clone_invoker();
//             let _r = kick_p2p(invoker, ptype).await?;
//         }

//         {
//             shared.data.lock().ctrl = Some(session.ctrl_client().clone_invoker());
//         }

//         let r = session.wait_for_completed().await;
//         tracing::info!("session finished {r:?}");

//         tokio::time::sleep(Duration::from_millis(1000)).await;
//     }

// }

async fn repeat_connect(
    args: &CmdArgs,
) -> (StreamSession<impl PacketSink, impl PacketSource>, String) {
    let mut last_success = true;

    loop {
        let r = try_connect(&args).await;

        match r {
            Ok(r) => {
                return r;
            }
            Err(e) => {
                if last_success {
                    last_success = false;
                    tracing::warn!("connect failed [{e:?}]");
                    tracing::info!("try reconnecting...");
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(1000)).await;
    }
}

// type CtrlSession = self::ctrl::Ctrl;
// type SessionInvoker = self::ctrl::SessionInvoker;
// mod ctrl {
//     use futures::stream::{SplitSink, SplitStream};
//     use rtun::{switch::{session_stream::StreamSession, switch_pair::SwitchPairInvoker}, ws::client::{WsSink, WsSource}};
//     use tokio::net::TcpStream;
//     use tokio_tungstenite::{WebSocketStream, MaybeTlsStream, tungstenite::Message};

//     type Source = WsSource<SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>>;
//     type Sink = WsSink<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>;
//     pub type Ctrl = StreamSession<Sink, Source>;

//     // pub type Invoker = CtrlInvoker<Entity<Entity<impl PacketSource>>>;
//     pub type SessionInvoker = SwitchPairInvoker<Source>;

// }

struct Shared<H: CtrlHandler> {
    data: Mutex<SharedData<H>>,
}

struct SharedData<H: CtrlHandler> {
    ctrl: Option<CtrlInvoker<H>>,
}

async fn try_connect(
    args: &CmdArgs,
) -> Result<(StreamSession<impl PacketSink, impl PacketSource>, String)> {
    let url = client_select_url(&args.url, args.agent.as_deref(), args.secret.as_deref()).await?;
    let url_str = url.as_str();

    let (stream, _r) = ws_connect_to(url_str)
        .await
        .with_context(|| format!("connect to agent failed"))?;

    let agent_name = get_agent_from_url(&url).with_context(|| "can't get agent name")?;
    tracing::info!("connected to agent {agent_name:?}");

    // let uid = gen_huid();
    // let mut switch = make_switch_pair(uid, stream.split()).await?;
    let session = make_stream_session(stream.split(), false).await?;

    Ok((session, agent_name.into_owned()))
}

async fn run_socks_via_quic(pool: QuicPoolInvoker, listener: TcpListener) -> Result<()> {
    loop {
        let (mut stream, peer_addr) = listener
            .accept()
            .await
            .with_context(|| "accept tcp failed")?;

        tracing::trace!("[{peer_addr}] client connected");

        let pool = pool.clone();
        tokio::spawn(async move {
            let r = async move {
                let (mut wr1, mut rd1) = pool
                    .invoke(GetCh)
                    .await??
                    .with_context(|| "no available ch")?;
                let (mut rd2, mut wr2) = stream.split();
                tokio::select! {
                    r = tokio::io::copy(&mut rd2, &mut wr1) => {r?;},
                    r = tokio::io::copy(&mut rd1, &mut wr2) => {r?;},
                }
                Result::<()>::Ok(())
            }
            .await;
            tracing::trace!("[{peer_addr}] client finished with {r:?}");
            r
        });
    }
}

async fn run_tls_socks_via_quic(
    pool: QuicPoolInvoker,
    listener: TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    auth: Arc<Socks5AuthConfig>,
) -> Result<()> {
    loop {
        let (stream, peer_addr) = listener
            .accept()
            .await
            .with_context(|| "accept tcp failed")?;

        tracing::trace!("[{peer_addr}] client connected");

        let pool = pool.clone();
        let acceptor = acceptor.clone();
        let auth = auth.clone();
        tokio::spawn(async move {
            let r = async move {
                let stream = acceptor.accept(stream).await?;
                let (mut rd2, mut wr2) = tokio::io::split(stream);

                let (mut wr1, mut rd1) = pool
                    .invoke(GetCh)
                    .await??
                    .with_context(|| "no available ch")?;

                run_socks5_conn_bridge(&mut rd2, &mut wr2, &mut rd1, &mut wr1, &auth).await
            }
            .await;
            tracing::trace!("[{peer_addr}] client finished with {r:?}");
            r
        });
    }
}

// async fn run_socks_via_quic1<H: CtrlHandler>( pool: AgentPool<H>, listener: TcpListener ) -> Result<()> {

//     loop {
//         let (mut stream, peer_addr)  = listener.accept().await.with_context(||"accept tcp failed")?;

//         tracing::trace!("[{peer_addr}] client connected");

//         let pool = pool.clone();
//         tokio::spawn(async move {
//             let r = async move {
//                 let (mut wr1, mut rd1) = pool.get_ch().await?;
//                 let (mut rd2, mut wr2) = stream.split();
//                 tokio::select! {
//                     r = tokio::io::copy(&mut rd2, &mut wr1) => {r?;},
//                     r = tokio::io::copy(&mut rd1, &mut wr2) => {r?;},
//                 }
//                 Result::<()>::Ok(())
//             }.await;
//             tracing::trace!("[{peer_addr}] client finished with {r:?}");
//             r
//         });

//     }
// }

async fn run_socks_via_ctrl<H: CtrlHandler>(
    shared: Arc<Shared<H>>,
    listener: TcpListener,
) -> Result<()> {
    let mut next_ch_id = NextChId::default();
    loop {
        let (stream, peer_addr) = listener
            .accept()
            .await
            .with_context(|| "accept tcp failed")?;

        tracing::trace!("[{peer_addr}] client connected");

        let r = { shared.data.lock().ctrl.clone() };

        match r {
            Some(ctrl) => {
                let ch_id = next_ch_id.next_ch_id();

                tokio::spawn(async move {
                    let r = handle_client(ctrl, ch_id, stream, peer_addr).await;
                    tracing::trace!("[{peer_addr}] client finished with {r:?}");
                    r
                });
            }
            None => {}
        }
    }
}

async fn handle_client<H: CtrlHandler>(
    ctrl: CtrlInvoker<H>,
    ch_id: ChId,
    mut stream: TcpStream,
    peer_addr: SocketAddr,
) -> Result<()> {
    let (ch_tx, ch_rx) = ChPair::new(ch_id).split();

    let open_args = OpenSocksArgs {
        ch_id: Some(ch_id.0),
        peer_addr: peer_addr.to_string().into(),
        ..Default::default()
    };

    let ch_tx = {
        let r = ctrl
            .open_socks(ch_tx, open_args)
            .await
            .with_context(|| "open socks ch failed");
        match r {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("{e:?}");
                return Err(e);
            }
        }
    };
    tracing::debug!("opened socks {} -> {:?}", peer_addr, ch_tx.ch_id());

    // let mut ch_rx = ch_rx;
    // let r = copy::copy_loop(&mut stream, &ch_tx, &mut ch_rx).await;

    let mut ch_stream = ChStream::new2(ch_tx, ch_rx);
    // let r = copy_stream_bidir(&mut stream, &mut ch_stream).await;
    let r = tokio::io::copy_bidirectional(&mut stream, &mut ch_stream).await;

    let _r = ctrl.close_channel(ch_id).await;
    r?;
    Ok(())
}

// mod copy {
//     use anyhow::{Result, bail, Context, anyhow};
//     use bytes::BytesMut;
//     use rtun::channel::{ChSender, ChReceiver};
//     use tokio::{net::TcpStream, io::{AsyncWriteExt, AsyncReadExt}};

//     pub async fn copy_loop(stream: &mut TcpStream, ch_tx: &ChSender, ch_rx: &mut ChReceiver) -> Result<()> {
//         let mut buf = BytesMut::new();
//         loop {
//             tokio::select! {
//                 r = ch_rx.recv_packet() => {
//                     let packet = r.with_context(||"recv but channel closed")?;

//                     stream.write_all(&packet.payload[..]).await
//                     .with_context(||"write but stream closed")?;
//                 },
//                 r = stream.read_buf(&mut buf) => {
//                     let n = r.with_context(||"recv but stream closed")?;
//                     if n == 0 {
//                         bail!("socket recv-zero closed")
//                     }
//                     let payload = buf.split().freeze();
//                     ch_tx.send_data(payload).await
//                     .map_err(|_e|anyhow!("send but channel closed"))?;
//                 }
//             }
//         }
//     }
// }

struct LogWriter {
    multi: MultiProgress,
    // buf: BytesMut,
}

impl LogWriter {
    pub fn new(multi: MultiProgress) -> Self {
        Self {
            multi,
            // buf: BytesMut::new(),
        }
    }
}

impl io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        for line in LineIter(Some(buf)) {
            if let Ok(s) = std::str::from_utf8(line) {
                self.multi.println(s)?;
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub struct LineIter<'a>(pub Option<&'a [u8]>);

impl<'a> Iterator for LineIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        let data = self.0.take();
        match data {
            Some(data) => {
                let r = data.iter().position(|x| *x == b'\r' || *x == b'\n');
                match r {
                    Some(n) => {
                        let (line, data) = data.split_at(n);

                        let r = data.iter().position(|x| *x != b'\r' && *x != b'\n');
                        match r {
                            Some(n) => {
                                let (_r, data) = data.split_at(n);
                                self.0 = Some(data);
                            }
                            None => {
                                // self.0 = Some(data);
                            }
                        }
                        Some(line)
                    }
                    None => Some(data),
                }
            }
            None => None,
        }
    }
}

#[test]
fn test_line_iter() {
    {
        let list = vec!["abc", "abc\r", "abc\n", "abc\r\n", "abc\n\r"];

        for data in list {
            let mut iter = LineIter(Some(data.as_bytes()));
            assert_eq!(iter.next(), Some("abc".as_bytes()));
            assert_eq!(iter.next(), None);
        }
    }

    {
        let list = vec![
            "abc\rdef",
            "abc\ndef",
            "abc\r\ndef",
            "abc\n\rdef",
            "abc\rdef\r",
            "abc\rdef\n",
            "abc\rdef\r\n",
            "abc\rdef\n\r",
        ];

        for data in list {
            let mut iter = LineIter(Some(data.as_bytes()));
            assert_eq!(iter.next(), Some("abc".as_bytes()));
            assert_eq!(iter.next(), Some("def".as_bytes()));
            assert_eq!(iter.next(), None);
        }
    }
}

#[derive(Parser, Debug, Clone)]
#[clap(name = "socks", author, about, version)]
pub struct CmdArgs {
    #[clap(help = "eg: http://127.0.0.1:8080")]
    url: String,

    #[clap(short = 'a', long = "agent", long_help = "agent name")]
    agent: Option<String>,

    #[clap(
        short = 'l',
        long = "listen",
        long_help = "listen address",
        default_value = "0.0.0.0:12080"
    )]
    listen: String,

    #[clap(long = "secret", long_help = "authentication secret")]
    secret: Option<String>,

    #[clap(
        long = "quic-insecure",
        long_help = "skip quic tls certificate verification (quic:// only)"
    )]
    quic_insecure: bool,

    #[clap(long = "mode", long_help = "tunnel mode")]
    mode: Option<u32>,

    #[clap(
        long = "p2p-hardnat",
        value_enum,
        default_value_t = SocksHardNatModeCli::Off,
        long_help = "hard nat punching strategy for QUIC socks p2p (placeholder only for now)"
    )]
    p2p_hardnat: SocksHardNatModeCli,

    #[clap(
        long = "p2p-hardnat-role",
        value_enum,
        default_value_t = SocksHardNatRoleCli::Auto,
        long_help = "hard nat punching role hint (placeholder only for now)"
    )]
    p2p_hardnat_role: SocksHardNatRoleCli,

    #[clap(
        long = "p2p-hardnat-socket-count",
        default_value_t = DEFAULT_P2P_HARDNAT_SOCKET_COUNT,
        long_help = "hard nat socket count (placeholder only for now)"
    )]
    p2p_hardnat_socket_count: u32,

    #[clap(
        long = "p2p-hardnat-scan-count",
        default_value_t = DEFAULT_P2P_HARDNAT_SCAN_COUNT,
        long_help = "hard nat scan/random-port count (placeholder only for now)"
    )]
    p2p_hardnat_scan_count: u32,

    #[clap(
        long = "p2p-hardnat-interval",
        default_value_t = DEFAULT_P2P_HARDNAT_INTERVAL_MS,
        long_help = "hard nat probe interval in ms (placeholder only for now)"
    )]
    p2p_hardnat_interval: u64,

    #[clap(
        long = "p2p-hardnat-batch-interval",
        default_value_t = DEFAULT_P2P_HARDNAT_BATCH_INTERVAL_MS,
        long_help = "hard nat batch interval in ms (placeholder only for now)"
    )]
    p2p_hardnat_batch_interval: u64,

    #[clap(
        long = "p2p-hardnat-ttl",
        long_help = "hard nat ttl hint (placeholder only for now)"
    )]
    p2p_hardnat_ttl: Option<u32>,

    #[clap(
        long = "p2p-hardnat-no-ttl",
        long_help = "disable hard nat ttl pre-send optimization (placeholder only for now)"
    )]
    p2p_hardnat_no_ttl: bool,

    #[clap(long = "socks-ws", long_help = "listen addr for socks via ws")]
    socks_ws: Option<String>,

    #[clap(long = "tls-cert", long_help = "socks-over-tls cert file")]
    tls_cert: Option<String>,

    #[clap(long = "tls-key", long_help = "socks-over-tls key file")]
    tls_key: Option<String>,

    #[clap(
        long = "tls-user",
        long_help = "socks-over-tls user:password",
        // num_args = 0..,
    )]
    tls_user: Vec<String>,

    #[clap(long = "tls-listen", long_help = "tls listen address")]
    tls_listen: Option<String>,

    #[clap(
        long = "clash-listen",
        long_help = "http listen address for clash subscription"
    )]
    clash_listen: Option<String>,
}

mod tls_util {

    use anyhow::{bail, Result};
    use rustls::{server::ClientCertVerifier, Certificate, PrivateKey, ServerConfig};
    use std::{io, sync::Arc};
    use tokio_rustls::TlsAcceptor;
    use webpki::EndEntityCert;

    pub fn acceptor_from_cfg(config: Arc<ServerConfig>) -> Result<TlsAcceptor> {
        let acceptor = TlsAcceptor::from(config);
        Ok(acceptor)
    }

    // pub async fn acceptor_from_cert(key_file: &str, cert_file: &str, client_cert_verifier: Option<Arc<dyn ClientCertVerifier>>) -> Result<TlsAcceptor> {

    //     let config = try_load_server_certs(key_file, cert_file, client_cert_verifier).await?;
    //     let acceptor = TlsAcceptor::from(config);
    //     Ok(acceptor)
    // }

    // pub async fn try_load_server_certs(
    //     key_file: &str,
    //     cert_file: &str,
    //     client_cert_verifier: Option<Arc<dyn ClientCertVerifier>>,
    // ) -> io::Result<Arc<ServerConfig>> {
    //     let raw_cert = tokio::fs::read(cert_file).await?;
    //     let raw_key = tokio::fs::read(key_file).await?;

    //     raw_cert_to_server_cfg(raw_key, raw_cert, client_cert_verifier)
    // }

    pub fn raw_cert_to_server_cfg(
        raw_key: Vec<u8>,
        raw_cert: Vec<u8>,
        client_cert_verifier: Option<Arc<dyn ClientCertVerifier>>,
    ) -> io::Result<Arc<ServerConfig>> {
        use rustls_pemfile::Item;

        let certs = rustls_pemfile::certs(&mut raw_cert.as_ref())?;
        let key = match rustls_pemfile::read_one(&mut raw_key.as_ref())? {
            Some(Item::RSAKey(key)) | Some(Item::PKCS8Key(key)) | Some(Item::ECKey(key)) => key,
            _ => return Err(io_other("private key format not supported")),
        };

        let certs: Vec<Certificate> = certs.into_iter().map(Certificate).collect();
        let key = PrivateKey(key);

        // print_certs(certs.iter()).unwrap();

        let builder = ServerConfig::builder().with_safe_defaults();

        let builder = match client_cert_verifier {
            Some(verifier) => {
                // tracing::debug!("with_client_cert_verifier");
                builder.with_client_cert_verifier(verifier)
            }
            None => builder.with_no_client_auth(),
        };

        let mut config = builder.with_single_cert(certs, key).map_err(io_other)?;

        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

        Ok(Arc::new(config))
    }

    type BoxError = Box<dyn std::error::Error + Send + Sync>;
    fn io_other<E: Into<BoxError>>(error: E) -> io::Error {
        io::Error::new(io::ErrorKind::Other, error)
    }

    // pub async fn client_cert_verifier(ca_file: &str) -> Result<Arc<dyn ClientCertVerifier>> {
    //     let root_store = load_root(ca_file).await?;
    //     let verifier = AllowAnyAuthenticatedClient::new(root_store).boxed();
    //     Ok(verifier)
    // }

    // async fn load_root(cert_file: &str) -> Result<RootCertStore> {
    //     let data = tokio::fs::read(cert_file).await?;
    //     let certs = rustls_pemfile::certs(&mut data.as_ref())?;
    //     let certs: Vec<Certificate> = certs.into_iter().map(Certificate).collect();

    //     let mut root_store = RootCertStore::empty();
    //     for cert in certs {
    //         root_store.add(&cert)?;
    //     }

    //     Ok(root_store)
    // }

    pub async fn try_load_cert_dns_name(cert_file: &str) -> Result<String> {
        let cert = tokio::fs::read(cert_file).await?;
        let certs = rustls_pemfile::certs(&mut cert.as_ref())?;

        for cert in certs {
            let end_entity_cert = EndEntityCert::try_from(&cert[..])?;
            for name in end_entity_cert.dns_names()? {
                let name: &str = name.into();
                if !name.is_empty() {
                    return Ok(name.into());
                }
            }
        }
        bail!("no cert dns name found")
    }

    // pub fn print_certs<'a, I>(certs: I) -> Result<()>
    // where
    //     I: Iterator<Item = &'a Certificate>,
    // {
    //     for cert in certs {
    //         let end_entity_cert = webpki::EndEntityCert::try_from(&cert.0[..])?;
    //         tracing::debug!("--");
    //         for name in end_entity_cert.dns_names()? {
    //             let name: &str = name.into();
    //             tracing::debug!("  cert dns name [{name}]");
    //         }
    //     }
    //     Ok(())
    // }
}

mod clash_rest {
    use anyhow::{Context, Result};
    use axum::{response::IntoResponse, routing::get, Extension, Router};
    use axum_server::{tls_rustls::RustlsConfig, Handle};
    use rtun::async_rt::spawn_with_name;
    use rustls::ServerConfig;
    use std::{io, net::SocketAddr, sync::Arc};
    use tokio::{net::TcpListener, task::JoinHandle};

    pub async fn server_for_clash(
        listen_addr: SocketAddr,
        path: &str,
        tls_cfg: Option<Arc<ServerConfig>>,
        content: String,
    ) -> Result<ClashSubServer> {
        // let content = "abc".to_string();
        let shared = Arc::new(Shared { content });

        async fn get_clash(Extension(shared): Extension<Arc<Shared>>) -> impl IntoResponse {
            shared.content.clone()
        }

        let router = Router::new().route(path, get(get_clash));
        let router = router.layer(Extension(shared));

        let tls_cfg = tls_cfg.map(RustlsConfig::from_config);
        // let is_https = tls_cfg.is_some();

        let listener = TcpListener::bind(listen_addr)
            .await
            .with_context(|| format!("fail to bind [{}]", listen_addr))?
            .into_std()
            .with_context(|| "tcp listener into std failed")?;

        let server_handle = Handle::new();

        let task_name = format!("server");
        let task = spawn_with_name(task_name, async move {
            match tls_cfg {
                Some(tls_cfg) => {
                    axum_server::from_tcp_rustls(listener, tls_cfg)
                        // axum_server::bind_rustls(addr, tls_cfg)
                        .handle(server_handle)
                        .serve(router.into_make_service_with_connect_info::<SocketAddr>())
                        .await
                }
                None => {
                    axum_server::from_tcp(listener)
                        // axum_server::bind(addr)
                        .handle(server_handle)
                        .serve(router.into_make_service_with_connect_info::<SocketAddr>())
                        .await
                }
            }
        });

        Ok(ClashSubServer { _task: task })
    }

    pub struct ClashSubServer {
        _task: JoinHandle<io::Result<()>>,
    }

    struct Shared {
        content: String,
    }
}

mod regex_text {
    #[test]
    fn test_regex() {
        // let rgx = regex::Regex::new("rtun-1").unwrap();
        // assert!(rgx.is_match("rtun-1"), "{rgx}") ;
        // println!("regular expr [{rgx}] captures_len [{}]", rgx.captures_len());

        let rgx = regex::Regex::new("rtun-.*").unwrap();
        assert!(rgx.is_match("rtun-"), "{rgx}");
        assert!(rgx.is_match("rtun-1"), "{rgx}");
        assert!(rgx.is_match("rtun-2"), "{rgx}");
        assert!(!rgx.is_match("rtun1"), "{rgx}");
        assert!(!rgx.is_match("home_mini"), "{rgx}");

        let rgx = regex::Regex::new(".*").unwrap();
        assert!(rgx.is_match("rtun-"), "{rgx}");
        assert!(rgx.is_match("rtun-1"), "{rgx}");
        assert!(rgx.is_match("rtun-2"), "{rgx}");
        assert!(rgx.is_match("rtun1"), "{rgx}");
        assert!(rgx.is_match("home_mini"), "{rgx}");
    }
}

#[cfg(test)]
mod hard_nat_cli_tests {
    use super::*;
    use rtun::p2p::hard_nat::{
        HARD_NAT_MAX_SCAN_COUNT, HARD_NAT_MAX_SOCKET_COUNT, HARD_NAT_MAX_TTL,
    };

    #[test]
    fn quic_socks_hard_nat_cli_defaults_to_off() {
        let args = CmdArgs::parse_from(["socks", "http://127.0.0.1:8080"]);
        let cfg = resolve_quic_socks_hard_nat_config(&args).unwrap();

        assert_eq!(cfg.mode, SocksHardNatModeCli::Off);
        assert_eq!(cfg.role, SocksHardNatRoleCli::Auto);
        assert_eq!(cfg.socket_count, DEFAULT_P2P_HARDNAT_SOCKET_COUNT);
        assert_eq!(cfg.scan_count, DEFAULT_P2P_HARDNAT_SCAN_COUNT);
        assert_eq!(cfg.interval_ms, DEFAULT_P2P_HARDNAT_INTERVAL_MS as u32);
        assert_eq!(
            cfg.batch_interval_ms,
            DEFAULT_P2P_HARDNAT_BATCH_INTERVAL_MS as u32
        );
        assert_eq!(cfg.ttl, None);
        assert!(!cfg.no_ttl);
    }

    #[test]
    fn quic_socks_hard_nat_cli_parses_fallback_role_ttl_and_no_ttl() {
        let args = CmdArgs::parse_from([
            "socks",
            "quic://rtun.example:9888",
            "--p2p-hardnat",
            "fallback",
            "--p2p-hardnat-role",
            "nat4",
            "--p2p-hardnat-socket-count",
            "17",
            "--p2p-hardnat-scan-count",
            "33",
            "--p2p-hardnat-interval",
            "1500",
            "--p2p-hardnat-batch-interval",
            "4500",
            "--p2p-hardnat-ttl",
            "9",
            "--p2p-hardnat-no-ttl",
        ]);
        let cfg = resolve_quic_socks_hard_nat_config(&args).unwrap();

        assert_eq!(cfg.mode, SocksHardNatModeCli::Fallback);
        assert_eq!(cfg.role, SocksHardNatRoleCli::Nat4);
        assert_eq!(cfg.socket_count, 17);
        assert_eq!(cfg.scan_count, 33);
        assert_eq!(cfg.interval_ms, 1500);
        assert_eq!(cfg.batch_interval_ms, 4500);
        assert_eq!(cfg.ttl, Some(9));
        assert!(cfg.no_ttl);
    }

    #[test]
    fn quic_socks_hard_nat_cli_rejects_values_above_limits() {
        let socket_count = (HARD_NAT_MAX_SOCKET_COUNT + 1).to_string();
        let args = CmdArgs::parse_from([
            "socks",
            "quic://rtun.example:9888",
            "--p2p-hardnat-socket-count",
            socket_count.as_str(),
        ]);
        let err = resolve_quic_socks_hard_nat_config(&args)
            .unwrap_err()
            .to_string();
        assert!(err.contains("socket count too large"));

        let scan_count = (HARD_NAT_MAX_SCAN_COUNT + 1).to_string();
        let args = CmdArgs::parse_from([
            "socks",
            "quic://rtun.example:9888",
            "--p2p-hardnat-scan-count",
            scan_count.as_str(),
        ]);
        let err = resolve_quic_socks_hard_nat_config(&args)
            .unwrap_err()
            .to_string();
        assert!(err.contains("scan count too large"));

        let ttl = (HARD_NAT_MAX_TTL + 1).to_string();
        let args = CmdArgs::parse_from([
            "socks",
            "quic://rtun.example:9888",
            "--p2p-hardnat-ttl",
            ttl.as_str(),
        ]);
        let err = resolve_quic_socks_hard_nat_config(&args)
            .unwrap_err()
            .to_string();
        assert!(err.contains("ttl too large"));
    }
}
