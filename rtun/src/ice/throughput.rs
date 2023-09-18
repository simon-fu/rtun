use std::{io::{ErrorKind, self}, time::Duration};

use bytes::{BufMut, Buf};
use tokio::io::{AsyncRead, AsyncWrite, AsyncReadExt, AsyncWriteExt};
use anyhow::Result;
use tokio::time::Instant;
use crate::proto::ThroughputArgs;


pub async fn run_throughput<R, W>(mut rd: R, mut wr: W, args: ThroughputArgs) -> Result<()> 
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut recv_buf = vec![0_u8; args.recv_buf_size as usize];

    let num_words = args.send_buf_size/4;
    let mut verifier = Verify::new(num_words);

    let send_buf = {
        let mut buf = vec![0_u8; args.send_buf_size as usize];
        let mut data = &mut buf[..];
        for n in 0..num_words {
            data.put_u32(n);
        }
        buf
    };
    
    let mut wdata = &send_buf[..];
    
    let mut total_sent_bytes = 0_u64;
    let mut total_recv_bytes = 0_u64;

    let mut last_sent_bytes = 0_u64;
    let mut last_recv_bytes = 0_u64;

    let mut last_update = Instant::now();

    let mut interval = tokio::time::interval(Duration::from_millis(1500));
    interval.tick().await;

    let mut recv_pos = 0;
    const MAX_SEND_BYTES: u64 = 99999999_010_000_000;

    loop {
        tokio::select! {
            r = rd.read(&mut recv_buf[recv_pos..]) => {
                let n = r?;
                if n == 0 {
                    break;
                }
                total_recv_bytes += n as u64;

                let len = recv_pos+n;
                let remaining = verifier.verfiy(&recv_buf[..len]);
                (&mut recv_buf[..]).copy_within(len-remaining .. len, 0);
                recv_pos = remaining;
            },
            r = wr.write(wdata), if total_sent_bytes < MAX_SEND_BYTES => {
                let n = r?;
                if n == 0 {
                    return Err(io::Error::from(ErrorKind::WriteZero).into())
                }
                total_sent_bytes += n as u64;

                wdata.advance(n);
                if wdata.len() == 0 {
                    wdata = &send_buf[..];
                }
            },
            _r = interval.tick() => {
                let elapsed = Instant::now() - last_update;
                last_update = Instant::now();
                if elapsed > Duration::ZERO {
                    
                    let sent_bytes = total_sent_bytes - last_sent_bytes;
                    let recv_bytes = total_recv_bytes - last_recv_bytes;

                    let sent_kps = sent_bytes * 8 * 1000 / (elapsed.as_millis() as u64) / 1000;
                    let recv_kps = recv_bytes * 8 * 1000 / (elapsed.as_millis() as u64) / 1000;

                    tracing::debug!("sent {sent_kps} Kb/s, recv {recv_kps} Kb/s, total sent {total_sent_bytes}, recv {total_recv_bytes} ");
                }
                last_sent_bytes = total_sent_bytes;
                last_recv_bytes = total_recv_bytes;
            }
        }
    }
    Ok(())
}

struct Verify {
    num_words: u32,
    next: u32,
}

impl Verify {
    fn new(num_words: u32) -> Self {
        Self { num_words, next: 0 }
    }

    /// return remains
    fn verfiy(&mut self, mut data: &[u8]) -> usize {
        while data.len() >= 4 {
            let w = data.get_u32();
            assert_eq!(self.next, w);
            self.next += 1;
            if self.next == self.num_words {
                self.next = 0;
            }
        }
        data.len()
    }
}


