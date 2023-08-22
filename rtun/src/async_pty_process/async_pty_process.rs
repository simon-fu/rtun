/*
TODO:
    - impl kill_on_drop by fork pty_process::blocking::Pty
*/

use std::{collections::HashMap, os::fd::{AsFd, AsRawFd}, io::{Read, Write}};
use bytes::{Bytes, BytesMut};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::debug;
use crate::async_mpsc_fd;
use anyhow::{Result, Context, bail};


pub async fn make_async_pty_process<S1, S2, I,>(
    program: S1, 
    args: I,
    row: u16, 
    col: u16,
) -> Result<(Sender, Receiver)>
where
    S1: AsRef<std::ffi::OsStr>,
    S2: AsRef<std::ffi::OsStr>,
    I: IntoIterator<Item = S2>,
{
    lazy_static::lazy_static! {
        static ref MSG_TX: Mutex<async_mpsc_fd::Sender<Msg>> = {
            let (tx, rx) = async_mpsc_fd::unbound();
            std::thread::spawn(move || {
                work_loop(rx)
            });
            Mutex::new(tx)
        };
    }

    let (tx, rx) = mpsc::channel(128);

    let session = make_pty_session(program, args, tx, row, col)?;

    let mut msg_tx = {
        MSG_TX.lock().clone()
    };

    let pty_id = {
        let (tx, rx) = futures::channel::oneshot::channel();
        msg_tx = tokio::task::spawn_blocking(move ||
            msg_tx.blocking_send(Msg::AddSession(session, tx))
            .map(|_x|msg_tx)
        ).await??;
        // msg_tx.blocking_send(Msg::AddSession(session, tx))?;
        rx.await?
    };
    
    Ok((
        Sender {
            tx: msg_tx,
            pty_id,
        },
        Receiver {
            rx,
        }
    ))
}

pub struct Sender {
    tx: async_mpsc_fd::Sender<Msg>,
    pty_id: PtyId,
}

impl Drop for Sender {
    fn drop(&mut self) {
        // TODO: blocking_send may block async task
        let _r = self.tx.blocking_send(Msg::RemoveSession(self.pty_id.clone()));
    }
}

impl Sender {
    pub async fn send_data(&self, value: Bytes) -> Result<(), mpsc::error::SendError<Bytes>> {
        let tx = self.tx.clone();
        let pty_id = self.pty_id.clone();
        tokio::task::spawn_blocking(
            move ||tx.blocking_send(Msg::StdinBytes(pty_id, value)))
        .await
        .unwrap()
        .map_err(|e| {
            match e.0 {
                Msg::StdinBytes(_ptye_id, d) => mpsc::error::SendError(d),
                _ => panic!("never send error"),
            }
        })
    }

    pub async fn send_resize(&self, cols: u16, rows: u16) -> Result<(), mpsc::error::SendError<Bytes>> {
        let tx = self.tx.clone();
        let pty_id = self.pty_id.clone();
        let size = pty_process::Size::new(rows, cols);
        
        tokio::task::spawn_blocking(
            move ||tx.blocking_send(Msg::PtyResize(pty_id, size))
        )
        .await
        .unwrap()
        .map_err(|e| {
            match e.0 {
                Msg::StdinBytes(_ptye_id, d) => mpsc::error::SendError(d),
                _ => panic!("never send error"),
            }
        })
    }
}

pub struct Receiver {
    rx: mpsc::Receiver<Bytes>,
}

impl Receiver {
    pub async fn recv(&mut self) -> Option<Bytes> {
        self.rx.recv().await
    }
}

fn work_loop(mut rx: async_mpsc_fd::Receiver<Msg>) -> Result<()> {

    let socket_fd = rx.fd();
    // let mut alloc_pty_id = 0_u64;
    // let mut pty_map = PtyMap::new();
    let mut ctx = WorkContext {
        alloc_pty_id: 0,
        pty_map: Default::default(),
    };

    'outer: loop {

        let mut set = nix::sys::select::FdSet::new();
        
        set.insert(socket_fd);

        for (_k, session) in ctx.pty_map.iter() {
            set.insert(session.fd);
        }

        let n = nix::sys::select::select(
            None,
            Some(&mut set),
            None,
            None,
            None,
        ).with_context(||"select failed")?;

        if n == 0 {
            continue;
        }

        ctx.pty_map.retain(|key, session| {
            if set.contains(session.fd) {
                let r = session.read_and_send();
                if let Err(e) = r {
                    let _r = session.try_wait_child();
                    debug!("remove pty by read failed: {key:?} {e:?}");
                    return false
                }
            }

            true
        });

        if set.contains(socket_fd) {
            rx.prepared_recv()?;
            loop {
                match rx.try_recv() {
                    Ok(msg) => {
                        if let Some((pty_id, session, op_r)) = process_msg(&mut ctx, msg) {

                            let mut removed = false;
                            if let Some(reason) = session.try_wait_child() {
                                debug!("process msg but exit pty {:?}, reason[{:?}]", pty_id, reason);
                                ctx.pty_map.remove(&pty_id);
                                removed = true;
                            }

                            if !removed {
                                if let Err(e) = op_r {
                                    debug!("process msg failed: {pty_id:?} {e:?}");
                                }
                            }
                        }
                        // match msg {
                        //     Msg::StdinBytes(pty_id, data) => {
                        //         if let Some(session) = pty_map.get_mut(&pty_id)  {
                        //             let op_r = session.pty.write_all(&data[..]);
                                    
                        //             if let Some(reason) = session.try_wait_child() {
                        //                 debug!("write pty but exit [{:?}]", reason)
                        //             }

                        //             if let Err(e) = op_r {
                        //                 let _r = session.try_wait_child();
                        //                 pty_map.remove(&pty_id);
                        //                 debug!("removed pty by failed: {pty_id:?} {e:?}");
                        //             }
                        //         }
                        //     },
                        //     Msg::PtyResize(pty_id, args) => {
                        //         if let Some(session) = pty_map.get_mut(&pty_id)  {
                        //             let op_r = session.pty.resize(args);

                                    
                        //         }
                        //     },
                        //     Msg::AddSession(session, tx) => {
                        //         alloc_pty_id += 1;
                        //         let pty_id = PtyId(alloc_pty_id);
                        //         pty_map.insert(pty_id.clone(), session);
                        //         let _r = tx.send(pty_id.clone());
                        //         debug!("added pty {:?}", pty_id);
                        //     },
                        //     Msg::RemoveSession(pty_id) => {
                        //         if let Some(mut session) = pty_map.remove(&pty_id) {
                        //             let _r = session.child.kill();
                        //             let _r = session.try_wait_child();
                        //             debug!("normal removed pty {:?}", pty_id);
                        //         }
                        //     },
                        // }
                    },
                    Err(e) => {
                        use async_mpsc_fd::UnboundedTryRecvError;
                        match e {
                            UnboundedTryRecvError::Empty => break,
                            UnboundedTryRecvError::Disconnected => break 'outer,
                        }
                    },
                }
            }
        }
    }

    Ok(())
}

