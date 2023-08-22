use anyhow::{Result, anyhow};

use rtun::channel::{ChSender, ChReceiver};

use termwiz::{caps::Capabilities, terminal::{new_terminal, Terminal, ScreenSize}};

use crate::client_ch_pty::{PtyChSender, PtyChReceiver, process_recv_result};

use self::async_term::async_term;

pub async fn run(ch_tx: ChSender, ch_rx: ChReceiver) -> Result<()> {

    let tx = PtyChSender::new(ch_tx);
    let mut rx = PtyChReceiver::new(ch_rx);

    let mut fin = async_term().await?;
    
    loop {
        tokio::select! {
            r = fin.read() => {
                // tracing::debug!("read input {:?}", r);

                let ev = r?;
                tx.send_event(ev).await.map_err(|_e|anyhow!("send pty event fail"))?; 

                // if data.len() == 0 {
                //     break;
                // }

                // tx.send_stdin_data(data.to_vec().into()).await.map_err(|_e|anyhow!("send_data fail"))?; 
            },
            r = rx.recv_packet() => {
                process_recv_result(r).await?;
            }
        }
    }

    // Ok(())
}



pub async fn get_size() -> Result<ScreenSize> {
    let size = tokio::task::spawn_blocking(|| {
        let caps = Capabilities::new_from_env()?;
        let mut terminal = new_terminal(caps)?;
        let size  = terminal.get_screen_size()?;
        Result::<_>::Ok(size)
    }).await??;
    Ok(size)
}

mod async_term {
    use anyhow::{Result, Context};
    use futures::channel::oneshot;
    use rtun::pty::{PtyEvent, PtySize};
    use termwiz::{caps::Capabilities, terminal::{new_terminal, Terminal}, input::{InputEvent, KeyEvent, KeyCode, Modifiers}};
    use std::{sync::Arc, time::Duration, collections::VecDeque};
    use bytes::BytesMut;
    use parking_lot::Mutex;
    use tokio::sync::watch;

    pub async fn async_term() -> Result<AsyncTerm> { 
        let (tx, rx) = watch::channel(());
    
        let shared = Arc::new(Shared {
            tx,
            data: Default::default(),
        });
        
        {
            let (tx, rx) = oneshot::channel();

            spawn_read_thread(shared.clone(), tx);

            rx.await
            .with_context(||"init async input unexpect")??;
        }
    
        Ok(AsyncTerm {
            rx,
            shared,
        })
    }

    #[derive(Default)]
    struct EventQue {
        que: VecDeque<PtyEvent>,
        last_stdin_buf: BytesMut,
    }

    impl EventQue {

        pub fn pop(&mut self) -> Option<PtyEvent> {
            match self.que.pop_front() {
                Some(v) => Some(v),
                None => {
                    self.check_stdin_buf();
                    self.que.pop_front()
                },
            }
        }

        pub fn push_stdin_data(&mut self, data: &[u8]) {
            self.last_stdin_buf.extend_from_slice(data);
        }

        pub fn push_resize(&mut self, cols: u16, rows: u16) {
            self.check_stdin_buf();
            self.que.push_back(PtyEvent::Resize(PtySize{ cols, rows, }));
        }

        fn check_stdin_buf(&mut self) {
            if self.last_stdin_buf.len() > 0 {
                let data = self.last_stdin_buf.split().freeze();
                self.que.push_back(PtyEvent::StdinData(data));
            }
        }
    }
    
    fn init_terminal() -> Result<impl Terminal> {

        // 开启鼠标会导致 iterm 无法用鼠标选取文本
        let caps = Capabilities::new_with_hints(
            termwiz::caps::ProbeHints::new_from_env().mouse_reporting(Some(false))
        )?;

        // let caps = Capabilities::new_from_env()?;

        let terminal = new_terminal(caps)?;
        Ok(terminal)
    }

    fn spawn_read_thread(shared: Arc<Shared>, init_tx: oneshot::Sender<Result<()>>) {
        let _r = std::thread::spawn(move || {

            let terminal = match  init_terminal() {
                Ok(terminal) => {
                    let _r = init_tx.send(Ok(()));
                    terminal
                },
                Err(e) => {
                    let _r = init_tx.send(Err(e));
                    return Ok(()) // TODO
                },
            };
            
            let r = poll_input_loop(&shared, terminal);

            {
                let mut data = shared.data.lock();
                if data.end_reason.is_none() {
                    if let Err(e) = &r {
                        data.end_reason = Some(EndReason::Unexpect(format!("{e:?}")));
                    }
                }
                data.is_end = true;
            }
            let _r = shared.tx.send(());

            r
        });
    }

