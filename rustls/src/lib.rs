#![deny(missing_docs)]
#![doc(html_root_url = "https://docs.rs/lapin-futures-rustls/0.4.0/")]

//! lapin-futures-rustls
//!
//! This library offers a nice integration of `rustls` with the `lapin-futures` library.
//! It uses `amq-protocol` URI parsing feature and adds a `connect` method to `AMQPUri`
//! which will provide you with a `lapin_futures::client::Client` wrapped in a `Future`.
//!
//! It autodetects whether you're using `amqp` or `amqps` and opens either a raw `TcpStream`
//! or a `TlsStream` using `rustls` as the SSL engine.
//!
//! ## Connecting and opening a channel
//!
//! ```rust,no_run
//! extern crate amq_protocol;
//! extern crate env_logger;
//! extern crate futures;
//! extern crate lapin_futures_rustls;
//! extern crate tokio_core;
//!
//! use amq_protocol::uri::AMQPUri;
//! use futures::future::Future;
//! use lapin_futures_rustls::AMQPConnectionExt;
//! use tokio_core::reactor::Core;
//!
//! fn main() {
//!     env_logger::init().unwrap();
//!
//!     let uri      = "amqps://user:pass@host/vhost?heartbeat=10".parse::<AMQPUri>().unwrap();
//!     let mut core = Core::new().unwrap();
//!     let handle   = core.handle();
//!
//!     core.run(
//!         uri.connect(&handle).and_then(|client| {
//!             println!("Connected!");
//!             client.create_confirm_channel()
//!         }).and_then(|channel| {
//!             println!("Closing channel.");
//!             channel.close(200, "Bye".to_string())
//!         })
//!     ).unwrap();
//! }
//! ```

extern crate amq_protocol;
extern crate bytes;
extern crate futures;
extern crate lapin_futures;
extern crate rustls;
extern crate tokio_core;
extern crate tokio_io;
extern crate tokio_rustls;
extern crate webpki_roots;

/// Reexport of the `lapin_futures` crate
pub mod lapin;
/// Reexport of the `uri` module from the `amq_protocol` crate
pub mod uri;

use std::io::{self, Read, Write};
use std::sync::Arc;

use bytes::{Buf, BufMut};
use futures::future::Future;
use futures::Poll;
use tokio_core::net::TcpStream;
use tokio_core::reactor::Handle;
use tokio_io::{AsyncRead, AsyncWrite};
use tokio_rustls::{ClientConfigExt, TlsStream};

use lapin::client::ConnectionOptions;
use uri::{AMQPQueryString, AMQPScheme, AMQPUri, AMQPUserInfo};

/// Represents either a raw `TcpStream` or a `TlsStream` backend by `rustls`.
/// The `TlsStream` is wrapped in a `Box` to keep the enum footprint minimal.
pub enum AMQPStream {
    /// The raw `TcpStream` used for basic AMQP connections.
    Raw(TcpStream),
    /// The `TlsStream` used for AMQPs connections.
    Tls(Box<TlsStream<TcpStream, rustls::ClientSession>>),
}

/// Add a connect method providing a `lapin_futures::client::Client` wrapped in a `Future`.
pub trait AMQPConnectionExt {
    /// Method providing a `lapin_futures::client::Client` wrapped in a `Future`
    /// using a `tokio_code::reactor::Handle`.
    fn connect(&self, handle: &Handle) -> Box<Future<Item = lapin::client::Client<AMQPStream>, Error = io::Error> + 'static>;
}

impl AMQPConnectionExt for AMQPUri {
    fn connect(&self, handle: &Handle) -> Box<Future<Item = lapin::client::Client<AMQPStream>, Error = io::Error> + 'static> {
        let userinfo = self.authority.userinfo.clone();
        let vhost    = self.vhost.clone();
        let query    = self.query.clone();
        let stream   = match self.scheme {
            AMQPScheme::AMQP  => AMQPStream::raw(handle, &self.authority.host, self.authority.port),
            AMQPScheme::AMQPS => AMQPStream::tls(handle, &self.authority.host, self.authority.port),
        };