#[cfg(test)]
mod test_tokio_kcp {
    use tokio_kcp::{KcpConfig, KcpListener, KcpStream};
    use tracing::Level;
    // use futures::{AsyncBufReadExt, AsyncWriteExt};
    use crate::{async_rt::spawn_with_name, ice::throughput::run_throughput, proto::ThroughputArgs};
    // use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    #[tokio::test]
    async fn test_tokio_kcp_throughput() {
        tracing_subscriber::fmt()
        .with_max_level(Level::DEBUG)
        .init();

        let args = ThroughputArgs {
            send_buf_size: 32*1024,
            recv_buf_size: 32*1024,
            ..Default::default()
        };

        let config = KcpConfig::default();

        let args1 = args.clone();
        let mut listener = KcpListener::bind(config.clone(), "0.0.0.0:11911").await.unwrap();
        let task1 = spawn_with_name("server", async move {
            let (stream, addr) = listener.accept().await.unwrap();
            println!("accepted from {}", addr);
            let (rd, wr) = tokio::io::split(stream);
            run_throughput(rd, wr, args1).await.unwrap();
        });

        let args2 = args;
        let task2 = spawn_with_name("client", async move {
            let remote_addr = "127.0.0.1:11911".parse().unwrap();
            let stream = KcpStream::connect(&config, remote_addr).await.unwrap();
            println!("connected to {}", remote_addr);
            let (rd, wr) = tokio::io::split(stream);
            run_throughput(rd, wr, args2).await.unwrap();
        });

        let _r = task1.await;
        let _r = task2.await;

    }
}

#[cfg(test)]
mod test_quinn {
    // use std::net::ToSocketAddrs;

    use tracing::Level;
    use tracing_subscriber::EnvFilter;
    use crate::{async_rt::spawn_with_name, ice::throughput::run_throughput, proto::ThroughputArgs};
    // use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
    use common::{make_server_endpoint, make_client_endpoint};
    use tracing::info;
    use if_watch::tokio::IfWatcher;

    #[tokio::test]
    async fn test_quinn_throughput() {
        // cargo test --release -- --nocapture test_quinn_throughput

        tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_env_filter(EnvFilter::from("rtun=debug"))
        .init();

        let args = ThroughputArgs {
            send_buf_size: 32*1024,
            recv_buf_size: 32*1024,
            ..Default::default()
        };


        let server_connect_addr = "127.0.0.1:11911".parse().unwrap();
        let server_listen_addr = "0.0.0.0:11911".parse().unwrap();
        // let server_listen_addr = ":::11911".to_socket_addrs().unwrap().next().unwrap();
        // let server_listen_addr = SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 0], 11911));
        let (endpoint, server_cert) = make_server_endpoint(server_listen_addr).unwrap();

        let server_addr = endpoint.local_addr().unwrap();
        info!("server local addr {}", server_addr);

        if server_addr.ip().is_unspecified() {
            let watcher = IfWatcher::new().unwrap();
            info!("ifnet list: ==>");
            for (n, ifnet) in watcher.iter().enumerate() {
                let if_addr = ifnet.addr();
                let yes = (server_addr.is_ipv4() && if_addr.is_ipv4()) 
                || (server_addr.is_ipv6() && if_addr.is_ipv6());
                info!("No.{} ifnet {ifnet:?}, same family {yes}", n+1, );
            }
            info!("ifnet list: <==");
        }

        let args1 = args.clone();
        let endpoint1 = endpoint.clone();
        let task1 = spawn_with_name("server", async move {
            let incoming_conn = endpoint1.accept().await.unwrap();
            let conn = incoming_conn.await.unwrap();
            info!(
                "[server] connection accepted: remote_address={}",
                conn.remote_address()
            );

            let (wr, rd) = conn.accept_bi().await.unwrap();
            info!("accepted stream");

            // let (rd, wr) = tokio::io::split(stream);
            run_throughput(rd, wr, args1).await.unwrap();
        });



        let args2 = args;
        let task2 = spawn_with_name("client", async move {
            let endpoint = make_client_endpoint("0.0.0.0:0".parse().unwrap(), &[&server_cert]).unwrap();
            info!("client local addr {}", endpoint.local_addr().unwrap());

            // connect to server
            let connection = endpoint
                .connect(server_connect_addr, "localhost")
                .unwrap()
                .await
                .unwrap();
            info!("[client] connected: addr={}", connection.remote_address());
        
            let (wr, rd) = connection.open_bi().await.unwrap();
            info!("opened stream");

            run_throughput(rd, wr, args2).await.unwrap();
        });

        let _r = task1.await;
        let _r = task2.await;

    }