    fn poll_input_loop(shared: &Arc<Shared>, mut terminal: impl Terminal) -> Result<()> {

        let interval = Duration::from_secs(1);

        terminal.set_raw_mode()?;

        let mut detector = ForceDetector::default();

        loop{
            let r = terminal.poll_input(Some(interval))?;

            if let Some(event) = r {
                // tracing::debug!("got input {event:?}");

                match &event {
                    InputEvent::Key(KeyEvent {
                        key: KeyCode::Escape,
                        ..
                    }) => {
                        tracing::debug!("got break key1");
                        shared.data.lock().end_reason = Some(EndReason::Forced);
                        break;
                    },

                    InputEvent::Key(key_ev) => {
                        if detector.detect(key_ev) {
                            // tracing::debug!("got break key");
                            shared.data.lock().end_reason = Some(EndReason::Forced);
                            break;
                        }
    
                        if let Some(ks) = try_encode_key(key_ev) {
                            let mut data = shared.data.lock();
                            data.que.push_stdin_data(ks.as_bytes());
                        }

                        // // aaa
                        // try_read_stdin()?;
                    }

                    InputEvent::Resized { cols, rows } => {
                        // tracing::debug!("pty resize cols {cols:?}, rows {rows:?}");
                        {
                            let mut data = shared.data.lock();
                            data.que.push_resize(*cols as u16, *rows as u16);
                        }
                    },
                    InputEvent::Paste(content) => {
                        {
                            let mut data = shared.data.lock();
                            data.que.push_stdin_data(content.as_bytes());
                        }
                    },
                    _ => { }
                }
            }

            let r = shared.tx.send(());
            if r.is_err() {
                break;
            }

        }
        Ok(())
    }

    // fn try_read_stdin() -> Result<()> {
    //     use std::io::Read;
    //     let mut buf = [0; 128];
    //     let n = std::io::stdin().read(&mut buf[..])
    //     .with_context(||"read stdin failed")?;
    //     print!("=== read {n}: {:?}\r\n", std::str::from_utf8(&buf[..n]).ok());
    //     Ok(())
    // }

    fn try_encode_key(key_ev: &KeyEvent) -> Option<String> {
        return key_ev.key.encode(
            key_ev.modifiers, 
            termwiz::input::KeyCodeEncodeModes {
                encoding: termwiz::input::KeyboardEncoding::Xterm,
                application_cursor_keys: false,
                newline_mode: false,
                modify_other_keys: None,
            }, 
            true,
        ).ok();
    }

    #[derive(Debug, Default)]
    struct ForceDetector {
        index: usize,
    }

    impl ForceDetector  {
        fn detect(&mut self, key_ev: &KeyEvent) -> bool {

            const MAGIC: &[u8] = b"zaza";

            if key_ev.modifiers.contains(Modifiers::CTRL) {
                if let KeyCode::Char(c) = key_ev.key {
                    if MAGIC[self.index] == c as u8 {
                        self.index += 1;
                        if self.index >= MAGIC.len() {
                            self.index = 0;
                            return true;
                        } else {
                            return false
                        }
                    }
                }
            }

            self.index = 0;
            false
        }
    }
    
    type Recver = watch::Receiver<()>;
    type Sender = watch::Sender<()>;
    
    #[derive(Clone)]
    pub struct AsyncTerm {
        rx: Recver,
        shared: Arc<Shared>,
    }
    
    impl AsyncTerm {
        pub async fn read(&mut self) -> Result<PtyEvent, EndReason> {
            loop {
                {
                    let mut data = self.shared.data.lock();
                    // if data.buf.len() > 0 {
                    //     return Ok(data.buf.split().freeze())
                    // }
                    if let Some(ev) = data.que.pop() {
                        return Ok(ev)
                    }
    
                    if let Some(reason) = data.end_reason.as_ref() {
                        return Err(reason.clone())
                    }
                }
    
                let _r = self.rx.changed().await;
            }
        }
    }
    
    
    struct Shared {
        tx: Sender,
        data: Mutex<SharedData>,
    }
    
    
    #[derive(Default)]
    struct SharedData {
        // buf: BytesMut,
        que: EventQue,
        end_reason: Option<EndReason>,
        is_end: bool,
    }
    
    #[derive(Debug, Clone)]
    pub enum EndReason {
        // EOF,
        // Error(Arc<std::io::Error>),
        Forced,
        Unexpect(String),
    }
    
    impl std::fmt::Display for EndReason {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            std::fmt::Debug::fmt(&self, f)
        }
    }
    
    impl std::error::Error for EndReason {
        
    }
    
}





// use anyhow::{Result, anyhow};

// use rtun::channel::{ChSender, ChReceiver};

