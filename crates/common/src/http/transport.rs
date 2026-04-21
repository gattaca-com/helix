use std::{
    future::Future,
    io::{self, Read, Write},
    pin::Pin,
    task::{Context, Poll},
};

use http_body_util::Full;
use hyper::{body::Bytes, client::conn::http1::Connection};
use mio::net::TcpStream;
use rustls::ClientConnection;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
pub type HyperConn = Pin<Box<Connection<Transport, Full<Bytes>>>>;

// Plain TCP or TLS over TCP, unified for hyper's IO traits.
pub enum Transport {
    Plain(TcpStream),
    Tls(TlsStream),
}

pub struct TlsStream {
    pub tcp: TcpStream,
    pub tls: ClientConnection,
}

impl TlsStream {
    fn flush_tls(&mut self) -> io::Result<()> {
        while self.tls.wants_write() {
            match self.tls.write_tls(&mut self.tcp) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

impl hyper::rt::Read for TlsStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        // Pump encrypted bytes from TCP into rustls.
        loop {
            match this.tls.read_tls(&mut this.tcp) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
        if let Err(e) = this.tls.process_new_packets() {
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e)));
        }
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        let n = unsafe {
            let dst = buf.as_mut();
            let len = dst.len();
            std::ptr::write_bytes(dst.as_mut_ptr().cast::<u8>(), 0, len);
            let slice = std::slice::from_raw_parts_mut(dst.as_mut_ptr().cast::<u8>(), len);
            match this.tls.reader().read(slice) {
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Poll::Pending,
                Err(e) => return Poll::Ready(Err(e)),
            }
        };
        if n > 0 {
            unsafe { buf.advance(n) };
        }
        Poll::Ready(Ok(()))
    }
}

impl hyper::rt::Write for TlsStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let n = match this.tls.writer().write(buf) {
            Ok(n) => n,
            Err(e) => return Poll::Ready(Err(e)),
        };
        Poll::Ready(this.flush_tls().map(|_| n))
    }

    fn poll_flush(self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(self.get_mut().flush_tls())
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl hyper::rt::Read for Transport {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Transport::Plain(tcp) => {
                if buf.remaining() == 0 {
                    return Poll::Ready(Ok(()));
                }
                let n = unsafe {
                    let dst = buf.as_mut();
                    let len = dst.len();
                    std::ptr::write_bytes(dst.as_mut_ptr().cast::<u8>(), 0, len);
                    let slice = std::slice::from_raw_parts_mut(dst.as_mut_ptr().cast::<u8>(), len);
                    match tcp.read(slice) {
                        Ok(n) => n,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Poll::Pending,
                        Err(e) => return Poll::Ready(Err(e)),
                    }
                };
                if n > 0 {
                    unsafe { buf.advance(n) };
                }
                Poll::Ready(Ok(()))
            }
            Transport::Tls(tls) => Pin::new(tls).poll_read(cx, buf),
        }
    }
}

impl hyper::rt::Write for Transport {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Transport::Plain(tcp) => match tcp.write(buf) {
                Ok(n) => Poll::Ready(Ok(n)),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Poll::Pending,
                Err(e) => Poll::Ready(Err(e)),
            },
            Transport::Tls(tls) => Pin::new(tls).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Transport::Plain(tcp) => match tcp.flush() {
                Ok(()) => Poll::Ready(Ok(())),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Poll::Pending,
                Err(e) => Poll::Ready(Err(e)),
            },
            Transport::Tls(tls) => Pin::new(tls).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Transport::Plain(_) => Poll::Ready(Ok(())),
            Transport::Tls(tls) => Pin::new(tls).poll_shutdown(cx),
        }
    }
}
