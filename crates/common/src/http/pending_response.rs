use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
    time::Duration,
};

use http_body_util::{BodyExt, Collected, Full};
use hyper::{
    Request, Response,
    body::{Bytes, Incoming},
    client::conn::http1::{Connection, SendRequest},
};
use mio::{Events, Poll as MioPoll};

use crate::http::transport::{BoxFuture, HyperConn, Transport};

pub enum State {
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
    Collecting {
        conn: HyperConn,
        status: u16,
        body: BoxFuture<Result<Collected<Bytes>, hyper::Error>>,
    },
    Done,
}

pub struct PendingResponse {
    pub state: State,
    pub mio: MioPoll,
    pub events: Events,
}

unsafe fn noop_clone(p: *const ()) -> RawWaker {
    RawWaker::new(p, &NOOP_VT)
}
unsafe fn noop(_: *const ()) {}
static NOOP_VT: RawWakerVTable = RawWakerVTable::new(noop_clone, noop, noop, noop);
pub(crate) fn noop_waker() -> Waker {
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &NOOP_VT)) }
}

impl Future for PendingResponse {
    type Output = Result<(u16, Bytes), Box<dyn std::error::Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let _ = this.mio.poll(&mut this.events, Some(Duration::ZERO));
        loop {
            match std::mem::replace(&mut this.state, State::Done) {
                State::Handshaking { mut handshake, req } => match handshake.as_mut().poll(cx) {
                    Poll::Pending => {
                        this.state = State::Handshaking { handshake, req };
                        return Poll::Pending;
                    }
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e.into())),
                    Poll::Ready(Ok((mut sender, conn))) => {
                        let resp = Box::pin(sender.send_request(req));
                        this.state = State::Responding { conn: Box::pin(conn), resp };
                    }
                },
                State::Responding { mut conn, mut resp } => {
                    if let Poll::Ready(Err(e)) = conn.as_mut().poll(cx) {
                        return Poll::Ready(Err(e.into()));
                    }
                    match resp.as_mut().poll(cx) {
                        Poll::Pending => {
                            this.state = State::Responding { conn, resp };
                            return Poll::Pending;
                        }
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e.into())),
                        Poll::Ready(Ok(response)) => {
                            let status = response.status().as_u16();
                            let body = Box::pin(response.into_body().collect());
                            this.state = State::Collecting { conn, status, body };
                        }
                    }
                }
                State::Collecting { mut conn, status, mut body } => {
                    if let Poll::Ready(Err(e)) = conn.as_mut().poll(cx) {
                        return Poll::Ready(Err(e.into()));
                    }
                    match body.as_mut().poll(cx) {
                        Poll::Pending => {
                            this.state = State::Collecting { conn, status, body };
                            return Poll::Pending;
                        }
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e.into())),
                        Poll::Ready(Ok(collected)) => {
                            return Poll::Ready(Ok((status, collected.to_bytes())))
                        }
                    }
                }
                State::Done => panic!("PendingResponse polled after completion"),
            }
        }
    }
}

impl PendingResponse {
    pub fn poll_bytes(&mut self) -> Poll<Result<(u16, Bytes), Box<dyn std::error::Error>>> {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        Pin::new(self).poll(&mut cx)
    }

    pub fn poll_json<T: serde::de::DeserializeOwned>(
        &mut self,
    ) -> Poll<Result<T, Box<dyn std::error::Error>>> {
        match self.poll_bytes() {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok((status, bytes))) => {
                if !(200..300).contains(&status) {
                    return Poll::Ready(Err(format!("HTTP {status}").into()));
                }
                Poll::Ready(serde_json::from_slice(&bytes).map_err(|e| Box::new(e) as _))
            }
        }
    }
}
