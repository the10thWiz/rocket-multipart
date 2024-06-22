#![deny(missing_docs)]
//! # Rocket Multipart Streams
//!
//! Implements support for Multipart streams in Rocket. The core type is
//! [`MultipartStream`], which adapts a stream of [`MultipartSection`]s into a
//! `multipart/mixed` response.

use std::{
    borrow::Cow,
    fmt::Display,
    io::{self, Cursor},
    mem::replace,
    pin::Pin,
    str::Utf8Error,
    task::{ready, Context, Poll},
};

use memchr::{memchr, memmem::find, memrchr};
use rocket::{
    data::{DataStream, FromData, Outcome, ToByteUnit},
    futures::{Stream, StreamExt},
    http::{ContentType, Header, HeaderMap, Status},
    response::{Responder, Result},
    tokio::io::{AsyncBufRead, AsyncRead, ReadBuf},
    Data, Request, Response,
};
use thiserror::Error;
use tokio_util::{
    bytes::{BufMut, Bytes, BytesMut},
    codec::{Decoder, FramedRead},
};

/// A single section to be returned in a stream
pub struct MultipartSection<'r> {
    /// The content type of this section
    pub content_type: Option<ContentType>,
    /// The content encoding (compression) of this section
    pub content_encoding: Option<Cow<'r, str>>,
    /// The actual contents. Use `Box::pin()` to adapt any type
    /// that implements `AsyncRead`.
    pub content: Pin<Box<dyn AsyncRead + Send + 'r>>,
}

/// A stream of sections to be returned as a `multipart/mixed` stream.
pub struct MultipartStream<T> {
    boundary: String,
    stream: T,
    sub_type: &'static str,
}

impl<T> MultipartStream<T> {
    /// Construct a stream, using the specified string as a boundary marker
    /// between stream items.
    pub fn new(boundary: impl Into<String>, stream: T) -> Self {
        Self {
            boundary: boundary.into(),
            stream,
            sub_type: "mixed",
        }
    }

    /// Construct a stream, generating a random 15 character (alpha-numeric)
    /// boundary marker
    #[cfg(feature = "rand")]
    pub fn new_random(stream: T) -> Self {
        use rand::{distributions::Alphanumeric, Rng};

        Self {
            boundary: rand::thread_rng()
                .sample_iter(Alphanumeric)
                .map(|v| v as char)
                .take(15)
                .collect(),
            stream,
            sub_type: "mixed",
        }
    }

    /// Change the ContentType sub type from the default `mixed`
    pub fn with_subtype(mut self, sub_type: &'static str) -> Self {
        self.sub_type = sub_type;
        self
    }
}

impl<'r, 'o: 'r, T: Stream<Item = MultipartSection<'o>> + Send + 'o> Responder<'r, 'o>
    for MultipartStream<T>
{
    fn respond_to(self, _r: &'r Request<'_>) -> Result<'o> {
        Response::build()
            .status(Status::Ok)
            .header(
                ContentType::new("multipart", self.sub_type)
                    .with_params(("boundary", self.boundary.clone())),
            )
            .streamed_body(MultipartStreamInner(
                self.boundary,
                self.stream,
                StreamState::Waiting,
            ))
            .ok()
    }
}

struct MultipartStreamInner<'r, T>(String, T, StreamState<'r>);

impl<'r, T> MultipartStreamInner<'r, T> {
    fn inner(self: Pin<&mut Self>) -> (&str, Pin<&mut T>, &mut StreamState<'r>) {
        // SAFETY: We are projecting `String` and `StreamState` to simple borrows (they implement unpin, so this is fine)
        // We project `T` to `Pin<&mut T>`, since we don't know (or care) if it implement unpin
        let this = unsafe { self.get_unchecked_mut() };
        (
            &this.0,
            unsafe { Pin::new_unchecked(&mut this.1) },
            &mut this.2,
        )
    }
}

enum StreamState<'r> {
    Waiting,
    Header(Cursor<Vec<u8>>, Pin<Box<dyn AsyncRead + Send + 'r>>),
    Raw(Pin<Box<dyn AsyncRead + Send + 'r>>),
    Footer(Cursor<Vec<u8>>),
}

struct HV<T>(&'static str, Option<T>);
impl<T: Display> Display for HV<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.1 {
            Some(v) => write!(f, "{}: {}\r\n", self.0, v),
            None => Ok(()),
        }
    }
}

