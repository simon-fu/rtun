
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


#[cfg(test)]
mod test_udp {
    use std::{net::SocketAddr, sync::Arc, time::Duration};
    use tokio::net::UdpSocket;
    use tracing::{debug, Instrument};

    #[tokio::test]
    #[ignore = "manual long-running UDP debug test"]
    async fn test_udp() {
        tracing_subscriber::fmt()
        .with_max_level(tracing::metadata::LevelFilter::DEBUG)
        .with_target(false)
        .with_ansi(true)
        .init();

        let span = tracing::span!(parent: None, tracing::Level::DEBUG, "", s="root");
        udp_loop().instrument(span).await;
    }

    async fn udp_loop() {
        let local_addr: SocketAddr = "0.0.0.0:10000".parse().unwrap();
        let socket = new_udp_reuseport(local_addr);
        // let socket = UdpSocket::bind("0.0.0.0:10000").await.unwrap();
        let socket = Arc::new(socket);
        // let local_addr = socket.local_addr().unwrap();
        debug!("listening at {}", local_addr);

        // let connections: HashMap<String, Connection> = HashMap::new();
        let mut buf = vec![0; 1700];
        loop {
            let (n, from) = socket.recv_from(&mut buf).await.unwrap();
            debug!("n={n}, from {from}");
            let peer_socket: Arc<UdpSocket> = new_udp_reuseport(local_addr).into();
            // let peer_socket = {
            //     let socket_fd = socket.as_raw_fd();
            //     let std_socket = unsafe { std::net::UdpSocket::from_raw_fd(socket_fd) };
            //     UdpSocket::from_std(std_socket).unwrap()
            // };

            peer_socket.connect(from).await.unwrap();
            let root_socket = socket.clone();
            
            let sid = format!("{from}");
            let span = tracing::span!(parent: None, tracing::Level::DEBUG, "", s=&sid);

            let _task = tokio::spawn(async move {
                peer_loop(peer_socket, from, root_socket).await;
            }.instrument(span));
            // let _r = _task.await;
        }
    }

    async fn peer_loop(socket: Arc<UdpSocket>, _peer_addr: SocketAddr, _root_socket: Arc<UdpSocket>) {
        let mut buf = vec![0; 1700];
        // let mut num = 0_u64;
        loop {
            debug!(" ===> ");
            tokio::select! {
                r = socket.recv(&mut buf) => {
                    match r {
                        Ok(n) => {
                            debug!("recv n={n}");
                        },
                        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                            debug!("ignore ConnectionRefused");
                        }
                        Err(e) => {
                            debug!("recv error [{e:?}]");
                        }
                    }
                }
                _r = tokio::time::sleep(Duration::from_secs(3)) => {
                    // num += 1;
                    // let data = format!("hello {num}\n");
                    // debug!("sending num {num} ...");
                    // // root_socket.send_to(data.as_bytes(), peer_addr).await.unwrap();
                    // let r = socket.send(data.as_bytes()).await;
                    // debug!("sent num {num}, result {r:?}");
                }
            }
            debug!(" <=== ");

        }
    }

    fn new_udp_reuseport(local_addr: SocketAddr) -> UdpSocket {
        let udp_sock = socket2::Socket::new(
            if local_addr.is_ipv4() {
                socket2::Domain::IPV4
            } else {
                socket2::Domain::IPV6
            },
            socket2::Type::DGRAM,
            None,
        )
        .unwrap();
        udp_sock.set_reuse_port(true).unwrap();
        // udp_sock.set_reuse_address(true).unwrap();
        // from tokio-rs/mio/blob/master/src/sys/unix/net.rs
        udp_sock.set_cloexec(true).unwrap();
        udp_sock.set_nonblocking(true).unwrap();
        udp_sock.bind(&socket2::SockAddr::from(local_addr)).unwrap();
        let udp_sock: std::net::UdpSocket = udp_sock.into();
        udp_sock.try_into().unwrap()
    }
    
}
