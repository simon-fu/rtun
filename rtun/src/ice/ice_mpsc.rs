

use anyhow::{Result, anyhow, Context};
use bytes::{Bytes, BytesMut, Buf};
use tokio::sync::mpsc;

use crate::async_rt::spawn_with_name;

use super::{webrtc_ice_peer::{IceConn, IceWriteHalf, IceReadHalf}, mpsc_pair::MpscPair};



pub trait UpgradeToMpsc {
    fn upgrade_to_mpsc(self, conv: u32) -> Result<IceMpsc>;
}

impl UpgradeToMpsc for IceConn {
    fn upgrade_to_mpsc(self, conv: u32) -> Result<IceMpsc> {
        let conn = kick_xfer2(self, conv);
        Ok(conn)
    }
}

fn kick_xfer2(conn: IceConn, _conv: u32) -> MpscPair {

    let (tx1, rx1) = mpsc::channel(128);
    let (tx2, rx2) = mpsc::channel(128);
    let (rd, wr) = conn.split();

    spawn_with_name("task-ice-mpsc-read", async move {
        let r = read_task(rd, tx2).await;
        tracing::debug!("finished {r:?}");
        r
    });

    spawn_with_name("task-ice-mpsc-write", async move {
        let r = write_task(wr, rx1).await;
        tracing::debug!("finished {r:?}");
        r
    });

    MpscPair {
        writer: tx1.into(),
        reader: rx2.into(),
    }
}

async fn read_task(mut rd: IceReadHalf, tx: mpsc::Sender<Bytes>) -> Result<()> {
    let mut conn_rd_buf = BytesMut::new();
    loop {
        conn_rd_buf.resize(1700, 0);
        let len = rd.read_data(&mut conn_rd_buf[..]).await?;
        if len == 0 {
            break;
        }
        let data = conn_rd_buf.split_to(len).freeze();
        let _r = tx.send(data).await.map_err(|_e|anyhow!("tx failed"))?;
    }
    Ok(())
}

async fn write_task(mut wr: IceWriteHalf, mut rx: mpsc::Receiver<Bytes>) -> Result<()> {
    let mtu = 1400;

    loop {
        let data = rx.recv().await.with_context(||"rx failed")?;
        let mut data = &data[..];
        while data.len() > 0 {
            let len = data.len().min(mtu);
            let n = wr.write_data(&data[..len]).await?;
            data.advance(n);
        }
    }
}

// fn kick_xfer(conn: IceConn, _conv: u32) -> MpscPair {

//     let (tx1, rx1) = mpsc::channel(128);
//     let (tx2, rx2) = mpsc::channel(128);

//     spawn_with_name("task-ice-mpsc", async move {
//         let r = xfer_task(conn, tx2, rx1).await;
//         tracing::debug!("finished {r:?}");
//         r
//     });

//     MpscPair {
//         writer: tx1.into(),
//         reader: rx2.into(),
//     }
// }

// async fn xfer_task(conn: IceConn, tx: mpsc::Sender<Bytes>, mut rx: mpsc::Receiver<Bytes>) -> Result<()> {

//     let (mut rd, mut wr) = conn.split();


//     let mut pending_to_conn: Option<BytesCursor> = None;
//     let mut pending_to_tx: Option<Bytes> = None;

//     let mut conn_rd_buf = BytesMut::new();

//     loop {
        
//         if pending_to_tx.is_none() {
//             conn_rd_buf.resize(1700, 0);
//         }

//         tokio::select! {
//             r = rx.recv(), if pending_to_conn.is_none() => {
//                 match r {
//                     Some(d) => {
//                         pending_to_conn = Some(d.into());
//                     },
//                     None => break,
//                 }
//             },
//             r = conn_try_send(&mut wr, &mut pending_to_conn, 1400), if pending_to_conn.is_some()  => {
//                 r?;
//             }
//             r = rd.read_data(&mut conn_rd_buf[..]) , if pending_to_tx.is_none() => {
//                 let len = r?;

//                 if len == 0 {
//                     break;
//                 }
                
//                 pending_to_tx = Some(conn_rd_buf.split_to(len).freeze());
                
//             },
//             r = tx.reserve(), if pending_to_tx.is_some() => {
//                 let permit = r?;
//                 if let Some(data) = pending_to_tx.take() {
//                     permit.send(data);
//                 }
//             },
//         }
//     }

    

//     Ok(())
// }


// async fn conn_try_send(wr: &mut IceWriteHalf, pending: &mut Option<BytesCursor>, mtu: usize) -> Result<()> {
//     if let Some(cursor) = pending { 
//         while cursor.remaining() > 0 {
//             let data = cursor.chunk();
//             let data = &data[..data.len().min(mtu)];
//             let num = wr.write_data(data).await?;
//             cursor.advance(num);
//         }
//         pending.take();
//     }
//     Ok(())
// }







pub type IceMpsc = MpscPair;


