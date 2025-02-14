use std::io;
use std::sync::Arc;
use std::task::{Context, Poll};
use tracing::warn;

use async_trait::async_trait;
use celestia_proto::p2p::pb::{HeaderRequest, HeaderResponse};
use celestia_types::ExtendedHeader;
use futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use libp2p::{
    core::Endpoint,
    request_response::{self, Codec, InboundFailure, OutboundFailure, ProtocolSupport},
    swarm::{
        ConnectionDenied, ConnectionId, FromSwarm, NetworkBehaviour, PollParameters,
        THandlerInEvent, THandlerOutEvent, ToSwarm,
    },
    Multiaddr, PeerId, StreamProtocol,
};
use prost::Message;
use tracing::debug;
use tracing::instrument;

mod client;
mod server;
mod utils;

use crate::header_ex::client::HeaderExClientHandler;
use crate::header_ex::server::HeaderExServerHandler;
use crate::p2p::P2pError;
use crate::peer_tracker::PeerTracker;
use crate::store::Store;
use crate::utils::{protocol_id, OneshotResultSender};

/// Max request size in bytes
const REQUEST_SIZE_MAXIMUM: usize = 1024;
/// Max response size in bytes
const RESPONSE_SIZE_MAXIMUM: usize = 10 * 1024 * 1024;

type RequestType = HeaderRequest;
type ResponseType = Vec<HeaderResponse>;
type ReqRespBehaviour = request_response::Behaviour<HeaderCodec>;
type ReqRespEvent = request_response::Event<RequestType, ResponseType>;
type ReqRespMessage = request_response::Message<RequestType, ResponseType>;

pub(crate) struct HeaderExBehaviour<S>
where
    S: Store + 'static,
{
    req_resp: ReqRespBehaviour,
    client_handler: HeaderExClientHandler,
    server_handler: HeaderExServerHandler<S>,
}

pub(crate) struct HeaderExConfig<'a, S> {
    pub network_id: &'a str,
    pub peer_tracker: Arc<PeerTracker>,
    pub header_store: Arc<S>,
}

#[derive(Debug, thiserror::Error)]
pub enum HeaderExError {
    #[error("Header not found")]
    HeaderNotFound,

    #[error("Invalid response")]
    InvalidResponse,

    #[error("Invalid request")]
    InvalidRequest,

    #[error("Inbound failure: {0}")]
    InboundFailure(InboundFailure),

    #[error("Outbound failure: {0}")]
    OutboundFailure(OutboundFailure),
}

impl<S> HeaderExBehaviour<S>
where
    S: Store + 'static,
{
    pub(crate) fn new(config: HeaderExConfig<'_, S>) -> Self {
        HeaderExBehaviour {
            req_resp: ReqRespBehaviour::new(
                [(
                    protocol_id(config.network_id, "/header-ex/v0.0.3"),
                    ProtocolSupport::Full,
                )],
                request_response::Config::default(),
            ),
            client_handler: HeaderExClientHandler::new(config.peer_tracker),
            server_handler: HeaderExServerHandler::new(config.header_store),
        }
    }

    #[instrument(level = "trace", skip(self, respond_to))]
    pub(crate) fn send_request(
        &mut self,
        request: HeaderRequest,
        respond_to: OneshotResultSender<Vec<ExtendedHeader>, P2pError>,
    ) {
        self.client_handler
            .on_send_request(&mut self.req_resp, request, respond_to);
    }

    fn on_to_swarm(
        &mut self,
        ev: ToSwarm<ReqRespEvent, THandlerInEvent<ReqRespBehaviour>>,
    ) -> Option<ToSwarm<(), THandlerInEvent<Self>>> {
        match ev {
            ToSwarm::GenerateEvent(ev) => {
                self.on_req_resp_event(ev);
                None
            }
            _ => Some(ev.map_out(|_| ())),
        }
    }

    #[instrument(level = "trace", skip_all)]
    fn on_req_resp_event(&mut self, ev: ReqRespEvent) {
        match ev {
            // Received a response for an ongoing outbound request
            ReqRespEvent::Message {
                message:
                    ReqRespMessage::Response {
                        request_id,
                        response,
                    },
                peer,
            } => {
                self.client_handler
                    .on_response_received(peer, request_id, response);
            }

            // Failure while client requests
            ReqRespEvent::OutboundFailure {
                peer,
                request_id,
                error,
            } => {
                self.client_handler.on_failure(peer, request_id, error);
            }

            // Received new inbound request
            ReqRespEvent::Message {
                message:
                    ReqRespMessage::Request {
                        request_id,
                        request,
                        channel,
                    },
                peer,
            } => {
                self.server_handler
                    .on_request_received(peer, request_id, request, channel);
            }

            // Response to inbound request was sent
            ReqRespEvent::ResponseSent { peer, request_id } => {
                self.server_handler.on_response_sent(peer, request_id);
            }

            // Failure while server responds
            ReqRespEvent::InboundFailure {
                peer,
                request_id,
                error,
            } => {
                self.server_handler.on_failure(peer, request_id, error);
            }
        }
    }
}

