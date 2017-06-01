use std::io::Cursor;
use std::path::Path;
use std::collections::HashMap;
use std::sync::{Arc,Mutex};
use ::xdr_codec::{Pack,Unpack};
use ::bytes::{BufMut, BytesMut};
use ::tokio_io::codec;
use ::tokio_io::{AsyncRead, AsyncWrite};
use ::tokio_io::codec::length_delimited;
use ::tokio_proto::multiplex::{self, RequestId};
use ::tokio_service::Service;
use ::request;
use ::LibvirtError;
use ::futures::{Stream, Sink, Poll, StartSend, Future, future};

struct LibvirtCodec;

#[derive(Debug,Clone)]
pub struct LibvirtRequest {
    pub header: request::virNetMessageHeader,
    pub payload: BytesMut,
}

#[derive(Debug,Clone)]
pub struct LibvirtResponse {
    pub header: request::virNetMessageHeader,
    pub payload: BytesMut,
}

impl codec::Encoder for LibvirtCodec {
    type Item = (RequestId, LibvirtRequest);
    type Error = ::std::io::Error;

    fn encode(&mut self, msg: (RequestId, LibvirtRequest), buf: &mut BytesMut) -> Result<(), Self::Error> {
        use ::std::io::ErrorKind;
        let mut req = msg.1;
        let buf = {
            let mut writer = buf.writer();
            req.header.serial = msg.0 as u32;
            try!(req.header.pack(&mut writer).map_err(|e| ::std::io::Error::new(ErrorKind::InvalidInput, e.to_string())));
            writer.into_inner()
        };
        buf.reserve(req.payload.len());
        buf.put(req.payload);
        Ok(())
    }
}

impl codec::Decoder for LibvirtCodec {
    type Item = (RequestId, LibvirtResponse);
    type Error = ::std::io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        use ::std::io::ErrorKind;
        let (header, hlen, buf) = {
            let mut reader = Cursor::new(buf);
            let (header, hlen) = try!(request::virNetMessageHeader::unpack(&mut reader)
                                        .map_err(|e| ::std::io::Error::new(ErrorKind::InvalidInput, e.to_string())));
            (header, hlen, reader.into_inner())
        };
        let payload = buf.split_off(hlen);
        Ok(Some((header.serial as RequestId, LibvirtResponse {
            header: header,
            payload: payload,
        })))
    }
}

fn framed_delimited<T, C>(framed: length_delimited::Framed<T>, codec: C) -> FramedTransport<T, C>
    where T: AsyncRead + AsyncWrite, C: codec::Encoder + codec::Decoder
 {
    FramedTransport{ inner: framed, codec: codec }
}

struct FramedTransport<T, C> where T: AsyncRead + AsyncWrite + 'static {
    inner: length_delimited::Framed<T>,
    codec: C,
}

impl<T, C> Stream for FramedTransport<T, C> where
                T: AsyncRead + AsyncWrite, C: codec::Decoder,
                ::std::io::Error: ::std::convert::From<<C as ::tokio_io::codec::Decoder>::Error> {
    type Item = <C as codec::Decoder>::Item;
    type Error = <C as codec::Decoder>::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        use futures::Async;
        let codec = &mut self.codec;
        self.inner.poll().and_then(|async| {
            match async {
                Async::Ready(Some(mut buf)) => {
                    let pkt = try!(codec.decode(&mut buf));
                    Ok(Async::Ready(pkt))
                },
                Async::Ready(None) => {
                    Ok(Async::Ready(None))
                },
                Async::NotReady => {
                    Ok(Async::NotReady)
                }
            }
        }).map_err(|e| e.into())
    }
}

impl<T, C> Sink for FramedTransport<T, C> where
        T: AsyncRead + AsyncWrite + 'static,
        C: codec::Encoder + codec::Decoder,
        ::std::io::Error: ::std::convert::From<<C as ::tokio_io::codec::Encoder>::Error> {
    type SinkItem = <C as codec::Encoder>::Item;
    type SinkError = <C as codec::Encoder>::Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        use futures::AsyncSink;
        let codec = &mut self.codec;
        let mut buf = BytesMut::with_capacity(64);
        try!(codec.encode(item, &mut buf));
        assert!(try!(self.inner.start_send(buf)).is_ready());
        Ok(AsyncSink::Ready)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.inner.poll_complete().map_err(|e| e.into())
    }

    fn close(&mut self) -> Poll<(), Self::SinkError> {
        try_ready!(self.poll_complete().map_err(|e| e.into()));
        self.inner.close().map_err(|e| e.into())
    }
}