impl<'r, T: Stream<Item = MultipartSection<'r>> + Send + 'r> AsyncRead
    for MultipartStreamInner<'r, T>
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let (boundary, mut stream, state) = self.inner();
        loop {
            match state {
                StreamState::Waiting => match stream.as_mut().poll_next(cx) {
                    Poll::Ready(Some(v)) => {
                        *state = StreamState::Header(
                            Cursor::new(
                                format!(
                                    "\r\n--{boundary}\r\n{}{}\r\n",
                                    HV("Content-Type", v.content_type),
                                    HV("Content-Encoding", v.content_encoding),
                                )
                                .into_bytes(),
                            ),
                            v.content,
                        );
                    }
                    Poll::Ready(None) => {
                        *state = StreamState::Footer(Cursor::new(
                            format!("\r\n--{boundary}--\r\n").into_bytes(),
                        ));
                    }
                    Poll::Pending => return Poll::Pending,
                },
                StreamState::Header(r, _) => {
                    let cur = buf.filled().len();
                    match Pin::new(r).poll_read(cx, buf) {
                        Poll::Ready(Ok(())) => (),
                        v => return v,
                    }
                    if cur == buf.filled().len() {
                        // EOF, move on
                        if let StreamState::Header(_, next) =
                            std::mem::replace(state, StreamState::Waiting)
                        {
                            *state = StreamState::Raw(next);
                        } else {
                            unreachable!()
                        }
                    } else {
                        return Poll::Ready(Ok(()));
                    }
                }
                StreamState::Raw(r) => {
                    let cur = buf.filled().len();
                    match r.as_mut().poll_read(cx, buf) {
                        Poll::Ready(Ok(())) => (),
                        v => return v,
                    }
                    if cur == buf.filled().len() {
                        // EOF, move on
                        *state = StreamState::Waiting;
                    } else {
                        return Poll::Ready(Ok(()));
                    }
                }
                StreamState::Footer(r) => {
                    let cur = buf.filled().len();
                    match Pin::new(r).poll_read(cx, buf) {
                        Poll::Ready(Ok(())) => (),
                        v => return v,
                    }
                    if cur == buf.filled().len() {
                        // EOF, move on
                        return Poll::Ready(Ok(()));
                    } else {
                        return Poll::Ready(Ok(()));
                    }
                }
            }
        }
    }
}

/// A single item in a multipart stream
pub struct MultipartReadSection<'r, 'a> {
    headers: HeaderMap<'static>,
    reader: &'a mut MultipartReader<'r>,
}

impl<'a> MultipartReadSection<'_, 'a> {
    /// Gets the list of headers specific to this multipart section
    pub fn headers(&self) -> &HeaderMap<'static> {
        &self.headers
    }
}

impl AsyncRead for MultipartReadSection<'_, '_> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // Relies on AsyncBufRead
        let mut this = self.get_mut();
        match Pin::new(&mut this).poll_fill_buf(cx) {
            Poll::Ready(Ok(buffer)) => {
                let write_buf = buf.initialize_unfilled();
                let len = buffer.len().min(write_buf.len());
                write_buf[..len].copy_from_slice(&buffer[..len]);
                unsafe { buf.advance_mut(len) };
                Pin::new(this).consume(len);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Pending => return Poll::Pending,
        }
    }
}

impl AsyncBufRead for MultipartReadSection<'_, '_> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<&[u8]>> {
        let this = self.get_mut();
        let buffer = &mut this.reader.buffer;
        if buffer.is_empty() {
            match ready!(this.reader.stream.poll_next_unpin(cx)) {
                Some(Ok(by)) => *buffer = by,
                None => *buffer = MultipartFrame::End,
                Some(Err(e)) => return Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e))),
            }
        }
        // Buffer now either has data, or we have run out of data to provide
        if let MultipartFrame::Data(data) = buffer {
            return Poll::Ready(Ok(data));
        } else {
            return Poll::Ready(Ok(b""));
        }
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        let this = self.get_mut();
        if let MultipartFrame::Data(data) = &mut this.reader.buffer {
            let _ = data.split_to(amt.min(data.len()));
        }
    }
}