// use termwiz::{caps::Capabilities, terminal::{new_terminal, Terminal, ScreenSize}};

// use crate::client_ch_pty::{PtyChSender, PtyChReceiver, process_recv_result};

// use self::async_input::async_input;

// pub async fn run(ch_tx: ChSender, ch_rx: ChReceiver) -> Result<()> {

//     let tx = PtyChSender::new(ch_tx);
//     let mut rx = PtyChReceiver::new(ch_rx);

//     let mut fin = async_input().await?;
    
//     loop {
//         tokio::select! {
//             r = fin.read() => {
//                 // tracing::debug!("read input {:?}", r);

//                 let ev = r?;
//                 tx.send_event(ev).await.map_err(|_e|anyhow!("send pty event fail"))?; 

//                 // if data.len() == 0 {
//                 //     break;
//                 // }

//                 // tx.send_stdin_data(data.to_vec().into()).await.map_err(|_e|anyhow!("send_data fail"))?; 
//             },
//             r = rx.recv_packet() => {
//                 process_recv_result(r).await?;
//             }
//         }
//     }

//     // Ok(())
// }



// pub async fn get_size() -> Result<ScreenSize> {
//     let size = tokio::task::spawn_blocking(|| {
//         let caps = Capabilities::new_from_env()?;
//         let mut terminal = new_terminal(caps)?;
//         let size  = terminal.get_screen_size()?;
//         Result::<_>::Ok(size)
//     }).await??;
//     Ok(size)
// }

// mod async_input {
//     use anyhow::{Result, Context};
//     use futures::channel::oneshot;
//     use rtun::pty::{PtyEvent, PtySize};
//     use termwiz::{caps::Capabilities, terminal::{new_terminal, Terminal}, input::{InputEvent, KeyEvent, KeyCode, Modifiers}};
//     use std::{sync::Arc, time::Duration, collections::VecDeque};
//     use bytes::BytesMut;
//     use parking_lot::Mutex;
//     use tokio::sync::watch;
    
//     pub async fn async_input() -> Result<AsyncInput> { 
//         let (tx, rx) = watch::channel(());
    
//         let shared = Arc::new(Shared {
//             tx,
//             data: Default::default(),
//         });
        
//         {
//             let (tx, rx) = oneshot::channel();

//             spawn_read_thread(shared.clone(), tx);

//             rx.await
//             .with_context(||"init async input unexpect")??;
//         }
    
//         Ok(AsyncInput {
//             rx,
//             shared,
//         })
//     }

//     #[derive(Default)]
//     struct EventQue {
//         que: VecDeque<PtyEvent>,
//         last_stdin_buf: BytesMut,
//     }

//     impl EventQue {

//         pub fn pop(&mut self) -> Option<PtyEvent> {
//             match self.que.pop_front() {
//                 Some(v) => Some(v),
//                 None => {
//                     self.check_stdin_buf();
//                     self.que.pop_front()
//                 },
//             }
//         }

//         pub fn push_stdin_data(&mut self, data: &[u8]) {
//             self.last_stdin_buf.extend_from_slice(data);
//         }

//         pub fn push_resize(&mut self, cols: u16, rows: u16) {
//             self.check_stdin_buf();
//             self.que.push_back(PtyEvent::Resize(PtySize{ cols, rows, }));
//         }

//         fn check_stdin_buf(&mut self) {
//             if self.last_stdin_buf.len() > 0 {
//                 let data = self.last_stdin_buf.split().freeze();
//                 self.que.push_back(PtyEvent::StdinData(data));
//             }
//         }
//     }
    
//     fn init_terminal() -> Result<impl Terminal> {
//         let caps = Capabilities::new_from_env()?;
//         let terminal = new_terminal(caps)?;
//         Ok(terminal)
//     }

//     fn spawn_read_thread(shared: Arc<Shared>, init_tx: oneshot::Sender<Result<()>>) {
//         let _r = std::thread::spawn(move || {

//             let terminal = match  init_terminal() {
//                 Ok(terminal) => {
//                     let _r = init_tx.send(Ok(()));
//                     terminal
//                 },
//                 Err(e) => {
//                     let _r = init_tx.send(Err(e));
//                     return Ok(()) // TODO
//                 },
//             };
            
//             let r = poll_input_loop(&shared, terminal);

//             {
//                 let mut data = shared.data.lock();
//                 if data.end_reason.is_none() {
//                     if let Err(e) = &r {
//                         data.end_reason = Some(EndReason::Unexpect(format!("{e:?}")));
//                     }
//                 }
//                 data.is_end = true;
//             }
//             let _r = shared.tx.send(());