        Box::new(stream.and_then(move |stream| connect_stream(stream, userinfo, vhost, &query)))
    }
}

impl AMQPConnectionExt for str {
    fn connect(&self, handle: &Handle) -> Box<Future<Item = lapin::client::Client<AMQPStream>, Error = io::Error> + 'static> {
        match self.parse::<AMQPUri>() {
            Ok(uri)  => uri.connect(handle),
            Err(err) => Box::new(futures::future::err(io::Error::new(io::ErrorKind::Other, err))),
        }
    }
}

impl AMQPStream {
    fn raw(handle: &Handle, host: &str, port: u16) -> Box<Future<Item = Self, Error = io::Error> + 'static> {
        match open_tcp_stream(handle, host, port) {
            Ok(stream) => Box::new(futures::future::ok(AMQPStream::Raw(stream))),
            Err(e)     => Box::new(futures::future::err(e)),
        }
    }

    fn tls(handle: &Handle, host: &str, port: u16) -> Box<Future<Item = Self, Error = io::Error> + 'static> {
        let mut config = rustls::ClientConfig::new();
        config.root_store.add_trust_anchors(&webpki_roots::ROOTS);
        let config     = Arc::new(config);

        match open_tcp_stream(handle, host, port) {
            Ok(stream) => Box::new(config.connect_async(host, stream).map(Box::new).map(AMQPStream::Tls)),
            Err(e)     => Box::new(futures::future::err(e)),
        }
    }
}

impl Read for AMQPStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match *self {
            AMQPStream::Raw(ref mut raw) => raw.read(buf),
            AMQPStream::Tls(ref mut tls) => tls.read(buf),
        }
    }
}

impl AsyncRead for AMQPStream {
    unsafe fn prepare_uninitialized_buffer(&self, buf: &mut [u8]) -> bool {
        match *self {
            AMQPStream::Raw(ref raw) => raw.prepare_uninitialized_buffer(buf),
            AMQPStream::Tls(ref tls) => tls.prepare_uninitialized_buffer(buf),
        }
    }

    fn read_buf<B: BufMut>(&mut self, buf: &mut B) -> Poll<usize, io::Error> {
        match *self {
            AMQPStream::Raw(ref mut raw) => raw.read_buf(buf),
            AMQPStream::Tls(ref mut tls) => tls.read_buf(buf),
        }
    }
}

impl Write for AMQPStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match *self {
            AMQPStream::Raw(ref mut raw) => raw.write(buf),
            AMQPStream::Tls(ref mut tls) => tls.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match *self {
            AMQPStream::Raw(ref mut raw) => raw.flush(),
            AMQPStream::Tls(ref mut tls) => tls.flush(),
        }
    }
}

impl AsyncWrite for AMQPStream {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        match *self {
            AMQPStream::Raw(ref mut raw) => raw.shutdown(),
            AMQPStream::Tls(ref mut tls) => tls.shutdown(),
        }
    }

    fn write_buf<B: Buf>(&mut self, buf: &mut B) -> Poll<usize, io::Error> {
        match *self {
            AMQPStream::Raw(ref mut raw) => raw.write_buf(buf),
            AMQPStream::Tls(ref mut tls) => tls.write_buf(buf),
        }
    }
}

fn open_tcp_stream(handle: &Handle, host: &str, port: u16) -> io::Result<TcpStream> {
    std::net::TcpStream::connect((host, port)).and_then(|stream| TcpStream::from_stream(stream, handle))
}

fn connect_stream<T: AsyncRead + AsyncWrite + 'static>(stream: T, credentials: AMQPUserInfo, vhost: String, query: &AMQPQueryString) -> Box<Future<Item = lapin::client::Client<T>, Error = io::Error> + 'static> {
    let defaults = ConnectionOptions::default();
    Box::new(lapin::client::Client::connect(stream, &ConnectionOptions {
        username:  credentials.username,
        password:  credentials.password,
        vhost:     vhost,
        frame_max: query.frame_max.unwrap_or_else(|| defaults.frame_max),
        heartbeat: query.heartbeat.unwrap_or_else(|| defaults.heartbeat),
    }))
}