/// Error returned by `MultipartReader`
#[derive(Debug, Error)]
pub enum Error {
    /// An underlying IO error
    #[error(transparent)]
    Io(#[from] io::Error),
    /// A header was not utf8 encoded
    #[error(transparent)]
    Encoding(#[from] Utf8Error),
    /// The content-type of a multipart stream did not specify a boundary
    #[error("The content type of a multipart stream must specify a boundary")]
    BoundaryNotSpecified,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
enum MultipartDecoderState {
    BeforeFirstBoundary,
    Headers,
    Data,
    End,
}
struct MultipartDecoder<'r> {
    state: MultipartDecoderState,
    boundary: &'r str,
}

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
enum MultipartFrame {
    Boundary,
    Header(Header<'static>),
    Data(Bytes),
    End,
}

impl MultipartFrame {
    fn is_empty(&self) -> bool {
        if let Self::Data(v) = self {
            v.is_empty()
        } else {
            false
        }
    }
}

const CHUNK_SIZE: usize = 1024;

impl MultipartDecoder<'_> {
    fn parse_header(header: &[u8]) -> std::result::Result<Header<'static>, Error> {
        if let Some(middle) = memchr(b':', header) {
            Ok(Header::new(
                std::str::from_utf8(&header[..middle])?.to_owned(),
                std::str::from_utf8(&header[middle + 1..])?
                    .trim()
                    .to_owned(),
            ))
        } else {
            // Malformed header
            todo!()
        }
    }
}

impl Decoder for MultipartDecoder<'_> {
    type Item = MultipartFrame;
    type Error = Error;

    fn decode(
        &mut self,
        src: &mut BytesMut,
    ) -> std::prelude::v1::Result<Option<Self::Item>, Self::Error> {
        loop {
            match self.state {
                MultipartDecoderState::BeforeFirstBoundary => {
                    if let Some(pos) = find(src, self.boundary.as_bytes()) {
                        if pos < 2 {
                            let _ = src.split_to(pos + 1);
                            continue;
                        }
                        if pos + self.boundary.len() + 2 > src.len() {
                            let _ = src.split_to(pos - 4);
                            src.reserve(CHUNK_SIZE);
                            return Ok(None);
                        }
                        if &src[pos - 2..pos] == b"--"
                            && &src[pos + self.boundary.len()..][..2] == b"--"
                        {
                            self.state = MultipartDecoderState::End;
                            continue;
                        }
                        if &src[pos - 2..pos] != b"--"
                            && &src[pos + self.boundary.len()..][..2] != b"\r\n"
                        {
                            let _ = src.split_to(pos + self.boundary.len());
                        } else {
                            let _ = src.split_to(pos + self.boundary.len() + "\r\n".len());
                            self.state = MultipartDecoderState::Headers;
                            return Ok(Some(MultipartFrame::Boundary));
                        }
                    } else {
                        src.reserve(CHUNK_SIZE);
                        return Ok(None);
                    }
                }
                MultipartDecoderState::Headers => {
                    if let Some(end) = find(src, b"\r\n") {
                        let header = src.split_to(end + "\r\n".len());
                        if end == 0 {
                            self.state = MultipartDecoderState::Data;
                            continue;
                        }
                        return Ok(Some(MultipartFrame::Header(Self::parse_header(
                            &header[..],
                        )?)));
                    } else {
                        src.reserve(CHUNK_SIZE);
                        return Ok(None);
                    }
                }
                MultipartDecoderState::Data => {
                    if let Some(pos) = find(src, self.boundary.as_bytes()) {
                        if pos < 4 {
                            let data = src.split_to(pos + 1);
                            return Ok(Some(MultipartFrame::Data(data.freeze())));
                        }
                        if pos + self.boundary.len() + 2 > src.len() {
                            let data = src.split_to(pos - 4);
                            src.reserve(CHUNK_SIZE);
                            if data.is_empty() {
                                return Ok(None);
                            } else {
                                return Ok(Some(MultipartFrame::Data(data.freeze())));
                            }
                        }
                        if &src[pos - 4..pos] == b"\r\n--"
                            && &src[pos + self.boundary.len()..][..2] == b"--"
                        {
                            self.state = MultipartDecoderState::End;
                            if pos - 4 > 0 {
                                let data = src.split_to(pos - 4);
                                return Ok(Some(MultipartFrame::Data(data.freeze())));
                            }
                            continue;
                        }
                        if &src[pos - 4..pos] != b"\r\n--"
                            && &src[pos + self.boundary.len()..][..2] != b"\r\n"
                        {
                            let data = src.split_to(pos + self.boundary.len());
                            return Ok(Some(MultipartFrame::Data(data.freeze())));
                        } else {
                            if pos > 4 {
                                let data = src.split_to(pos - 4);
                                return Ok(Some(MultipartFrame::Data(data.freeze())));
                            }
                            let _ = src.split_to(pos + self.boundary.len() + "\r\n".len());
                            self.state = MultipartDecoderState::Headers;
                            return Ok(Some(MultipartFrame::Boundary));
                        }
                    } else {
                        let end = src
                            .len()
                            .saturating_sub(self.boundary.len() + 4) // Known safe prefix
                            .max(memrchr(b'\r', src).unwrap_or(0));
                        let data = src.split_to(end);
                        src.reserve(CHUNK_SIZE);
                        if data.is_empty() {
                            return Ok(None);
                        } else {
                            return Ok(Some(MultipartFrame::Data(data.freeze())));
                        }
                    }
                }
                MultipartDecoderState::End => {
                    let _ = src.split();
                    src.reserve(CHUNK_SIZE);
                    return Ok(None);
                }
            }
        }
    }
}

