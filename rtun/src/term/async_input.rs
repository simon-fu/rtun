use anyhow::{Context, Result};
use futures::{Stream, StreamExt};
use std::{
    pin::Pin,
    task::{self, Poll},
};
use tokio::signal::unix::Signal;

use crate::{
    async_stdin::{async_std_in, AsyncStdin},
    pty::{PtyEvent, PtySize},
};

pub fn make_async_input() -> Result<AsynInput> {
    Ok(AsynInput {
        size_signal: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?,
        stdin: async_std_in(),
    })
}

pub struct AsynInput {
    size_signal: Signal,
    stdin: AsyncStdin,
}

impl Stream for AsynInput {
    type Item = Result<PtyEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        // tracing::debug!("AsynInput::poll start\r");

        if let Poll::Ready(Some(_r)) = self.size_signal.poll_recv(cx) {
            // tracing::debug!("AsynInput::poll has resize event\r");

            let mut size = get_term_size();

            while let Poll::Ready(Some(_r)) = self.size_signal.poll_recv(cx) {
                size = get_term_size();
            }

            let r = size
                .map(|x| PtyEvent::Resize(x))
                .with_context(|| "can't get terminal size");

            // tracing::debug!("AsynInput::poll return resize {:?}\r", r);
            return Poll::Ready(Some(r));
        }

        let r = self.stdin.poll_next_unpin(cx);
        // tracing::debug!("AsynInput::poll stdin result {:?}\r", r);
        match r {
            Poll::Ready(r) => {
                let r = r.map(|x| x.map(|y| PtyEvent::StdinData(y)).map_err(|e| e.into()));
                // tracing::debug!("AsynInput::poll return stdin {:?}\r", r);
                return Poll::Ready(r);
            }
            Poll::Pending => {
                // tracing::debug!("AsynInput::poll return Pending\r");
                return Poll::Pending;
            }
        }
    }
}

pub fn get_term_size() -> Option<PtySize> {
    term_size::dimensions().map(|x| PtySize {
        cols: x.0 as u16,
        rows: x.1 as u16,
    })
}

// use anyhow::{Result, Context};
// use bytes::BytesMut;
// use std::{task::{self, Poll}, pin::Pin, ops::DerefMut};
// use futures::Stream;
// use tokio::{signal::unix::Signal, io::{AsyncRead, Stdin}};

// use crate::pty::{PtyEvent, PtySize};

// pub fn make_async_input() -> Result<AsynInput> {
//     Ok(AsynInput {
//         size_signal: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?,
//         stdin: tokio::io::stdin(),
//         buf: BytesMut::new(),
//      })
// }

// pub struct AsynInput {
//     size_signal: Signal,
//     stdin: Stdin,
//     buf: BytesMut,
// }

// impl Stream for AsynInput {
//     type Item = Result<PtyEvent>;

//     fn poll_next(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {

//         if let Poll::Ready(Some(_r)) = self.size_signal.poll_recv(cx) {

//             let mut size = get_term_size();

//             while let Poll::Ready(Some(_r)) = self.size_signal.poll_recv(cx) {
//                 size = get_term_size();
//             }

//             let r = size
//             .map(|x|PtyEvent::Resize(x))
//             .with_context(||"can't get terminal size");

//             return Poll::Ready(Some(r))
//         }

//         if self.buf.len() == 0 {
//             if self.buf.capacity() == 0 {
//                 self.buf = BytesMut::with_capacity(4*1024);
//             }

//             let cap = self.buf.capacity();
//             self.buf.resize(cap, 0);
//         }

//         let self0 = self.deref_mut();
//         let stdin = &mut self0.stdin;
//         let buf = &mut self0.buf;

//         let mut rbuf = tokio::io::ReadBuf::new(buf);

//         let r = Pin::new(stdin).poll_read(cx, &mut rbuf);
//         match r {
//             Poll::Ready(r) => {
//                 match r {
//                     Ok(_v) => {
//                         let n = rbuf.filled().len();
//                         let data = buf.split_to(n).freeze();
//                         Poll::Ready(Some(Ok(PtyEvent::StdinData(data))))
//                     },
//                     Err(e) => Poll::Ready(Some(Err(e.into()))),
//                 }
//             },
//             Poll::Pending => Poll::Pending,
//         }

//     }
// }

// pub fn get_term_size() -> Option<PtySize> {
//     term_size::dimensions().map(|x|PtySize {
//         cols: x.0 as u16,
//         rows: x.1 as u16,
//     })
// }
