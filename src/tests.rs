use async_stream::stream;
use rocket::{
    futures::Stream,
    get,
    http::{ContentType, Header, Status},
    local::blocking::Client,
    post, routes, Build, Rocket,
};

use crate::*;

#[get("/mixed")]
fn multipart_route() -> MultipartStream<impl Stream<Item = MultipartSection<'static>>> {
    MultipartStream::new(
        "Sep",
        stream! {
            yield MultipartSection::from_slice(b"How can I help you")
                .add_header(ContentType::Text);
            yield MultipartSection::from_slice(b"today?")
                .add_header(ContentType::Text);
            yield MultipartSection::from_slice(&[0xFF, 0xFE, 0xF0])
                .add_header(ContentType::Binary);
        },
    )
}
#[post("/mixed", data = "<multipart>")]
async fn multipart_data(mut multipart: MultipartReader<'_>) -> std::result::Result<String, Status> {
    use std::fmt::Write as _;
    let mut s = String::new();
    write!(s, "M CT: {}\n", multipart.content_type()).unwrap();
    while let Some(a) = multipart.next().await.map_err(|_| Status::BadRequest)? {
        if let Some(ct) = a.headers().get_one("Content-Type") {
            write!(s, "CT: {}\n", ct).unwrap();
        }
        let buf = a.to_bytes().await.unwrap();
        if let Ok(val) = std::str::from_utf8(&buf) {
            write!(s, "V: {}\n", val).unwrap();
        } else {
            write!(s, "R: {:?}\n", &buf[..]).unwrap();
        }
    }
    Ok(s)
}

#[cfg(feature = "json")]
#[post("/json", data = "<multipart>")]
async fn json_data(mut multipart: MultipartReader<'_>) -> std::result::Result<String, Status> {
    use std::fmt::Write as _;
    let mut s = String::new();
    write!(s, "M CT: {}\n", multipart.content_type()).unwrap();
    while let Some(a) = multipart.next().await.map_err(|_| Status::BadRequest)? {
        if let Some(ct) = a.headers().get_one("Content-Type") {
            write!(s, "CT: {}\n", ct).unwrap();
        }
        write!(
            s,
            "{:?}\n",
            a.json::<serde_json::Value>()
                .await
                .map(|v| serde_json::to_string(&v).unwrap())
        )
        .unwrap();
    }
    Ok(s)
}
#[cfg(feature = "json")]
#[get("/json")]
fn json_send() -> MultipartStream<impl Stream<Item = MultipartSection<'static>>> {
    MultipartStream::new(
        "Sep",
        stream! {
            yield MultipartSection::from_json(&serde_json::json!({"val": 0})).unwrap()
                .add_header(ContentType::JSON);
        },
    )
}

fn rocket() -> Rocket<Build> {
    let mut rocket = rocket::build().mount("/", routes![multipart_route, multipart_data]);
    #[cfg(feature = "json")]
    {
        rocket = rocket.mount("/", routes![json_data, json_send]);
    }
    rocket
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
fn simple_encoder() {
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

#[test]
fn empty_section() {
    let client = Client::untracked(rocket()).unwrap();
    let mut expected_contents = vec![];
    expected_contents.extend_from_slice(b"--Sep\r\n");
    expected_contents.extend_from_slice(b"Content-Type: text/plain\r\n");
    expected_contents.extend_from_slice(b"\r\n");
    expected_contents.extend_from_slice(b"\r\n--Sep\r\n");
    expected_contents.extend_from_slice(b"Content-Type: text/fake\r\n");
    expected_contents.extend_from_slice(b"\r\nCT");
    expected_contents.extend_from_slice(b"\r\n--Sep--");
    let res = client
        .post("/mixed")
        .header(Header::new("Content-Type", "multipart/mixed; boundary=Sep"))
        .body(expected_contents)
        .dispatch();
    assert_eq!(res.status(), Status::Ok);
    assert_eq!(
        res.into_string().unwrap(),
        "M CT: multipart/mixed; boundary=Sep
CT: text/plain
V: 
CT: text/fake
V: CT
"
    );
}

#[test]
#[cfg(feature = "json")]
fn json_decode() {
    let client = Client::untracked(rocket()).unwrap();
    let mut expected_contents = vec![];
    expected_contents.extend_from_slice(b"--Sep\r\n");
    expected_contents.extend_from_slice(b"Content-Type: application/json\r\n");
    expected_contents.extend_from_slice(b"\r\n{ \"val\": 0 }");
    expected_contents.extend_from_slice(b"\r\n--Sep--");
    let res = client
        .post("/json")
        .header(Header::new("Content-Type", "multipart/mixed; boundary=Sep"))
        .body(expected_contents)
        .dispatch();
    assert_eq!(res.status(), Status::Ok);
    assert_eq!(
        res.into_string().unwrap(),
        "M CT: multipart/mixed; boundary=Sep
CT: application/json
Ok(\"{\\\"val\\\":0}\")
"
    );
}
#[test]
#[cfg(feature = "json")]
fn json_encode() {
    let client = Client::untracked(rocket()).unwrap();
    let mut expected_contents = vec![];
    expected_contents.extend_from_slice(b"\r\n--Sep\r\n");
    expected_contents.extend_from_slice(b"Content-Type: application/json\r\n");
    expected_contents.extend_from_slice(b"\r\n{\"val\":0}");
    expected_contents.extend_from_slice(b"\r\n--Sep--\r\n");
    let res = client.get("/json").dispatch();
    assert_eq!(res.status(), Status::Ok);
    assert_eq!(res.into_bytes().unwrap(), expected_contents);
}