/// The multipart data guard
pub struct MultipartReader<'r> {
    stream: FramedRead<DataStream<'r>, MultipartDecoder<'r>>,
    buffer: MultipartFrame,
    content_type: &'r ContentType,
}

impl<'r> MultipartReader<'r> {
    // Not `Stream`, b/c the output needs a mutable borrow from the reader
    /// Gets the next section from this multipart reader.
    pub async fn next(
        &mut self,
    ) -> std::result::Result<Option<MultipartReadSection<'r, '_>>, Error> {
        while self.buffer != MultipartFrame::Boundary {
            match self.stream.next().await {
                Some(Ok(MultipartFrame::End)) | None => {
                    self.buffer = MultipartFrame::End;
                    return Ok(None);
                }
                Some(Ok(val)) => self.buffer = val,
                Some(Err(e)) => return Err(e),
            }
        }

        let mut headers = HeaderMap::new();
        while !matches!(self.buffer, MultipartFrame::Data(_)) {
            if let MultipartFrame::Header(h) = replace(&mut self.buffer, MultipartFrame::Boundary) {
                headers.add(h);
            }
            match self.stream.next().await {
                Some(Ok(MultipartFrame::End)) | None => {
                    self.buffer = MultipartFrame::End;
                    if headers.is_empty() {
                        return Ok(None);
                    } else {
                        break;
                    }
                }
                Some(Ok(val)) => self.buffer = val,
                Some(Err(e)) => return Err(e),
            }
        }
        Ok(Some(MultipartReadSection {
            headers,
            reader: self,
        }))
    }

    /// The content type of the multipart stream as a whole. The primary type is always `multipart`
    pub fn contenty_type(&self) -> &'r ContentType {
        self.content_type
    }
}

#[rocket::async_trait]
impl<'r> FromData<'r> for MultipartReader<'r> {
    type Error = Error;

    async fn from_data(req: &'r Request<'_>, data: Data<'r>) -> Outcome<'r, Self> {
        if let Some(content_type) = req.content_type().filter(|c| c.top() == "multipart") {
            if let Some(boundary) = content_type.param("boundary") {
                Outcome::Success(Self {
                    stream: FramedRead::new(
                        data.open(100.mebibytes()),
                        MultipartDecoder {
                            state: MultipartDecoderState::BeforeFirstBoundary,
                            boundary,
                        },
                    ),
                    buffer: MultipartFrame::Data(Bytes::new()),
                    content_type,
                })
            } else {
                Outcome::Error((Status::BadRequest, Error::BoundaryNotSpecified))
            }
        } else {
            Outcome::Forward((data, Status::BadRequest))
        }
    }
}

#[cfg(test)]
mod tests {
    use async_stream::stream;
    use rocket::{
        get, local::blocking::Client, post, routes, tokio::io::AsyncReadExt, Build, Rocket,
    };