    mod common {
        use quinn::{ClientConfig, Endpoint, ServerConfig};
        use std::{error::Error, net::SocketAddr, sync::Arc};
        
        /// Constructs a QUIC endpoint configured for use a client only.
        ///
        /// ## Args
        ///
        /// - server_certs: list of trusted certificates.
        #[allow(unused)]
        pub fn make_client_endpoint(
            bind_addr: SocketAddr,
            server_certs: &[&[u8]],
        ) -> Result<Endpoint, Box<dyn Error>> {
            let client_cfg = configure_client(server_certs)?;
            let mut endpoint = Endpoint::client(bind_addr)?;
            endpoint.set_default_client_config(client_cfg);
            Ok(endpoint)
        }
        
        /// Constructs a QUIC endpoint configured to listen for incoming connections on a certain address
        /// and port.
        ///
        /// ## Returns
        ///
        /// - a stream of incoming QUIC connections
        /// - server certificate serialized into DER format
        #[allow(unused)]
        pub fn make_server_endpoint(bind_addr: SocketAddr) -> Result<(Endpoint, Vec<u8>), Box<dyn Error>> {
            let (server_config, server_cert) = configure_server()?;
            let endpoint = Endpoint::server(server_config, bind_addr)?;
            Ok((endpoint, server_cert))
        }
        
        /// Builds default quinn client config and trusts given certificates.
        ///
        /// ## Args
        ///
        /// - server_certs: a list of trusted certificates in DER format.
        fn configure_client(server_certs: &[&[u8]]) -> Result<ClientConfig, Box<dyn Error>> {
            let mut certs = rustls::RootCertStore::empty();
            for cert in server_certs {
                certs.add(&rustls::Certificate(cert.to_vec()))?;
            }
        
            let client_config = ClientConfig::with_root_certificates(certs);
            Ok(client_config)
        }
        
        /// Returns default server configuration along with its certificate.
        fn configure_server() -> Result<(ServerConfig, Vec<u8>), Box<dyn Error>> {
            let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
            let cert_der = cert.serialize_der().unwrap();
            let priv_key = cert.serialize_private_key_der();
            let priv_key = rustls::PrivateKey(priv_key);
            let cert_chain = vec![rustls::Certificate(cert_der.clone())];
        
            let mut server_config = ServerConfig::with_single_cert(cert_chain, priv_key)?;
            let transport_config = Arc::get_mut(&mut server_config.transport).unwrap();
            transport_config.max_concurrent_uni_streams(0_u8.into());
        
            Ok((server_config, cert_der))
        }
        
        #[allow(unused)]
        pub const ALPN_QUIC_HTTP: &[&[u8]] = &[b"hq-29"];
    }
}

#[cfg(test)]
mod test_ice_conn {

    use anyhow::Result;
    use tracing::Level;
    use tracing_subscriber::EnvFilter;
    use crate::{async_rt::spawn_with_name, ice::{throughput::run_throughput, ice_peer::{IcePeer, IceConfig}, ice_quic::{UpgradeToQuic, QuicIceCert}}, proto::ThroughputArgs};
    // use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
    use tracing::debug;

