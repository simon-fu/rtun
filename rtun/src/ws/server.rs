use std::{task::{self, Poll}, pin::Pin};

use anyhow::{Result, anyhow};
use futures::{StreamExt, Sink, SinkExt, Stream, stream::{SplitSink, SplitStream}};
use protobuf::Message as PbMessage;

use crate::{proto::RawPacket, channel::ChPacket, switch::{switch_source::{PacketSource, StreamPacket, StreamError}, switch_sink::{PacketSink, SinkError}}};

// use tokio_tungstenite::{connect_async, tungstenite::{Message as WsMessage, Error as WsError}, WebSocketStream, MaybeTlsStream};

use axum::{ Error as WsError, extract::ws::Message as WsMessage};

// WsStreamAxum

pub struct WsStreamAxum<S> {
    sink: WsSink<SplitSink<S, WsMessage>>,
    source: WsSource<SplitStream<S>>,
}

impl<S> WsStreamAxum<S> {
    pub fn new((sink, source): (SplitSink<S, WsMessage>, SplitStream<S>)) -> Self {
        Self {
            sink: WsSink(sink),
            source: WsSource(source),
        }
    }
}

impl<S> WsStreamAxum<S> {
    pub fn split(self) -> (WsSink<SplitSink<S, WsMessage>>, WsSource<SplitStream<S>>) {
        (self.sink, self.source)
    }
}

impl<S> Stream for WsStreamAxum<S> 
where 
    S: StreamExt<Item = Result<WsMessage, WsError>> 
        + Unpin,
{
    type Item = Result<StreamPacket, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        self.source.poll_next_unpin(cx)
    }
}

impl<S> Sink<ChPacket> for WsStreamAxum<S> 
where
    S: Sink<WsMessage, Error = WsError> + Unpin,
{
    type Error = SinkError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.sink.poll_ready_unpin(cx).map_err(|e|e.into())
    }

    fn start_send(mut self: Pin<&mut Self>, item: ChPacket) -> Result<(), Self::Error> {
        self.sink.start_send_unpin(item).map_err(|e|e.into())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.sink.poll_flush_unpin(cx).map_err(|e|e.into())
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.sink.poll_close_unpin(cx).map_err(|e|e.into())
    }

}

// impl<S> PacketStream for WsStreamAxum<S> 
// where
//     S: 'static
//     + Unpin
//     + Send
//     + StreamExt<Item = Result<WsMessage, WsError>> 
//     + Sink<WsMessage, Error = WsError>
// { }



pub struct WsSource<S>(pub S);

impl<S> Stream for WsSource<S> 
where 
    S: StreamExt<Item = Result<WsMessage, WsError>> 
        + Unpin,
{
    type Item = Result<StreamPacket, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            let r = match self.0.poll_next_unpin(cx) {
                Poll::Ready(v) => v,
                Poll::Pending => return Poll::Pending,
            };
    
            let r = match r {
                Some(r) => r,
                None => return Poll::Ready(None),
            };
    
            let msg = match r {
                Ok(r) => r,
                Err(e) => return Poll::Ready(Some(Err(e.into()))),
            };
    
            match msg {
                WsMessage::Text(s) => return Poll::Ready(Some(Err(anyhow!("got msg::text {s:?}").into()))),
                WsMessage::Close(c) => return Poll::Ready(Some(Err(anyhow!("got msg::close {c:?}")))),
                // WsMessage::Frame(_v) => return Poll::Ready(Some(Err(anyhow!("got msg::frame")))),
                WsMessage::Ping(_ping) => {}, // bail!("got msg::ping"),
                WsMessage::Pong(_pong) => {}, // bail!("got msg::pong"),
                WsMessage::Binary(d) => {
                    return Poll::Ready(Some(Ok(d)))
                },
            }
        }
        
    }
}

impl<S> PacketSource for WsSource<S> 
where
    S: 'static
    + Unpin
    + Send
    + StreamExt<Item = Result<WsMessage, WsError>> 
{ }

pub struct WsSink<S>(pub S);

