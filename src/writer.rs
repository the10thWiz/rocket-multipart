use std::{
    io::{self, Cursor},
    pin::Pin,
    task::{Context, Poll},
};

use rocket::{
    futures::Stream,
    http::{ContentType, Header, HeaderMap, Status},
    response::{Responder, Result},
    tokio::io::{AsyncRead, ReadBuf},
    Request, Response,
};

/// A single section to be returned in a stream
pub struct MultipartSection<'r> {
    headers: HeaderMap<'static>,
    content: Pin<Box<dyn AsyncRead + Send + 'r>>,
}

impl<'r> MultipartSection<'r> {
    /// Construct a new MultipartSection from an async reader.
    ///
    /// If the readers is already in a `Box`, use [`Self::from_box`]
    pub fn new<T: AsyncRead + Send + 'r>(reader: T) -> Self {
        Self {
            headers: HeaderMap::new(),
            content: Box::pin(reader),
        }
    }

    /// Construct a new MultipartSection from a Boxed async reader.
    ///
    /// Useful to avoid double boxing a reader.
    pub fn from_box(reader: Box<dyn AsyncRead + Send + 'r>) -> Self {
        Self {
            headers: HeaderMap::new(),
            content: Box::into_pin(reader),
        }
    }

    /// Construct a new MultipartSection from a byte slice.
    pub fn from_slice(slice: &'r [u8]) -> Self {
        Self {
            headers: HeaderMap::new(),
            content: Box::pin(Cursor::new(slice)),
        }
    }

    /// Add a header to this section. If this section already has a header with
    /// the same name, this method adds an additional value.
    pub fn add_header(mut self, header: impl Into<Header<'static>>) -> Self {
        self.headers.add(header);
        self
    }

    /// Replaces a header for this section. If this section already has a header
    /// with the same name, this methods replaces all values with the new value.
    pub fn replace_header(mut self, header: impl Into<Header<'static>>) -> Self {
        self.headers.replace(header);
        self
    }

    fn encode_headers(&self, boundary: &str) -> String {
        let mut s = format!("\r\n--{boundary}\r\n");
        for h in self.headers.iter() {
            s.push_str(h.name.as_str());
            s.push_str(": ");
            s.push_str(h.value());
            s.push_str("\r\n");
        }
        s.push_str("\r\n");
        s
    }
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

impl<'r, T: Stream<Item = MultipartSection<'r>> + Send + 'r> AsyncRead
    for MultipartStreamInner<'r, T>
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let (boundary, mut stream, state) = self.inner();
        loop {
            match state {
                StreamState::Waiting => match stream.as_mut().poll_next(cx) {
                    Poll::Ready(Some(v)) => {
                        *state = StreamState::Header(
                            Cursor::new(v.encode_headers(boundary).into_bytes()),
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
