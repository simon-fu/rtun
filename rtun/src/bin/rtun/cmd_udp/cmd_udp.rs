
use std::{io, collections::VecDeque};

use anyhow::{Result, bail};

use bytes::{Bytes, BytesMut, BufMut};
use clap::Parser;

use console::Term;

use rtun::async_rt;
use tokio::sync::{oneshot, mpsc};

use super::{tui::footer::{Event, FooterInput}, app::make_app};


pub fn run(args: CmdArgs) -> Result<()> { 
    
    let (event_tx, event_rx) = tokio::sync::mpsc::channel(128);
    
    {
        let event_tx = event_tx.clone();
        crate::init_log2(move || LogWriter::new(event_tx.clone()));
    }
    

    let (app1, app2) = make_app(event_tx.clone())?;
    let listen = args.listen.clone();

    let (async_tx, rx) = oneshot::channel();
    let thread = std::thread::spawn(|| {
        async_rt::run_multi_thread(async move {
            tokio::select! {
                // r =  do_run(_args) => r,
                r = app2.run(listen) => r,
                _r = rx => bail!("got exit"),
            }
        })
    });


    let event_tx1 = event_tx.clone();
    std::thread::spawn(move || -> io::Result<()> {
        let term = Term::stdout();
        loop {
            let key = term.read_key()?; 
            let r = event_tx1.blocking_send(Event::Key(key));
            if r.is_err() {
                return Ok(())
            }
        }
        
    });


    // kick_slow_log(event_tx);


    FooterInput::new(event_rx)
    .run_app(app1)?;


    let _r = async_tx.send(());
    println!("send result {_r:?}");

    let _r = thread.join();
    println!("join result {_r:?}");
    
    Ok(())
}

// /// for test only
// fn kick_slow_log(event_tx: mpsc::Sender<Event>) {
//     use std::time::Duration;
//     std::thread::spawn(move || {
//         for n in 0..100 {
//             std::thread::sleep(Duration::from_secs(2));
//             let _r = event_tx.blocking_send(Event::Log(format!("sync log").into_bytes().into())) ;
//             std::thread::sleep(Duration::from_secs(2));
//             let r = event_tx.blocking_send(Event::Log(format!(" 1234567 {n}\n").into_bytes().into())) ;
//             if r.is_err() {
//                 break;
//             }
//         }
//     });
// }



// struct RwBuf {
    
// }

struct LogWriter {
    tx: mpsc::Sender<Event>,
    buf: BytesMut,
    que: VecDeque<Bytes>,
}


impl LogWriter {
    pub fn new(tx: mpsc::Sender<Event>) -> Self {
        Self { 
            buf: make_buf(),
            que: Default::default(),
            tx 
        }
    }
}

impl io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.len() == 0 {
            return Ok(0)
        }

        // if !self.buf.has_remaining_mut() {
        //     let data = std::mem::replace(&mut self.buf, make_buf()).freeze();
        // }

        let num = (self.buf.capacity() - self.buf.len()).min(buf.len());
        self.buf.put_slice(&buf[..]);
        if self.buf.len() == self.buf.capacity() {
            let data = std::mem::replace(&mut self.buf, make_buf()).freeze();
            self.que.push_back(data);
        }

        while let Some(d) = self.que.pop_front() {
            if let Ok(permit) = self.tx.try_reserve() {
                permit.send(Event::Log(d));
            } else {
                self.que.push_front(d);
                return Ok(num);
            }
        }

        if self.buf.len() > 0 {
            if let Ok(permit) = self.tx.try_reserve() {
                permit.send(Event::Log(self.buf.split().freeze()));
                if !self.buf.has_remaining_mut() {
                    self.buf = make_buf();
                }
            }
        }

        Ok(num)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn make_buf() -> BytesMut {
    const BUF_CAP: usize = 8*1024;
    BytesMut::with_capacity(BUF_CAP)
}




#[derive(Parser, Debug)]
#[clap(name = "udp", author, about, version)]
pub struct CmdArgs {
    #[clap(
        short = 'l',
        long = "listen",
        long_help = "listen address",
    )]
    listen: Option<String>,
}