impl<S> Sink<ChPacket> for WsSink<S> 
where
    S: Sink<WsMessage, Error = WsError> + Unpin,
{
    type Error = SinkError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready_unpin(cx).map_err(|e|e.into())
    }

    fn start_send(mut self: Pin<&mut Self>, item: ChPacket) -> Result<(), Self::Error> {
        let data = RawPacket {
            ch_id: item.ch_id.0,
            payload: item.payload,
            ..Default::default()
        }.write_to_bytes()?;
        self.0.start_send_unpin(WsMessage::Binary(data)).map_err(|e|e.into())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_flush_unpin(cx).map_err(|e|e.into())
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_close_unpin(cx).map_err(|e|e.into())
    }

}

impl<S> PacketSink for WsSink<S> 
where
    S: 'static
    + Unpin
    + Send
    + Sink<WsMessage, Error = WsError>
{ }


















// use std::{task::{self, Poll}, pin::Pin};

// use anyhow::{Result, anyhow};
// use futures::{StreamExt, Sink, SinkExt, Stream};
// use protobuf::Message as PbMessage;

// use crate::{proto::RawPacket, channel::ChPacket, switch::switch_stream::{SinkError, StreamPacket, StreamError, PacketStream}};

// // use tokio_tungstenite::{connect_async, tungstenite::{Message as WsMessage, Error as WsError}, WebSocketStream, MaybeTlsStream};

// use axum::{ Error as WsError, extract::ws::Message as WsMessage};

// pub struct WsStreamAxum<S>(pub S);

// impl<S> WsStreamAxum<S> {
//     pub fn new(socket: S) -> Self {
//         Self(socket)
//     }
// }

// impl<S> Stream for WsStreamAxum<S> 
// where 
//     S: StreamExt<Item = Result<WsMessage, WsError>> 
//         + Unpin,
// {
//     type Item = Result<StreamPacket, StreamError>;

//     fn poll_next(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
//         loop {
//             let r = match self.0.poll_next_unpin(cx) {
//                 Poll::Ready(v) => v,
//                 Poll::Pending => return Poll::Pending,
//             };
    
//             let r = match r {
//                 Some(r) => r,
//                 None => return Poll::Ready(None),
//             };
    
//             let msg = match r {
//                 Ok(r) => r,
//                 Err(e) => return Poll::Ready(Some(Err(e.into()))),
//             };
    
//             match msg {
//                 WsMessage::Text(s) => return Poll::Ready(Some(Err(anyhow!("got msg::text {s:?}").into()))),
//                 WsMessage::Close(c) => return Poll::Ready(Some(Err(anyhow!("got msg::close {c:?}")))),
//                 // WsMessage::Frame(_v) => return Poll::Ready(Some(Err(anyhow!("got msg::frame")))),
//                 WsMessage::Ping(_ping) => {}, // bail!("got msg::ping"),
//                 WsMessage::Pong(_pong) => {}, // bail!("got msg::pong"),
//                 WsMessage::Binary(d) => {
//                     return Poll::Ready(Some(Ok(d)))
//                 },
//             }
//         }
        
//     }
// }

// impl<S> Sink<ChPacket> for WsStreamAxum<S> 
// where
//     S: Sink<WsMessage, Error = WsError> + Unpin,
// {
//     type Error = SinkError;

//     fn poll_ready(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
//         self.0.poll_ready_unpin(cx).map_err(|e|e.into())
//     }

//     fn start_send(mut self: Pin<&mut Self>, item: ChPacket) -> Result<(), Self::Error> {
//         let data = RawPacket {
//             ch_id: item.ch_id.0,
//             payload: item.payload,
//             ..Default::default()
//         }.write_to_bytes()?;
//         self.0.start_send_unpin(WsMessage::Binary(data)).map_err(|e|e.into())
//     }

//     fn poll_flush(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
//         self.0.poll_flush_unpin(cx).map_err(|e|e.into())
//     }

//     fn poll_close(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
//         self.0.poll_close_unpin(cx).map_err(|e|e.into())
//     }

// }

// impl<S> PacketStream for WsStreamAxum<S> 
// where
//     S: 'static
//     + Unpin
//     + Send
//     + StreamExt<Item = Result<WsMessage, WsError>> 
//     + Sink<WsMessage, Error = WsError>
// { }