//             r
//         });
//     }

//     fn poll_input_loop(shared: &Arc<Shared>, mut terminal: impl Terminal) -> Result<()> {

//         let interval = Duration::from_secs(1);

//         terminal.set_raw_mode()?;

//         let mut detector = ForceDetector::default();

//         loop{
//             let r = terminal.poll_input(Some(interval))?;

//             if let Some(event) = r {
//                 // tracing::debug!("got input {event:?}");

//                 match &event {
//                     InputEvent::Key(KeyEvent {
//                         key: KeyCode::Escape,
//                         ..
//                     }) => {
//                         tracing::debug!("got break key1");
//                         shared.data.lock().end_reason = Some(EndReason::Forced);
//                         break;
//                     },

//                     InputEvent::Key(key_ev) => {
//                         if detector.detect(key_ev) {
//                             // tracing::debug!("got break key");
//                             shared.data.lock().end_reason = Some(EndReason::Forced);
//                             break;
//                         }
    
//                         if let Some(ks) = try_encode_key(key_ev) {
//                             {
//                                 let mut data = shared.data.lock();
//                                 // data.buf.extend_from_slice(ks.as_bytes());
//                                 data.que.push_stdin_data(ks.as_bytes());
//                             }
        
//                             let r = shared.tx.send(());
//                             if r.is_err() {
//                                 break;
//                             }
//                         }
//                     }

//                     InputEvent::Resized { cols, rows } => {
//                         // tracing::debug!("pty resize cols {cols:?}, rows {rows:?}");
//                         {
//                             let mut data = shared.data.lock();
//                             data.que.push_resize(*cols as u16, *rows as u16);
//                         }

//                         let r = shared.tx.send(());
//                         if r.is_err() {
//                             break;
//                         }
//                     },
//                     _ => { }
//                 }
//             }

//             if shared.tx.is_closed() {
//                 break;
//             }

//         }
//         Ok(())
//     }

//     fn try_encode_key(key_ev: &KeyEvent) -> Option<String> {
//         return key_ev.key.encode(
//             key_ev.modifiers, 
//             termwiz::input::KeyCodeEncodeModes {
//                 encoding: termwiz::input::KeyboardEncoding::Xterm,
//                 application_cursor_keys: false,
//                 newline_mode: false,
//                 modify_other_keys: None,
//             }, 
//             true,
//         ).ok();
//     }

//     #[derive(Debug, Default)]
//     struct ForceDetector {
//         index: usize,
//     }

//     impl ForceDetector  {
//         fn detect(&mut self, key_ev: &KeyEvent) -> bool {

//             const MAGIC: &[u8] = b"zaza";

//             if key_ev.modifiers.contains(Modifiers::CTRL) {
//                 if let KeyCode::Char(c) = key_ev.key {
//                     if MAGIC[self.index] == c as u8 {
//                         self.index += 1;
//                         if self.index >= MAGIC.len() {
//                             self.index = 0;
//                             return true;
//                         } else {
//                             return false
//                         }
//                     }
//                 }
//             }

//             self.index = 0;
//             false
//         }
//     }
    
//     type Recver = watch::Receiver<()>;
//     type Sender = watch::Sender<()>;
    
//     #[derive(Clone)]
//     pub struct AsyncInput {
//         rx: Recver,
//         shared: Arc<Shared>,
//     }
    
//     impl AsyncInput {
//         pub async fn read(&mut self) -> Result<PtyEvent, EndReason> {
//             loop {
//                 {
//                     let mut data = self.shared.data.lock();
//                     // if data.buf.len() > 0 {
//                     //     return Ok(data.buf.split().freeze())
//                     // }
//                     if let Some(ev) = data.que.pop() {
//                         return Ok(ev)
//                     }
    
//                     if let Some(reason) = data.end_reason.as_ref() {
//                         return Err(reason.clone())
//                     }
//                 }
    
//                 let _r = self.rx.changed().await;
//             }
//         }
//     }
    
    
//     struct Shared {
//         tx: Sender,
//         data: Mutex<SharedData>,
//     }
    
    
//     #[derive(Default)]
//     struct SharedData {
//         // buf: BytesMut,
//         que: EventQue,
//         end_reason: Option<EndReason>,
//         is_end: bool,
//     }
    
//     #[derive(Debug, Clone)]
//     pub enum EndReason {
//         // EOF,
//         // Error(Arc<std::io::Error>),
//         Forced,
//         Unexpect(String),
//     }
    
//     impl std::fmt::Display for EndReason {
//         fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
//             std::fmt::Debug::fmt(&self, f)
//         }
//     }
    
//     impl std::error::Error for EndReason {
        
//     }
    
// }