impl<S> NetworkBehaviour for HeaderExBehaviour<S>
where
    S: Store + 'static,
{
    type ConnectionHandler = <ReqRespBehaviour as NetworkBehaviour>::ConnectionHandler;
    type ToSwarm = ();

    fn handle_established_inbound_connection(
        &mut self,
        connection_id: ConnectionId,
        peer: PeerId,
        local_addr: &Multiaddr,
        remote_addr: &Multiaddr,
    ) -> Result<Self::ConnectionHandler, ConnectionDenied> {
        self.req_resp.handle_established_inbound_connection(
            connection_id,
            peer,
            local_addr,
            remote_addr,
        )
    }

    fn handle_established_outbound_connection(
        &mut self,
        connection_id: ConnectionId,
        peer: PeerId,
        addr: &Multiaddr,
        role_override: Endpoint,
    ) -> Result<Self::ConnectionHandler, ConnectionDenied> {
        self.req_resp.handle_established_outbound_connection(
            connection_id,
            peer,
            addr,
            role_override,
        )
    }

    fn handle_pending_inbound_connection(
        &mut self,
        connection_id: ConnectionId,
        local_addr: &Multiaddr,
        remote_addr: &Multiaddr,
    ) -> Result<(), ConnectionDenied> {
        self.req_resp
            .handle_pending_inbound_connection(connection_id, local_addr, remote_addr)
    }

    fn handle_pending_outbound_connection(
        &mut self,
        connection_id: ConnectionId,
        maybe_peer: Option<PeerId>,
        addresses: &[Multiaddr],
        effective_role: Endpoint,
    ) -> Result<Vec<Multiaddr>, ConnectionDenied> {
        self.req_resp.handle_pending_outbound_connection(
            connection_id,
            maybe_peer,
            addresses,
            effective_role,
        )
    }

    fn on_swarm_event(&mut self, event: FromSwarm<'_, Self::ConnectionHandler>) {
        self.req_resp.on_swarm_event(event)
    }

    fn on_connection_handler_event(
        &mut self,
        peer_id: PeerId,
        connection_id: ConnectionId,
        event: THandlerOutEvent<Self>,
    ) {
        self.req_resp
            .on_connection_handler_event(peer_id, connection_id, event)
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
        params: &mut impl PollParameters,
    ) -> Poll<ToSwarm<Self::ToSwarm, THandlerInEvent<Self>>> {
        loop {
            if let Poll::Ready(ev) = self.req_resp.poll(cx, params) {
                if let Some(ev) = self.on_to_swarm(ev) {
                    return Poll::Ready(ev);
                }

                continue;
            }

            if self.client_handler.poll(cx).is_ready() {
                continue;
            }

            if self.server_handler.poll(cx, &mut self.req_resp).is_ready() {
                continue;
            }

            return Poll::Pending;
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct HeaderCodec;

#[async_trait]
impl Codec for HeaderCodec {
    type Protocol = StreamProtocol;
    type Request = HeaderRequest;
    type Response = Vec<HeaderResponse>;

    async fn read_request<T>(&mut self, _: &Self::Protocol, io: &mut T) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        let data = read_up_to(io, REQUEST_SIZE_MAXIMUM).await?;

        if data.len() >= REQUEST_SIZE_MAXIMUM {
            debug!("Message filled the whole buffer (len: {})", data.len());
        }

        parse_header_request(&data)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "invalid request"))
    }

    async fn read_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        let data = read_up_to(io, RESPONSE_SIZE_MAXIMUM).await?;

        if data.len() >= RESPONSE_SIZE_MAXIMUM {
            debug!("Message filled the whole buffer (len: {})", data.len());
        }

        let mut data = &data[..];
        let mut msgs = Vec::new();

        while let Some((header, rest)) = parse_header_response(data) {
            msgs.push(header);
            data = rest;
        }

        if msgs.is_empty() {
            return Err(io::Error::new(io::ErrorKind::Other, "invalid response"));
        }

        Ok(msgs)
    }

    async fn write_request<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        req: Self::Request,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        let mut buf = Vec::with_capacity(REQUEST_SIZE_MAXIMUM);

        let _ = req.encode_length_delimited(&mut buf);

        io.write_all(&buf).await?;

        Ok(())
    }

    async fn write_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        resps: Self::Response,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        let mut buf = Vec::with_capacity(RESPONSE_SIZE_MAXIMUM);

        for resp in resps {
            if resp.encode_length_delimited(&mut buf).is_err() {
                // Error on encoding means the buffer is full.
                // We will send a partial response back.
                debug!("Sending partial response");
                break;
            }
        }

        io.write_all(&buf).await?;

        Ok(())
    }
}