pub struct LibvirtTransport<T> where T: AsyncRead + AsyncWrite + 'static {
    inner: FramedTransport<T, LibvirtCodec>,
    events: Arc<Mutex<HashMap<i32, ::futures::sync::mpsc::Sender<::request::DomainEvent>>>>,
}

impl<T> LibvirtTransport<T> where T: AsyncRead + AsyncWrite + 'static {
    fn process_event(&self, resp: &LibvirtResponse) -> ::std::io::Result<bool> {
        let procedure = unsafe { ::std::mem::transmute(resp.header.proc_ as u16) };
        match procedure {
            request::remote_procedure::REMOTE_PROC_DOMAIN_EVENT_CALLBACK_LIFECYCLE => {
                let msg = {
                    let mut cursor = Cursor::new(&resp.payload);
                    let (msg, _) = request::generated::remote_domain_event_callback_lifecycle_msg::unpack(&mut cursor).unwrap();
                    debug!("LIFECYCLE EVENT (CALLBACK) PL: {:?}", msg);
                    msg
                };
                {
                    let mut map = self.events.lock().unwrap();
                    if let Some(sender) = map.get_mut(&msg.callbackID) {
                        use std::io::ErrorKind;
                        try!(sender.start_send(msg.into()).map_err(|e| ::std::io::Error::new(ErrorKind::InvalidInput, e.to_string())));
                        try!(sender.poll_complete().map_err(|e| ::std::io::Error::new(ErrorKind::InvalidInput, e.to_string())));
                    }
                }
                return Ok(true);
            },
            _ => {
                debug!("unknown procedure {:?} in {:?}", procedure, resp);
            },
        }
        Ok(false)
    }

    fn process_stream(&self, resp: &LibvirtResponse) -> bool {
        if resp.header.type_ == request::generated::virNetMessageType::VIR_NET_STREAM {
            println!("STREAM {:?}", resp);
            return true;
        }
        false
    }
}

impl<T> Stream for LibvirtTransport<T> where
    T: AsyncRead + AsyncWrite + 'static,
 {
    type Item = (RequestId, LibvirtResponse);
    type Error = ::std::io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        use futures::Async;
        match self.inner.poll() {
            Ok(async) => {
                match async {
                Async::Ready(Some((id, ref resp))) => {
                    debug!("FRAME READY ID: {} RESP: {:?}", id, resp);
                    if try!(self.process_event(resp)) {
                            debug!("processed event, get next packet");
                            return self.poll();
                    }

                    if self.process_stream(resp) {
                        debug!("processed stream msg, get next packet");
                        return self.poll();
                    }
                },
                _ => debug!("{:?}", async),
                }
                debug!("RETURNING {:?}", async);
                Ok(async)
            },
            Err(e) => Err(e),
        }
    }
}

impl<T> Sink for LibvirtTransport<T> where
    T: AsyncRead + AsyncWrite + 'static,
 {
    type SinkItem = (RequestId, LibvirtRequest);
    type SinkError = ::std::io::Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        self.inner.start_send(item)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.inner.poll_complete()
    }

    fn close(&mut self) -> Poll<(), Self::SinkError> {
        self.inner.close()
    }
}

#[derive(Debug, Clone)]
pub struct LibvirtProto {
    pub events: Arc<Mutex<HashMap<i32, ::futures::sync::mpsc::Sender<::request::DomainEvent>>>>,
}

impl<T> multiplex::ClientProto<T> for LibvirtProto where T: AsyncRead + AsyncWrite + 'static {
    type Request = LibvirtRequest;
    type Response = LibvirtResponse;
    type Transport = LibvirtTransport<T>;
    type BindTransport = Result<Self::Transport, ::std::io::Error>;

    fn bind_transport(&self, io: T) -> Self::BindTransport {
        let framed = length_delimited::Builder::new()
                        .big_endian()
                        .length_field_offset(0)
                        .length_field_length(4)
                        .length_adjustment(-4)
                        .new_framed(io);
        Ok(LibvirtTransport{ 
            inner: framed_delimited(framed, LibvirtCodec),
            events: self.events.clone(),
        })
    }
}

pub struct EventStream<T> {
    pub inner: ::futures::sync::mpsc::Receiver<T>,
}

impl<T> Stream for EventStream<T> {
    type Item = T;
    type Error = ();

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.inner.poll()
    }
}

pub struct LibvirtStream<T> {
    pub inner: ::futures::sync::mpsc::Receiver<T>,
}

impl<T> Stream for LibvirtStream<T> {
    type Item = T;
    type Error = ();

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.inner.poll()
    }
}

