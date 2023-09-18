use std::time::Duration;

use anyhow::{Result, Context};
use rtun::{switch::invoker_ctrl::{CtrlHandler, CtrlInvoker}, ice::{ice_peer::{IcePeer, IceConfig}, ice_quic::{UpgradeToQuic, QuicIceCert}, throughput::run_throughput, webrtc_ice_peer::{WebrtcIcePeer, WebrtcIceConfig, DtlsIceArgs}}, proto::{ThroughputArgs,  open_p2presponse::Open_p2p_rsp, P2PArgs, p2pargs::P2p_args, QuicThroughputArgs, P2PQuicArgs, WebrtcThroughputArgs, P2PDtlsArgs}, async_rt::spawn_with_name};
use tokio::time::timeout;
use tokio_util::compat::{FuturesAsyncReadCompatExt, FuturesAsyncWriteCompatExt};

pub async fn kick_p2p<H: CtrlHandler>(invoker: CtrlInvoker<H>, ptype: u32) -> Result<()> {
    tracing::debug!("kick p2p type {ptype}");
    
    if ptype == 11 {
        return kick_quic_throughput(invoker, ptype).await;
    } else if ptype == 12 {
        return kick_p2p_webrtc(invoker, ptype).await;
    } else {
        // bail!("unknown p2p type {ptype}")
        return Ok(())
    }
}

async fn kick_quic_throughput<H: CtrlHandler>(invoker: CtrlInvoker<H>, peer_type: u32) -> Result<()> {
    tracing::debug!("kick_quic_throughput");
    let timeout_duration = Duration::from_secs(10);

    let ice_servers = vec![
        "stun:stun1.l.google.com:19302".into(),
        "stun:stun2.l.google.com:19302".into(),
        "stun:stun.qq.com:3478".into(),
    ];

    let mut peer = IcePeer::with_config(IceConfig {
        servers: ice_servers.clone(),
        // disable_dtls: true,
        ..Default::default()
    });

    tracing::debug!("kick gather candidate");
    let local_args = peer.client_gather().await?;
    tracing::debug!("local args {local_args:?}");
    
    let cert = QuicIceCert::try_new()?;
    // let (peer, nat) = PunchPeer::bind_and_detect("0.0.0.0:0").await?;

    // let local_ufrag = peer.local_ufrag().to_string();

    // let nat_type = nat.nat_type();
    // if nat_type != Some(NatType::Cone) {
    //     tracing::warn!("nat type {nat_type:?}");
    // } else {
    //     tracing::debug!("nat type {nat_type:?}");
    // }
    // let mapped = nat.into_mapped().with_context(||"empty mapped address")?;

    let thrp_args = ThroughputArgs {
        send_buf_size: 32*1024,
        recv_buf_size: 32*1024,
        peer_type,
        ..Default::default()
    };

    // let invoker = session.ctrl_client().clone_invoker();
    let rsp = invoker.open_p2p(P2PArgs {
        p2p_args: Some(P2p_args::QuicThrput(QuicThroughputArgs {
            base: Some(P2PQuicArgs {
                ice: Some(local_args.into()).into(),
                cert_der: cert.to_bytes()?.into(),
                ..Default::default()
            }.into()).into(),
            throughput: Some(thrp_args.clone()).into(),
            ..Default::default()
        })),
        // args: Some(local_args.into()).into(),
        // tun_args: Some(Tun_args::Throughput(thrp_args.clone())),
        ..Default::default()
    }).await?;

    let rsp = rsp.open_p2p_rsp.with_context(||"no open_p2p_rsp")?;
    match rsp {
        Open_p2p_rsp::Args(mut remote_args) => {
            // tracing::debug!("raw args {remote_args}");
            let mut remote_args = remote_args.take_quic_thrput();
            let mut base = remote_args.take_base();
            let remote_args = base.take_ice().into();

            let local_cert = QuicIceCert::try_new()?;
            let remote_cert = base.take_cert_der();

            tracing::debug!("remote args {remote_args:?}");
            // // let mut tun = launch_tun_peer(peer, args.ufrag.into(), args.addr.parse()?, false);
            // peer.into_dial_and_chat(remote_args).await?;
            
            spawn_with_name("p2p-throughput", async move {
                tracing::debug!("starting");

                let r = async move {
                    let conn = peer.dial(remote_args).await?
                    .upgrade_to_quic(&local_cert, remote_cert).await?;
                    let (wr, rd) = conn.open_bi().await?;
                    let r = timeout(
                        timeout_duration, 
                        run_throughput(rd, wr, thrp_args)
                    ).await;
                    conn.close(0_u32.into(), "normal".as_bytes());
                    r?
                }.await;
                
                tracing::debug!("finished {r:?}");
            });

        },
        Open_p2p_rsp::Status(s) => {
            tracing::warn!("open p2p but {s:?}");
        },
        _ => {
            tracing::warn!("unknown Open_p2p_rsp {rsp:?}");
        }
    }     
    Ok(())
}

