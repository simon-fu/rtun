
use anyhow::Result;
use if_watch::{tokio::IfWatcher, IpNet, IfEvent};

use crate::async_rt::dummy;

pub fn ipnet_iter() -> Result<IpNetIter> {
    Ok(IpNetIter{
        watcher: IfWatcher::new()?,
    })
}

pub struct IpNetIter {
    watcher: IfWatcher,
}

impl Iterator for IpNetIter {
    type Item = Result<IpNet>;

    fn next(&mut self) -> Option<Self::Item> {
        use std::task::Poll;
        let waker = dummy::waker();
        loop {
            let r = self.watcher.poll_if_event(&mut dummy::context(&waker));
            match r {
                Poll::Ready(Ok(IfEvent::Up(ipnet))) => {
                    return Some(Ok(ipnet))
                },
                Poll::Ready(Ok(IfEvent::Down(_ipnet))) => {
                    
                },
                Poll::Ready(Err(e)) => return Some(Err(e.into())),
                Poll::Pending => return None
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
        tracing::info!("No.{} ifnet {ifnet:?}", n+1, );
    }
    tracing::info!("ifnet list: <==");

    tracing::info!("poll ifnet event ==>");

    let fun = ||  {
        let iter = ipnet_iter()?;
        for r in iter {
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

