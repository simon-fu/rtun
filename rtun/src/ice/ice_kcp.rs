use std::{
    io::{self, ErrorKind},
    time::Duration,
};

use anyhow::Result;
use bytes::{Buf, Bytes, BytesMut};
use tokio::{sync::mpsc, time::Instant};

use crate::{
    async_rt::spawn_with_name,
    ice::bytes_utils::BytesCursor,
    kcp::{
        kcp2::{Kcp, KcpWriteBuf},
        Error as KcpError,
    },
};

use super::{
    mpsc_pair::MpscPair,
    webrtc_ice_peer::{IceConn, IceWriteHalf},
};

pub trait UpgradeToKcp {
    fn upgrade_to_kcp(self, conv: u32) -> Result<IceKcpConnection>;
}

impl UpgradeToKcp for IceConn {
    fn upgrade_to_kcp(self, conv: u32) -> Result<IceKcpConnection> {
        let conn = kick_xfer(self, conv);
        Ok(conn)
    }
}

fn kick_xfer(conn: IceConn, conv: u32) -> MpscPair {
    let mut kcp = Kcp::new_stream(conv, KcpWriteBuf::new(()));
    kcp.set_wndsize(256, 256);
    // kcp.set_nodelay(true, 20, 2, true);

    let (tx1, rx1) = mpsc::channel(128);
    let (tx2, rx2) = mpsc::channel(128);

    spawn_with_name("task-kcp", async move {
        let r = kcp_task(kcp, conn, tx2, rx1).await;
        tracing::debug!("finished {r:?}");
        r
    });

    MpscPair {
        writer: tx1.into(),
        reader: rx2.into(),
    }
}

async fn kcp_task(
    mut kcp: KcpCtrl,
    conn: IceConn,
    tx: mpsc::Sender<Bytes>,
    mut rx: mpsc::Receiver<Bytes>,
) -> Result<()> {
    let (mut rd, mut wr) = conn.split();

    let limit = LimitArgs {
        max_send_size: kcp.mss() * 128,
    };

    let mut pending_to_conn: Option<BytesCursor> = None;
    let mut pending_to_kcp: Option<BytesCursor> = None;
    let mut pending_to_tx: Option<Bytes> = None;
    let mut kcp_recv_buf = BytesMut::new();

    let mut conn_rd_buf = vec![0; 1700];
    let mut next = kcp_update(&mut kcp)?;

    loop {
        kcp_try_send(&mut kcp, &mut pending_to_kcp, &limit)?;

        if pending_to_conn.is_none() {
            pending_to_conn = kcp.output_mut().pop_front().map(|x| x.into());
        }

        if pending_to_tx.is_none() {
            kcp_recv_buf.resize(1700, 0);
            let num = kcp_try_recv(&mut kcp, &mut kcp_recv_buf[..])?;
            if num > 0 {
                pending_to_tx = Some(kcp_recv_buf.split_to(num).freeze());
            }
        }

        tokio::select! {
            r = rx.recv(), if pending_to_kcp.is_none() => {
                match r {
                    Some(d) => {
                        pending_to_kcp = Some(d.into());
                    },
                    None => break,
                }
            },
            r = conn_try_send(&mut wr, &mut pending_to_conn, &limit), if pending_to_conn.is_some()  => {
                r?;
            }
            r = rd.read_data(&mut conn_rd_buf[..]) , if pending_to_tx.is_none() => {
                let len = r?;

                if len == 0 {
                    break;
                }

                kcp_try_input(&mut kcp, &conn_rd_buf[..len])?;
            },
            r = tx.reserve(), if pending_to_tx.is_some() => {
                let permit = r?;
                if let Some(data) = pending_to_tx.take() {
                    permit.send(data);
                }
            },
            _r = tokio::time::sleep_until(next) => {
                next = kcp_update(&mut kcp)?;
            }
        }
    }

    tracing::debug!("flush start");

    let _r = conn_try_send(&mut wr, &mut pending_to_conn, &limit).await;

    if let Some(data) = pending_to_tx.take() {
        let _r = tx.send(data).await;
    }

    let _r = kcp_try_flush(&mut kcp, &mut wr, &limit).await;

    while let Ok(len) = rd.read_data(&mut conn_rd_buf[..]).await {
        kcp_try_input(&mut kcp, &conn_rd_buf[..len])?;

        kcp_recv_buf.resize(1700, 0);
        let num = kcp_try_recv(&mut kcp, &mut kcp_recv_buf[..])?;
        if num > 0 {
            let data = kcp_recv_buf.split_to(num).freeze();
            if let Err(_e) = tx.send(data).await {
                break;
            }
        }
    }

    kcp_try_send(&mut kcp, &mut pending_to_kcp, &limit)?;

    let _r = kcp_try_flush(&mut kcp, &mut wr, &limit).await;

    tracing::debug!("flush done");

    Ok(())
}