type PtyMap = HashMap::<PtyId, PtySession>;
struct WorkContext {
    alloc_pty_id: u64,
    pty_map: PtyMap,
}

fn process_msg(ctx: &mut WorkContext, msg: Msg) -> Option<(PtyId, &mut PtySession, Result<()>)> {
    match msg {
        Msg::StdinBytes(pty_id, data) => {
            if let Some(session) = ctx.pty_map.get_mut(&pty_id)  {
                let op_r = session.pty.write_all(&data[..])
                .with_context(||"write_all failed");
                return Some((pty_id, session, op_r))
            } 
        },
        Msg::PtyResize(pty_id, args) => {
            if let Some(session) = ctx.pty_map.get_mut(&pty_id) {
                // tracing::debug!("pty resize {:?}, args {:?}", pty_id, args);
                let op_r = session.pty.resize(args)
                .with_context(||"resize failed");
                return Some((pty_id, session, op_r))
            }
        },
        Msg::AddSession(session, tx) => {
            ctx.alloc_pty_id += 1;
            let pty_id = PtyId(ctx.alloc_pty_id);
            ctx.pty_map.insert(pty_id.clone(), session);
            let _r = tx.send(pty_id.clone());
            debug!("added pty {:?}", pty_id);
        },
        Msg::RemoveSession(pty_id) => {
            if let Some(mut session) = ctx.pty_map.remove(&pty_id) {
                let _r = session.child.kill();
                let _r = session.try_wait_child();
                debug!("normal removed pty {:?}", pty_id);
            }
        },
    }
    return None
}

// #[derive(Clone)]
enum Msg {
    StdinBytes(PtyId, Bytes),
    PtyResize(PtyId, pty_process::Size),
    AddSession(PtySession, futures::channel::oneshot::Sender<PtyId>),
    RemoveSession(PtyId),
}


struct PtySession {
    pty: pty_process::blocking::Pty, 
    child: std::process::Child,
    fd: std::os::fd::RawFd,
    tx: mpsc::Sender<Bytes>,
    buf: BytesMut,
}

impl PtySession {
    pub fn read_and_send(&mut self) -> Result<()> {
        if self.buf.len() == 0 {
            if self.buf.capacity() == 0 {
                self.buf = BytesMut::with_capacity(4*1024);
            }

            self.buf.resize(self.buf.capacity(), 0);
        }

        let n = self.pty.read(&mut self.buf[..])?;
        // buf.resize(n, 0);
        let packet = self.buf.split_to(n).freeze();
        self.tx.blocking_send(packet)
        .map_err(|_e| anyhow::anyhow!("send pty bytes fail"))?;
        if n == 0 {
            bail!("pty reach eof")
        }

        if let Some(reason) = self.try_wait_child() {
            bail!("read pty but exit [{:?}]", reason)
        }

        Ok(())
    }

    pub fn try_wait_child(&mut self) -> Option<String> {
        match self.child.try_wait() {
            Ok(None) => None,
            Ok(Some(v)) => {
                // bail!("read pty but exit {:?}", v)
                Some(format!("{v:?}"))
            },
            Err(e) => {
                Some(format!("try_wait failed {e:?}"))
            }
        }
    }
}

#[derive(Debug, Clone)]
#[derive(Eq, Hash, PartialEq)]
struct PtyId(u64);

fn make_pty_session<S1, S2, I,>(
    program: S1, 
    args: I, 
    tx: mpsc::Sender<Bytes>,
    row: u16, 
    col: u16,
) -> Result<PtySession>
where
    S1: AsRef<std::ffi::OsStr>,
    S2: AsRef<std::ffi::OsStr>,
    I: IntoIterator<Item = S2>,
{
    let pty = pty_process::blocking::Pty::new()
        .with_context(||"new pty fail")?;

    let pts = pty.pts()
        .with_context(||"new pts fail")?;

    // pty.resize(pty_process::Size::new(24, 80))
    pty.resize(pty_process::Size::new(row, col))
        .with_context(||"resize pty fail")?;

    let child = pty_process::blocking::Command::new(program)
        .args(args)
        .spawn(&pts)
        .with_context(||"spawn command fail")?;

    Ok(PtySession {
        fd: pty.as_fd().as_raw_fd(),
        buf: BytesMut::new(),
        pty,
        child,
        tx,
    })
}
