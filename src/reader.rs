use std::{
    io,
    pin::Pin,
    str::{FromStr, Utf8Error},
    task::{ready, Context, Poll},
};

use memchr::{memchr, memmem::find, memrchr};
use rocket::{
    data::{DataStream, FromData, Outcome, ToByteUnit},
    futures::StreamExt,
    http::{ContentType, Header, HeaderMap, Status},
    tokio::io::{AsyncBufRead, AsyncRead, ReadBuf},
    Data, Request,
};
use thiserror::Error;
use tokio_util::{
    bytes::{BufMut, Bytes, BytesMut},
    codec::{Decoder, FramedRead},
};

type Result<T, E = Error> = std::result::Result<T, E>;

/// Error returned by `MultipartReader`
#[derive(Debug, Error)]
pub enum Error {
    /// An underlying IO error
    #[error(transparent)]
    Io(#[from] io::Error),
    /// A header was not utf8 encoded
    #[error(transparent)]
    Encoding(#[from] Utf8Error),
    /// An error from `serde_json`
    ///
    /// Only available on `json` feature
    #[cfg(feature = "json")]
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// The content-type of a multipart stream did not specify a boundary
    #[error("The content type of a multipart stream must specify a boundary")]
    BoundaryNotSpecified,
}

/// A single section in a multipart stream
///
/// Implements both `AsyncRead` and `AsyncBufRead`, and can be used with any API
/// that expects either.
pub struct MultipartReadSection<'r, 'a> {
    headers: HeaderMap<'static>,
    reader: &'a mut MultipartReader<'r>,
}

impl<'a> MultipartReadSection<'_, 'a> {
    /// Gets the list of headers specific to this multipart section
    pub fn headers(&self) -> &HeaderMap<'static> {
        &self.headers
    }

    /// Retrieves the `Content-Type` header (if it exists) and parses it.
    pub fn content_type(&self) -> Option<ContentType> {
        let s = self.headers.get_one("Content-Type")?;
        ContentType::from_str(s).ok()
    }

    /// Read the entire stream into a single bytes object.
    ///
    /// Should generally be more effecient than `read_to_end`, since it
    /// generally avoids copying data into a new buffer.
    pub async fn to_bytes(self) -> Result<Bytes> {
        let mut raw_data = BytesMut::new();
        while let MultipartFrame::Data(bytes) = &mut self.reader.buffer {
            raw_data.unsplit(bytes.split());
            match self.reader.stream.next().await {
                Some(Ok(next)) => self.reader.buffer = next,
                Some(Err(e)) => {
                    self.reader.buffer = MultipartFrame::End;
                    return Err(e);
                }
                None => self.reader.buffer = MultipartFrame::End,
            }
        }
        Ok(raw_data.freeze())
    }

    /// Read the entire stream, and parse it as a JSON object
    ///
    /// Only available on `json` feature
    #[cfg(feature = "json")]
    pub async fn json<T: serde::de::DeserializeOwned>(self) -> Result<T> {
        let bytes = self.to_bytes().await?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

impl AsyncRead for MultipartReadSection<'_, '_> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
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
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
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
    Data(BytesMut),
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
    fn parse_header(header: &[u8]) -> Result<Header<'static>> {
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

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>> {
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
                            return Ok(Some(MultipartFrame::Data(data)));
                        }
                        if pos + self.boundary.len() + 2 > src.len() {
                            let data = src.split_to(pos - 4);
                            src.reserve(CHUNK_SIZE);
                            if data.is_empty() {
                                return Ok(None);
                            } else {
                                return Ok(Some(MultipartFrame::Data(data)));
                            }
                        }
                        if &src[pos - 4..pos] == b"\r\n--"
                            && &src[pos + self.boundary.len()..][..2] == b"--"
                        {
                            self.state = MultipartDecoderState::End;
                            if pos - 4 > 0 {
                                let data = src.split_to(pos - 4);
                                return Ok(Some(MultipartFrame::Data(data)));
                            }
                            continue;
                        }
                        if &src[pos - 4..pos] != b"\r\n--"
                            && &src[pos + self.boundary.len()..][..2] != b"\r\n"
                        {
                            let data = src.split_to(pos + self.boundary.len());
                            return Ok(Some(MultipartFrame::Data(data)));
                        } else {
                            if pos > 4 {
                                let data = src.split_to(pos - 4);
                                return Ok(Some(MultipartFrame::Data(data)));
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
                            return Ok(Some(MultipartFrame::Data(data)));
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

/// A data guard for `multipart/*` data. Provides async reading of the
/// individual multipart sections.
///
/// # Example
///
/// ```rust,no_run
/// # use rocket::{post, tokio::io::AsyncReadExt};
/// # use rocket_multipart::MultipartReader;
/// #[post("/mixed", data = "<mixed>")]
/// async fn multipart_data(mut mixed: MultipartReader<'_>) -> String {
///     while let Some(mut a) = mixed.next().await.unwrap() {
///         if let Some(ct) = a.headers().get_one("Content-Type") {
///             // Check content_type
///         }
///         let mut buf = vec![];
///         a.read_to_end(&mut buf).await.unwrap();
///         // Use section's body
///     }
/// #   String::new()
/// }
/// ```
///
/// # Limits
///
/// Like most data guards, `MultipartReader` provides a configurable limit. It
/// uses the `file/multipart` limit or a default limit of 1 MiB. This is the
/// limit for the entire stream, not individual sections.
pub struct MultipartReader<'r> {
    stream: FramedRead<DataStream<'r>, MultipartDecoder<'r>>,
    buffer: MultipartFrame,
    content_type: &'r ContentType,
}

impl<'r> MultipartReader<'r> {
    /// Gets the next section from this multipart reader. The returned section
    /// mutably borrows from `self`, so it must be dropped before another secton
    /// can be read.
    pub async fn next(&mut self) -> Result<Option<MultipartReadSection<'r, '_>>> {
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
        loop {
            match self.stream.next().await {
                Some(Ok(MultipartFrame::End)) | None => {
                    self.buffer = MultipartFrame::End;
                    if headers.is_empty() {
                        return Ok(None);
                    } else {
                        break;
                    }
                }
                Some(Ok(MultipartFrame::Header(header))) => headers.add(header),
                Some(Ok(MultipartFrame::Boundary)) => {
                    self.buffer = MultipartFrame::Boundary;
                    break;
                }
                Some(Ok(val @ MultipartFrame::Data(_))) => {
                    self.buffer = val;
                    break;
                }
                // Some(Ok(val)) => self.buffer = val,
                Some(Err(e)) => return Err(e),
            }
        }
        Ok(Some(MultipartReadSection {
            headers,
            reader: self,
        }))
    }

    /// The content type of the multipart stream as a whole. The primary type is always `multipart`
    pub fn content_type(&self) -> &'r ContentType {
        self.content_type
    }
}

#[rocket::async_trait]
impl<'r> FromData<'r> for MultipartReader<'r> {
    type Error = Error;

    async fn from_data(req: &'r Request<'_>, data: Data<'r>) -> Outcome<'r, Self> {
        let limit = req
            .rocket()
            .config()
            .limits
            .get("file/multipart")
            .unwrap_or(1.mebibytes());
        if let Some(content_type) = req.content_type().filter(|c| c.top() == "multipart") {
            if let Some(boundary) = content_type.param("boundary") {
                Outcome::Success(Self {
                    stream: FramedRead::new(
                        data.open(limit),
                        MultipartDecoder {
                            state: MultipartDecoderState::BeforeFirstBoundary,
                            boundary,
                        },
                    ),
                    buffer: MultipartFrame::Data(BytesMut::new()),
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