async fn read_up_to<T>(io: &mut T, limit: usize) -> io::Result<Vec<u8>>
where
    T: AsyncRead + Unpin + Send,
{
    let mut buf = vec![0u8; limit];
    let mut read_len = 0;

    loop {
        if read_len == buf.len() {
            // No empty space. Buffer is full.
            break;
        }

        let len = io.read(&mut buf[read_len..]).await?;

        if len == 0 {
            // EOF
            break;
        }

        read_len += len;
    }

    buf.truncate(read_len);

    Ok(buf)
}

fn parse_delimiter(mut buf: &[u8]) -> Option<(usize, &[u8])> {
    if buf.is_empty() {
        return None;
    }

    let Ok(len) = prost::decode_length_delimiter(&mut buf) else {
        return None;
    };

    Some((len, buf))
}

fn parse_header_response(buf: &[u8]) -> Option<(HeaderResponse, &[u8])> {
    let (len, rest) = parse_delimiter(buf)?;

    if rest.len() < len {
        debug!("Message is incomplete: {len}");
        return None;
    }

    let Ok(msg) = HeaderResponse::decode(&rest[..len]) else {
        return None;
    };

    Some((msg, &rest[len..]))
}

fn parse_header_request(buf: &[u8]) -> Option<HeaderRequest> {
    let (len, rest) = parse_delimiter(buf)?;

    if rest.len() < len {
        debug!("Message is incomplete: {len}");
        return None;
    }

    let Ok(msg) = HeaderRequest::decode(&rest[..len]) else {
        return None;
    };

    Some(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;
    use celestia_proto::p2p::pb::{header_request::Data, HeaderRequest, HeaderResponse};
    use futures::io::{AsyncRead, AsyncReadExt, Cursor, Error};
    use futures::task::{Context, Poll};
    use libp2p::{request_response::Codec, swarm::StreamProtocol};
    use prost::{encode_length_delimiter, Message};
    use std::io::ErrorKind;
    use std::pin::Pin;

    #[cfg(not(target_arch = "wasm32"))]
    use tokio::test as async_test;
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as async_test;

    #[async_test]
    async fn test_decode_header_request_empty() {
        let header_request = HeaderRequest {
            amount: 0,
            data: None,
        };

        let encoded_header_request = header_request.encode_length_delimited_to_vec();

        let mut reader = Cursor::new(encoded_header_request);

        let stream_protocol = StreamProtocol::new("/foo/bar/v0.1");
        let mut codec = HeaderCodec {};

        let decoded_header_request = codec
            .read_request(&stream_protocol, &mut reader)
            .await
            .unwrap();

        assert_eq!(header_request, decoded_header_request);
    }

    #[async_test]
    async fn test_decode_multiple_small_header_response() {
        const MSG_COUNT: usize = 10;
        let header_response = HeaderResponse {
            body: vec![1, 2, 3],
            status_code: 1,
        };

        let encoded_header_response = header_response.encode_length_delimited_to_vec();

        let mut multi_msg = vec![];
        for _ in 0..MSG_COUNT {
            multi_msg.extend_from_slice(&encoded_header_response);
        }
        let mut reader = Cursor::new(multi_msg);

        let stream_protocol = StreamProtocol::new("/foo/bar/v0.1");
        let mut codec = HeaderCodec {};

        let decoded_header_response = codec
            .read_response(&stream_protocol, &mut reader)
            .await
            .unwrap();

        for decoded_header in decoded_header_response.iter() {
            assert_eq!(&header_response, decoded_header);
        }
        assert_eq!(decoded_header_response.len(), MSG_COUNT);
    }

    #[async_test]
    async fn test_decode_header_request_too_large() {
        let too_long_message_len = REQUEST_SIZE_MAXIMUM + 1;
        let mut length_delimiter_buffer = BytesMut::new();
        prost::encode_length_delimiter(too_long_message_len, &mut length_delimiter_buffer).unwrap();
        let mut reader = Cursor::new(length_delimiter_buffer);

        let stream_protocol = StreamProtocol::new("/foo/bar/v0.1");
        let mut codec = HeaderCodec {};

        let decoding_error = codec
            .read_request(&stream_protocol, &mut reader)
            .await
            .expect_err("expected error for too large request");

        assert_eq!(decoding_error.kind(), ErrorKind::Other);
    }

    #[async_test]
    async fn test_decode_header_response_too_large() {
        let too_long_message_len = RESPONSE_SIZE_MAXIMUM + 1;
        let mut length_delimiter_buffer = BytesMut::new();
        encode_length_delimiter(too_long_message_len, &mut length_delimiter_buffer).unwrap();
        let mut reader = Cursor::new(length_delimiter_buffer);

        let stream_protocol = StreamProtocol::new("/foo/bar/v0.1");
        let mut codec = HeaderCodec {};

        let decoding_error = codec
            .read_response(&stream_protocol, &mut reader)
            .await
            .expect_err("expected error for too large request");

        assert_eq!(decoding_error.kind(), ErrorKind::Other);
    }

    #[test]
    fn test_invalid_varint() {
        // 10 consecutive bytes with continuation bit set + 1 byte, which is longer than allowed
        //    for length delimiter
        let varint = [
            0b1000_0000,
            0b1000_0000,
            0b1000_0000,
            0b1000_0000,
            0b1000_0000,
            0b1000_0000,
            0b1000_0000,
            0b1000_0000,
            0b1000_0000,
            0b1000_0000,
            0b0000_0001,
        ];

        assert_eq!(parse_delimiter(&varint), None);
    }

    #[test]
    fn parse_trailing_zero_varint() {
        let varint = [0b1000_0001, 0b0000_0000, 0b1111_1111];
        assert!(matches!(parse_delimiter(&varint), Some((1, [255]))));

        let varint = [0b1000_0000, 0b1000_0000, 0b1000_0000, 0b0000_0000];
        assert!(matches!(parse_delimiter(&varint), Some((0, []))));
    }

    #[async_test]
    async fn test_decode_header_double_response_data() {
        let mut header_response_buffer = BytesMut::with_capacity(512);
        let header_response0 = HeaderResponse {
            body: b"9999888877776666555544443333222211110000".to_vec(),
            status_code: 1,
        };
        let header_response1 = HeaderResponse {
            body: b"0000111122223333444455556666777788889999".to_vec(),
            status_code: 2,
        };
        header_response0
            .encode_length_delimited(&mut header_response_buffer)
            .unwrap();
        header_response1
            .encode_length_delimited(&mut header_response_buffer)
            .unwrap();
        let mut reader = Cursor::new(header_response_buffer);

        let stream_protocol = StreamProtocol::new("/foo/bar/v0.1");
        let mut codec = HeaderCodec {};

        let decoded_header_response = codec
            .read_response(&stream_protocol, &mut reader)
            .await
            .unwrap();
        assert_eq!(header_response0, decoded_header_response[0]);
        assert_eq!(header_response1, decoded_header_response[1]);
    }

    #[async_test]
    async fn test_decode_header_request_chunked_data() {
        let data = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
        let header_request = HeaderRequest {
            amount: 1,
            data: Some(Data::Hash(data.to_vec())),
        };
        let encoded_header_request = header_request.encode_length_delimited_to_vec();

        let stream_protocol = StreamProtocol::new("/foo/bar/v0.1");
        let mut codec = HeaderCodec {};
        {
            let mut reader =
                ChunkyAsyncRead::<_, 1>::new(Cursor::new(encoded_header_request.clone()));
            let decoded_header_request = codec
                .read_request(&stream_protocol, &mut reader)
                .await
                .unwrap();
            assert_eq!(header_request, decoded_header_request);
        }
        {
            let mut reader =
                ChunkyAsyncRead::<_, 2>::new(Cursor::new(encoded_header_request.clone()));
            let decoded_header_request = codec
                .read_request(&stream_protocol, &mut reader)
                .await
                .unwrap();

            assert_eq!(header_request, decoded_header_request);
        }
        {
            let mut reader =
                ChunkyAsyncRead::<_, 3>::new(Cursor::new(encoded_header_request.clone()));
            let decoded_header_request = codec
                .read_request(&stream_protocol, &mut reader)
                .await
                .unwrap();

            assert_eq!(header_request, decoded_header_request);
        }
        {
            let mut reader =
                ChunkyAsyncRead::<_, 4>::new(Cursor::new(encoded_header_request.clone()));
            let decoded_header_request = codec
                .read_request(&stream_protocol, &mut reader)
                .await
                .unwrap();

            assert_eq!(header_request, decoded_header_request);
        }
    }

    #[async_test]
    async fn test_decode_header_response_chunked_data() {
        let data = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
        let header_response = HeaderResponse {
            body: data.to_vec(),
            status_code: 2,
        };
        let encoded_header_response = header_response.encode_length_delimited_to_vec();

        let stream_protocol = StreamProtocol::new("/foo/bar/v0.1");
        let mut codec = HeaderCodec {};
        {
            let mut reader =
                ChunkyAsyncRead::<_, 1>::new(Cursor::new(encoded_header_response.clone()));
            let decoded_header_response = codec
                .read_response(&stream_protocol, &mut reader)
                .await
                .unwrap();
            assert_eq!(header_response, decoded_header_response[0]);
        }
        {
            let mut reader =
                ChunkyAsyncRead::<_, 2>::new(Cursor::new(encoded_header_response.clone()));
            let decoded_header_response = codec
                .read_response(&stream_protocol, &mut reader)
                .await
                .unwrap();

            assert_eq!(header_response, decoded_header_response[0]);
        }
        {
            let mut reader =
                ChunkyAsyncRead::<_, 3>::new(Cursor::new(encoded_header_response.clone()));
            let decoded_header_response = codec
                .read_response(&stream_protocol, &mut reader)
                .await
                .unwrap();

            assert_eq!(header_response, decoded_header_response[0]);
        }
        {
            let mut reader =
                ChunkyAsyncRead::<_, 4>::new(Cursor::new(encoded_header_response.clone()));
            let decoded_header_response = codec
                .read_response(&stream_protocol, &mut reader)
                .await
                .unwrap();

            assert_eq!(header_response, decoded_header_response[0]);
        }
    }

    #[async_test]
    async fn test_chunky_async_read() {
        let read_data = "FOO123";
        let cur0 = Cursor::new(read_data);
        let mut chunky = ChunkyAsyncRead::<_, 3>::new(cur0);

        let mut output_buffer: BytesMut = b"BAR987".as_ref().into();

        let _ = chunky.read(&mut output_buffer[..]).await.unwrap();
        assert_eq!(output_buffer, b"FOO987".as_ref());
    }

    struct ChunkyAsyncRead<T: AsyncRead + Unpin, const CHUNK_SIZE: usize> {
        inner: T,
    }

    impl<T: AsyncRead + Unpin, const CHUNK_SIZE: usize> ChunkyAsyncRead<T, CHUNK_SIZE> {
        fn new(inner: T) -> Self {
            ChunkyAsyncRead { inner }
        }
    }

    impl<T: AsyncRead + Unpin, const CHUNK_SIZE: usize> AsyncRead for ChunkyAsyncRead<T, CHUNK_SIZE> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<Result<usize, Error>> {
            let len = buf.len().min(CHUNK_SIZE);
            Pin::new(&mut self.inner).poll_read(cx, &mut buf[..len])
        }
    }
}
