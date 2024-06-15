//! # Rocket Multipart Streams
//!
//! Implements support for Multipart streams in Rocket. The core type is
//! [`MultipartStream`], which adapts a stream of [`MultipartSection`]s into a
//! `multipart/mixed` response.

use std::{
    borrow::Cow,
    fmt::Display,
    io::Cursor,
    pin::Pin,
    task::{Context, Poll},
};

use rocket::{
    futures::Stream,
    http::{ContentType, Status},
    response::{Responder, Result},
    tokio::io::{AsyncRead, ReadBuf},
    Request, Response,
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

#[cfg(test)]
mod tests {
    use async_stream::stream;
    use rocket::{get, local::blocking::Client, routes, Build, Rocket};

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

    fn rocket() -> Rocket<Build> {
        rocket::build().mount("/", routes![multipart_route])
    }

    #[test]
    fn simple_it_works() {
        let client = Client::untracked(rocket()).unwrap();
        let res = client.get("/mixed").dispatch();
        assert_eq!(res.status(), Status::Ok);
        assert_eq!(res.content_type(), Some(ContentType::new("multipart", "mixed")));
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
        assert_eq!(res.into_bytes(), Some(expected_contents));
    }
}