async fn kick_p2p_webrtc<H: CtrlHandler>(invoker: CtrlInvoker<H>, peer_type: u32) -> Result<()> {
    tracing::debug!("kick_p2p_webrtc");
    let timeout_duration = Duration::from_secs(10);

    let ice_servers = vec![
        "stun:stun1.l.google.com:19302".into(),
        "stun:stun2.l.google.com:19302".into(),
        "stun:stun.qq.com:3478".into(),
    ];

    let mut peer = WebrtcIcePeer::with_config(WebrtcIceConfig {
        servers: ice_servers.clone(),
        // disable_dtls: true,
        ..Default::default()
    });

    tracing::debug!("kick gather candidate");
    let local_args = peer.gather_until_done().await?;
    tracing::debug!("local args {local_args:?}");
    
    // let (peer, nat) = PunchPeer::bind_and_detect("0.0.0.0:0").await?;

    // let local_ufrag = peer.local_ufrag().to_string();

    // let nat_type = nat.nat_type();
    // if nat_type != Some(NatType::Cone) {
    //     tracing::warn!("nat type {nat_type:?}");
    // } else {
    //     tracing::debug!("nat type {nat_type:?}");
    // }
    // let mapped = nat.into_mapped().with_context(||"empty mapped address")?;

    let thrp_args = ThroughputArgs {
        send_buf_size: 32*1024,
        recv_buf_size: 32*1024,
        peer_type,
        ..Default::default()
    };

    let rsp = invoker.open_p2p(P2PArgs {
        p2p_args: Some(P2p_args::WebrtcThrput(WebrtcThroughputArgs {
            base: Some(P2PDtlsArgs {
                ice: Some(local_args.ice.into()).into(),
                cert_fingerprint: local_args.cert_fingerprint.map(|x|x.into()),
                ..Default::default()
            }.into()).into(),
            throughput: Some(thrp_args.clone()).into(),
            ..Default::default()
        })),
        // args: Some(local_args.into()).into(),
        // tun_args: Some(Tun_args::Throughput(thrp_args.clone())),
        ..Default::default()
    }).await?;

    // // let invoker = session.ctrl_client().clone_invoker();
    // let rsp = invoker.open_p2p(OpenP2PArgs {
    //     args: Some(local_args.into()).into(),
    //     tun_args: Some(Tun_args::Throughput(thrp_args.clone())),
    //     ..Default::default()
    // }).await?;

    let rsp = rsp.open_p2p_rsp.with_context(||"no open_p2p_rsp")?;
    match rsp {
        Open_p2p_rsp::Args(mut remote_args) => {
            let mut remote_args = remote_args.take_webrtc_thrput();
            let mut dtls = remote_args.take_base();
            // let thrput = remote_args.take_throughput();
            let remote_args = DtlsIceArgs {
                ice: dtls.take_ice().into(),
                cert_fingerprint: Some(dtls.cert_fingerprint().into()),
            };

            tracing::debug!("remote args {remote_args:?}");
            // // let mut tun = launch_tun_peer(peer, args.ufrag.into(), args.addr.parse()?, false);
            // peer.into_dial_and_chat(remote_args).await?;
            
            spawn_with_name("p2p-throughput", async move {
                tracing::debug!("starting");

                let r = async move {
                    let conn = peer.kick_and_ugrade_to_kcp(remote_args, true).await?;
                    let (rd, wr) = conn.split();
                    // run_throughput(rd, wr, thrp_args).await
                    timeout(
                        timeout_duration, 
                        run_throughput(rd.compat(), wr.compat_write(), thrp_args)
                    ).await?
                }.await;
                
                tracing::debug!("finished {r:?}");
            });

        },
        Open_p2p_rsp::Status(s) => {
            tracing::warn!("open p2p but {s:?}");
        },
        _ => {
            tracing::warn!("unknown Open_p2p_rsp {rsp:?}");
        }
    }     
    Ok(())
}

