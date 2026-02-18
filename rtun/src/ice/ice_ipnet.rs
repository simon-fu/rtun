use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use if_watch::{tokio::IfWatcher, IfEvent, IpNet};
use tokio::time::timeout;

use crate::async_rt::dummy;

pub async fn ipnet_iter() -> Result<IpNetIter> {
    let mut watcher = IfWatcher::new().with_context(|| "new if watcher failed")?;

    let r = timeout(Duration::from_millis(1000), watcher.next()).await;

    let mut first = None;
    if let Ok(Some(r)) = r {
        let event = r.with_context(|| "next if event failed")?;
        match event {
            IfEvent::Up(ipnet) => first = Some(ipnet),
            IfEvent::Down(_r) => {}
        }
    }

    Ok(IpNetIter { watcher, first })
}

pub struct IpNetIter {
    watcher: IfWatcher,
    first: Option<IpNet>,
}

// impl IpNetIter {
//     pub async fn next_net(&mut self) -> Option<Result<IpNet>> {

//         loop {
//             let r = timeout(
//                 Duration::from_millis(1000),
//                 self.watcher.next(),
//             ).await;

//             match r {
//                 Ok(Some(Ok(IfEvent::Up(ipnet)))) => {
//                     return Some(Ok(ipnet))
//                 },
//                 Ok(Some(_r)) => {},
//                 Ok(None) => return None,
//                 Err(_e) => return None,
//             }
//         }

//     }
// }

impl Iterator for IpNetIter {
    type Item = Result<IpNet>;

    fn next(&mut self) -> Option<Self::Item> {
        use std::task::Poll;
        let waker = dummy::waker();

        if let Some(first) = self.first.take() {
            return Some(Ok(first));
        }

        loop {
            let r = self.watcher.poll_if_event(&mut dummy::context(&waker));
            match r {
                Poll::Ready(Ok(IfEvent::Up(ipnet))) => return Some(Ok(ipnet)),
                Poll::Ready(Ok(IfEvent::Down(_ipnet))) => {}
                Poll::Ready(Err(e)) => return Some(Err(e).with_context(|| "poll_if_event failed")),
                Poll::Pending => return None,
            }
        }
    }
}

#[tokio::test]
async fn test_ipnet() {
    // use futures::StreamExt;

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_env_filter(tracing_subscriber::EnvFilter::from("rtun=debug"))
        .with_target(false)
        .init();

    let watcher = IfWatcher::new().unwrap();
    tracing::info!("ifnet list: ==>");
    for (n, ifnet) in watcher.iter().enumerate() {
        // let if_addr = ifnet.addr();
        tracing::info!("No.{} ifnet {ifnet:?}", n + 1,);
    }
    tracing::info!("ifnet list: <==");

    tracing::info!("poll ifnet event ==>");

    let mut iter = ipnet_iter().await.unwrap();
    let mut fun = || {
        while let Some(r) = iter.next() {
            let ipnet = r?;
            tracing::info!("ipnet {ipnet:?}");
        }
        Result::<()>::Ok(())
    };
    let r = fun();

    // let r = tokio::time::timeout(Duration::from_secs(5), async move {
    //     while let Some(r) = watcher.next().await {
    //         let event = r?;
    //         tracing::info!("event {event:?}");
    //     }
    //     Result::<()>::Ok(())
    // }).await;
    tracing::info!("poll ifnet event <== {r:?}");
}