async fn conn_try_send(
    wr: &mut IceWriteHalf,
    pending: &mut Option<BytesCursor>,
    _limit: &LimitArgs,
) -> Result<()> {
    if let Some(cursor) = pending {
        while cursor.remaining() > 0 {
            let num = wr.write_data(cursor.chunk()).await?;
            cursor.advance(num);
        }
        pending.take();
    }
    Ok(())
}

fn kcp_try_send(
    kcp: &mut KcpCtrl,
    pending: &mut Option<BytesCursor>,
    limit: &LimitArgs,
) -> Result<()> {
    if let Some(cursor) = pending {
        let max_wait_snd = (kcp.snd_wnd() as usize) + 8;
        while cursor.remaining() > 0 && kcp.wait_snd() < max_wait_snd {
            let num = cursor.remaining().min(limit.max_send_size);
            let num = kcp.send(&cursor.chunk()[..num])?;
            cursor.advance(num);
        }

        if cursor.remaining() == 0 {
            pending.take();
        }
    }
    Ok(())
}

fn kcp_try_recv(kcp: &mut KcpCtrl, buf: &mut [u8]) -> Result<usize> {
    let r = kcp.recv(buf);
    match r {
        Ok(n) => Ok(n),
        Err(KcpError::RecvQueueEmpty) => Ok(0),
        Err(e) => Err(e.into()),
    }
}

fn kcp_try_input(kcp: &mut KcpCtrl, mut data: &[u8]) -> Result<()> {
    while data.len() > 0 {
        let cnt = kcp.input(data)?;
        data.advance(cnt);
    }
    Ok(())
}

async fn kcp_try_flush(kcp: &mut KcpCtrl, wr: &mut IceWriteHalf, limit: &LimitArgs) -> Result<()> {
    kcp_update(kcp)?;
    kcp.flush()?;
    kcp.flush_ack()?;

    while let Some(data) = kcp.output_mut().pop_front() {
        conn_try_send(wr, &mut Some(data.into()), &limit).await?;
    }
    Ok(())
}

pub type IceKcpConnection = MpscPair;

#[derive(Debug)]
struct LimitArgs {
    max_send_size: usize,
}

type KcpCtrl = Kcp<KcpWriteBuf>;

fn kcp_update(kcp: &mut KcpCtrl) -> io::Result<Instant> {
    let now = now_millis();
    kcp.update(now).map_err(into_io)?;
    let next = kcp.check(now);
    let next = Instant::now() + Duration::from_millis(next as u64);
    Ok(next)
}

#[inline]
pub fn now_millis() -> u32 {
    let start = std::time::SystemTime::now();
    let since_the_epoch = start
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time went afterwards");
    // (since_the_epoch.as_secs() * 1000 + since_the_epoch.subsec_millis() as u64) as u32
    since_the_epoch.as_millis() as u32
}

fn into_io<E: std::fmt::Debug>(e: E) -> io::Error {
    io::Error::new(ErrorKind::ConnectionAborted, format!("{:?}", e))
}
