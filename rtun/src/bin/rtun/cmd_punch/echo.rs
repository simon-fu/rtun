



use std::{net::SocketAddr, sync::Arc, time::Duration};
use bytes::{Buf, BufMut, BytesMut};
use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tracing::{info, span, Instrument, Level};
use crate::cmd_punch::udp_acceptor::new_udp_reuseport_v4;


pub async fn kick_echo_server(listen_addr: SocketAddr) -> Result<()> {
    let socket = UdpSocket::bind(listen_addr).await.with_context(||"bind echo server socket failed")?;
    
    let span = span!(parent: None, Level::DEBUG, "echo-server");
    tokio::spawn(async move {
        let mut buf = vec![0_u8; 1700];
        let mut num = 0_u64;
        loop {
            let r = socket.recv_from(&mut buf).await;
            let (len, from) = match r {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("recv_from error [{e:?}]");
                    continue;
                },
            };

            num += 1;
            let packet = &buf[..len];

            let seq = if len == 8 {
                Some((&packet[..]).get_u64())
            } else {
                None
            };

            info!("recv num [{num}], from [{from}], len [{len}], seq {seq:?}");
            let r = socket.send_to(packet, from).await;
            match r {
                Ok(_v) => {},
                Err(e) => {
                    tracing::warn!("No.{num}: send echo error [{e:?}] [{from}]");
                },
            }
        }
    }.instrument(span));

    Ok(())
}

pub async fn kick_echo_client(target: SocketAddr) -> Result<()> {
    let socket = new_udp_reuseport_v4().with_context(||"bind echo client socket failed")?;
    socket.connect(target).await.with_context(||"echo client connect failed")?;
    let socket = Arc::new(socket);

    {
        let span = span!(parent: None, Level::DEBUG, "echo-recv");
        let socket = socket.clone();
        tokio::spawn(async move {
            echo_recv(socket).await
        }.instrument(span));
    }

    {
        let span = span!(parent: None, Level::DEBUG, "echo-send");
        let socket = socket.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1000)).await;
            echo_send(socket).await
        }.instrument(span));
    }

    Ok(())
}

async fn echo_send(socket: Arc<UdpSocket>) {
    let mut buf = BytesMut::new();
    for n in 0..300_u64 {
        // let content = format!("msg {n}");
        // let r = tun_socket.send(content.as_bytes()).await.with_context(||"udp 
        buf.put_u64(n);
        let packet = buf.split_to(8);
        let r = socket.send(&packet[..]).await.with_context(||"udp send failed");

        match r {
            Ok(_len) => {
                // info!("No.{n}:  sent len [{_len}]");
            },
            Err(e) => {
                tracing::warn!("No.{n}: send error [{e:?}]");
            },
        }

        // let len = socket.send_to(content.as_bytes(), remote_addr).await.with_context(||"udp sen dto failed")?;
        // info!("sent len [{len}]");

        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

async fn echo_recv(socket: Arc<UdpSocket>) {
    let mut buf = vec![0_u8; 1700];
    let mut next_seq = 0_u64;
    loop {
        let r = socket.recv(&mut buf).await;

        let len = match r {
            Ok(len) => len,
            Err(e) => {
                tracing::warn!("recv error [{e:?}]");
                continue;
            },
        };

        if len != 8 {
            tracing::warn!("invalid len [{len}], seq [{next_seq}]");
            continue;
        }

        let seq = (&buf[..8]).get_u64();

        if seq != next_seq {
            tracing::warn!("expect seq [{next_seq}], but [{seq}]");
            if seq > next_seq {
                next_seq = seq + 1;
            }
            continue;
        }
        info!("recv seq [{seq}] ok");
        next_seq += 1;
        
    }
}
