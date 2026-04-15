use std::{
    pin::Pin,
    task::{Context, Poll, Waker},
    time::Duration,
};

use eventsource_stream::{Event, EventStream, Eventsource};
use futures::stream::Stream;
use http_body_util::Full;
use hyper::{
    Request, Response,
    body::{Body, Bytes, Incoming},
    client::conn::http1::{Connection, SendRequest},
};
use mio::{Events, Poll as MioPoll};

use crate::http::transport::{BoxFuture, HyperConn, Transport};

// --- SSE ---

// Drives the hyper connection alongside the response body, yielding raw byte chunks.
// Sits underneath EventStream which handles the SSE framing.
pub(crate) struct SseChunkStream {
    conn: HyperConn,
    body: Pin<Box<Incoming>>,
}

impl Stream for SseChunkStream {
    type Item = Result<Bytes, hyper::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let _ = this.conn.as_mut().poll(cx);
        match this.body.as_mut().poll_frame(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                Ok(data) => Poll::Ready(Some(Ok(data))),
                Err(_) => Poll::Pending, // trailers
            },
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
        }
    }
}

pub(crate) enum SseState {
    Handshaking {
        handshake: BoxFuture<
            hyper::Result<(SendRequest<Full<Bytes>>, Connection<Transport, Full<Bytes>>)>,
        >,
        req: Request<Full<Bytes>>,
    },
    Responding {
        conn: HyperConn,
        resp: BoxFuture<hyper::Result<Response<Incoming>>>,
    },
    Streaming(Pin<Box<EventStream<SseChunkStream>>>),
    Done,
}

pub(crate) struct SseRequest {
    pub state: SseState,
    pub mio: MioPoll,
    pub events: Events,
    pub waker: Waker,
}

impl SseRequest {
    /// Advance one step. Returns `None` when the server closes the stream (reconnect needed).
    pub fn poll(&mut self) -> Poll<Option<Result<Event, Box<dyn std::error::Error>>>> {
        let mut cx = Context::from_waker(&self.waker);
        let _ = self.mio.poll(&mut self.events, Some(Duration::ZERO));
        loop {
            match std::mem::replace(&mut self.state, SseState::Done) {
                SseState::Handshaking { mut handshake, req } => {
                    match handshake.as_mut().poll(&mut cx) {
                        Poll::Pending => {
                            self.state = SseState::Handshaking { handshake, req };
                            return Poll::Pending;
                        }
                        Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e.into()))),
                        Poll::Ready(Ok((mut sender, conn))) => {
                            let resp = Box::pin(sender.send_request(req));
                            self.state = SseState::Responding { conn: Box::pin(conn), resp };
                        }
                    }
                }
                SseState::Responding { mut conn, mut resp } => {
                    if let Poll::Ready(Err(e)) = conn.as_mut().poll(&mut cx) {
                        return Poll::Ready(Some(Err(e.into())));
                    }
                    match resp.as_mut().poll(&mut cx) {
                        Poll::Pending => {
                            self.state = SseState::Responding { conn, resp };
                            return Poll::Pending;
                        }
                        Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e.into()))),
                        Poll::Ready(Ok(response)) => {
                            let body = Box::pin(response.into_body());
                            let stream = Box::pin(SseChunkStream { conn, body }.eventsource());
                            self.state = SseState::Streaming(stream);
                        }
                    }
                }
                SseState::Streaming(mut stream) => match stream.as_mut().poll_next(&mut cx) {
                    Poll::Pending => {
                        self.state = SseState::Streaming(stream);
                        return Poll::Pending;
                    }
                    Poll::Ready(None) => return Poll::Ready(None),
                    Poll::Ready(Some(Ok(event))) => {
                        self.state = SseState::Streaming(stream);
                        return Poll::Ready(Some(Ok(event)));
                    }
                    Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(Box::new(e)))),
                },
                SseState::Done => panic!("SseRequest::poll called after completion"),
            }
        }
    }
}
