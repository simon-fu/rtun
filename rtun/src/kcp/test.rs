use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use ::kcp::Kcp;

#[test]
fn test_kcp_flush() {
    let output = MockOutput::default();
    let mut kcp = Kcp::new(123, output.clone());

    kcp.update(100).unwrap();

    let r = kcp.send("abc".as_bytes()).unwrap();
    assert_eq!(r, 3);

    let r = output.pop_front();
    assert_eq!(r.is_some(), false, "{r:?}");

    let _r = kcp.flush().unwrap();
    let r = output.pop_front();
    assert_eq!(r.is_some(), true, "{r:?}");

    let r = kcp.send("def".as_bytes()).unwrap();
    assert_eq!(r, 3);

    let r = output.pop_front();
    assert_eq!(r.is_some(), false, "{r:?}");

    // println!("snd_wnd {}", kcp.snd_wnd());
    // kcp.update(201).unwrap();
    // let r = output.pop_front();
    // assert_eq!(r.is_some(), true, "{r:?}");

    let _r = kcp.flush().unwrap();
    let r = output.pop_front();
    assert_eq!(r.is_some(), true, "{r:?}"); // <== panicked at here !!!!!!
}

#[test]
fn test_kcp_send() {
    let output = MockOutput::default();
    let mut kcp = Kcp::new_stream(123, output.clone());

    kcp.set_nodelay(true, 20, 2, true);
    // kcp.set_rx_minrto(10);
    // kcp.set_fast_resend(1);

    kcp.update(100).unwrap();

    let big_buf = vec![0; 1024];
    for n in 0..10 * 1024 {
        let r = kcp.send(&big_buf[..]).unwrap();
        assert_eq!(r, big_buf.len(), "{n}");
    }

    println!("send() done, {kcp:#?}",);

    kcp.flush().unwrap();
    println!("flush() 1");

    kcp.update(1100).unwrap();
    println!("update() 1");
    kcp.flush().unwrap();
    println!("flush() 2, {kcp:#?}");
}

#[derive(Default, Clone)]
struct MockOutput {
    que: Arc<Mutex<VecDeque<WriteOp>>>,
}

impl MockOutput {
    fn pop_front(&self) -> Option<WriteOp> {
        self.que.lock().unwrap().pop_front()
    }
}

impl std::io::Write for MockOutput {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        println!("op write {}", buf.len());
        self.que
            .lock()
            .unwrap()
            .push_back(WriteOp::Write(Vec::from(buf)));
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        println!("op flush");
        self.que.lock().unwrap().push_back(WriteOp::Flush);
        Ok(())
    }
}

#[derive(Debug)]
enum WriteOp {
    Write(Vec<u8>),
    Flush,
}

// #[inline]
// pub fn now_millis() -> u32 {
//     let start = std::time::SystemTime::now();
//     let since_the_epoch = start.duration_since(std::time::UNIX_EPOCH).expect("time went afterwards");
//     // (since_the_epoch.as_secs() * 1000 + since_the_epoch.subsec_millis() as u64) as u32
//     since_the_epoch.as_millis() as u32
// }