    use super::*;

    #[get("/mixed")]
    fn multipart_route() -> MultipartStream<impl Stream<Item = MultipartSection<'static>>> {
        MultipartStream::new(
            "Sep",
            stream! {
                yield MultipartSection {
                    content_type: Some(ContentType::Text),
                    content_encoding: None,
                    content: Box::pin(b"How can I help you" as &[u8])
                };
                yield MultipartSection {
                    content_type: Some(ContentType::Text),
                    content_encoding: None,
                    content: Box::pin(b"today?" as &[u8])
                };
                yield MultipartSection {
                    content_type: Some(ContentType::Binary),
                    content_encoding: None,
                    content: Box::pin(&[0xFFu8, 0xFE, 0xF0] as &[u8])
                };
            },
        )
    }
    #[post("/mixed", data = "<multipart>")]
    async fn multipart_data(
        mut multipart: MultipartReader<'_>,
    ) -> std::result::Result<String, Status> {
        use std::fmt::Write as _;
        let mut s = String::new();
        write!(s, "M CT: {}\n", multipart.contenty_type()).unwrap();
        while let Some(mut a) = multipart.next().await.map_err(|_| Status::BadRequest)? {
            if let Some(ct) = a.headers.get_one("Content-Type") {
                write!(s, "CT: {}\n", ct).unwrap();
            }
            let mut buf = vec![];
            a.read_to_end(&mut buf).await.unwrap();
            if let Ok(val) = std::str::from_utf8(&buf) {
                write!(s, "V: {}\n", val).unwrap();
            } else {
                write!(s, "R: {:?}\n", buf).unwrap();
            }
        }
        Ok(s)
    }

    fn rocket() -> Rocket<Build> {
        rocket::build().mount("/", routes![multipart_route, multipart_data])
    }

    fn example_multipart_stream() -> Vec<u8> {
        let mut expected_contents = vec![];
        // TODO: I insert an extra set at the beginning. This should be ignored by almost every reader.
        expected_contents.extend_from_slice(b"\r\n");

        expected_contents.extend_from_slice(b"--Sep\r\n");
        expected_contents.extend_from_slice(b"Content-Type: text/plain; charset=utf-8\r\n");
        expected_contents.extend_from_slice(b"\r\n");
        expected_contents.extend_from_slice(b"How can I help you\r\n");

        expected_contents.extend_from_slice(b"--Sep\r\n");
        expected_contents.extend_from_slice(b"Content-Type: text/plain; charset=utf-8\r\n");
        expected_contents.extend_from_slice(b"\r\n");
        expected_contents.extend_from_slice(b"today?\r\n");

        expected_contents.extend_from_slice(b"--Sep\r\n");
        expected_contents.extend_from_slice(b"Content-Type: application/octet-stream\r\n");
        expected_contents.extend_from_slice(b"\r\n");
        expected_contents.extend_from_slice(&[0xFF, 0xFe, 0xF0]);
        expected_contents.extend_from_slice(b"\r\n");

        expected_contents.extend_from_slice(b"--Sep--\r\n");
        expected_contents
    }

    #[test]
    fn simple_it_works() {
        let client = Client::untracked(rocket()).unwrap();
        let res = client.get("/mixed").dispatch();
        assert_eq!(res.status(), Status::Ok);
        assert_eq!(
            res.content_type(),
            Some(ContentType::new("multipart", "mixed"))
        );
        let expected_contents = example_multipart_stream();
        assert_eq!(res.into_bytes(), Some(expected_contents));
    }

    #[test]
    fn simple_decoder() {
        let client = Client::untracked(rocket()).unwrap();
        let expected_contents = example_multipart_stream();
        let res = client
            .post("/mixed")
            .header(Header::new("Content-Type", "multipart/mixed; boundary=Sep"))
            .body(expected_contents)
            .dispatch();
        assert_eq!(res.status(), Status::Ok);
        assert_eq!(
            res.into_string().unwrap(),
            "M CT: multipart/mixed; boundary=Sep
CT: text/plain; charset=utf-8
V: How can I help you
CT: text/plain; charset=utf-8
V: today?
CT: application/octet-stream
R: [255, 254, 240]
"
        );
    }
}