    #[tokio::test]
    async fn test_ice_conn_throughput() -> Result<()> {
        // cargo test --release -- --nocapture test_quinn_throughput

        tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_env_filter(EnvFilter::from("rtun=debug"))
        .init();

        let thr_args = ThroughputArgs {
            send_buf_size: 32*1024,
            recv_buf_size: 32*1024,
            ..Default::default()
        };

        let mut peer1 = IcePeer::with_config(IceConfig {
            ..Default::default()
        });
    
        let arg1 = peer1.client_gather().await?;
        debug!("arg1 {arg1:?}");

        let mut peer2 = IcePeer::with_config(IceConfig {
            ..Default::default()
        });
        
        let arg2 = peer2.server_gather(arg1).await?;
        debug!("arg2 {arg2:?}");
    
        let cert1 = QuicIceCert::try_new()?;
        let cert2 = QuicIceCert::try_new()?;

        let cert_der1 = cert1.to_bytes()?;
        let cert_der2 = cert2.to_bytes()?;

        let thr_args1 = thr_args.clone();
        let task1 = spawn_with_name("client", async move {
            let conn = peer1.dial(arg2).await?
            .upgrade_to_quic(&cert1, cert_der2.into()).await?;
            let (wr, rd) = conn.open_bi().await?;
            run_throughput(rd, wr, thr_args1).await?;
            Result::<()>::Ok(())
        });
    
        let thr_args2 = thr_args.clone();
        let task2 = spawn_with_name("server", async move {
            let conn = peer2.accept().await?
            .upgrade_to_quic(&cert2, cert_der1.into()).await?;
            let (wr, rd) = conn.accept_bi().await?;
            run_throughput(rd, wr, thr_args2).await?;
            Result::<()>::Ok(())
        });
    
        let r1 = task1.await?;
        let r2 = task2.await?;
    
        debug!("task1 finished {r1:?}");
        debug!("task2 finished {r2:?}");

        r1?;
        r2?;

        Ok(())

    }

    mod common {
        use quinn::{ClientConfig, Endpoint, ServerConfig};
        use std::{error::Error, net::SocketAddr, sync::Arc};
        
        /// Constructs a QUIC endpoint configured for use a client only.
        ///
        /// ## Args
        ///
        /// - server_certs: list of trusted certificates.
        #[allow(unused)]
        pub fn make_client_endpoint(
            bind_addr: SocketAddr,
            server_certs: &[&[u8]],
        ) -> Result<Endpoint, Box<dyn Error>> {
            let client_cfg = configure_client(server_certs)?;
            let mut endpoint = Endpoint::client(bind_addr)?;
            endpoint.set_default_client_config(client_cfg);
            Ok(endpoint)
        }
        
        /// Constructs a QUIC endpoint configured to listen for incoming connections on a certain address
        /// and port.
        ///
        /// ## Returns
        ///
        /// - a stream of incoming QUIC connections
        /// - server certificate serialized into DER format
        #[allow(unused)]
        pub fn make_server_endpoint(bind_addr: SocketAddr) -> Result<(Endpoint, Vec<u8>), Box<dyn Error>> {
            let (server_config, server_cert) = configure_server()?;
            let endpoint = Endpoint::server(server_config, bind_addr)?;
            Ok((endpoint, server_cert))
        }
        
        /// Builds default quinn client config and trusts given certificates.
        ///
        /// ## Args
        ///
        /// - server_certs: a list of trusted certificates in DER format.
        fn configure_client(server_certs: &[&[u8]]) -> Result<ClientConfig, Box<dyn Error>> {
            let mut certs = rustls::RootCertStore::empty();
            for cert in server_certs {
                certs.add(&rustls::Certificate(cert.to_vec()))?;
            }
        
            let client_config = ClientConfig::with_root_certificates(certs);
            Ok(client_config)
        }
        
        /// Returns default server configuration along with its certificate.
        fn configure_server() -> Result<(ServerConfig, Vec<u8>), Box<dyn Error>> {
            let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
            let cert_der = cert.serialize_der().unwrap();
            let priv_key = cert.serialize_private_key_der();
            let priv_key = rustls::PrivateKey(priv_key);
            let cert_chain = vec![rustls::Certificate(cert_der.clone())];
        
            let mut server_config = ServerConfig::with_single_cert(cert_chain, priv_key)?;
            let transport_config = Arc::get_mut(&mut server_config.transport).unwrap();
            transport_config.max_concurrent_uni_streams(0_u8.into());
        
            Ok((server_config, cert_der))
        }
        
        #[allow(unused)]
        pub const ALPN_QUIC_HTTP: &[&[u8]] = &[b"hq-29"];
    }
}

