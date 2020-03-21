#[macro_use]
extern crate clap;
extern crate hyper;
use hyper::body::HttpBody;
use hyper::header::{HeaderName, HeaderValue, CONTENT_TYPE};
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Client, Request, Response, Server, Uri};
use once_cell::sync::OnceCell;
use std::convert::Infallible;
use std::io::Cursor;
use std::iter::Iterator;
use std::sync::{Arc, Mutex};
//use tokio::io::{stdout, AsyncWriteExt as _};
type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
type VoidRes = Res<()>;

const PORT: u16 = 8888;
const BIND_ADDRESS: [u8; 4] = [127, 0, 0, 1];

const QUEUE_NAME: &'static str = "inproc://jpegs";

////////////////////////////////////////////////////////////////
// icky global shit
static CONTEXT: OnceCell<zmq::Context> = OnceCell::new();

fn set_context(ctx: zmq::Context) -> VoidRes {
    match CONTEXT.set(ctx) {
        Ok(_) => Ok(()),
        Err(_) => Err(Box::new(simple_error::SimpleError::new(
            "Error initializing context",
        ))),
    }
}

fn get_context() -> Res<zmq::Context> {
    match CONTEXT.get() {
        Some(x) => Ok(x.clone()),
        None => Err(Box::new(simple_error::SimpleError::new(
            "Couldn't get context",
        ))),
    }
}

////////////////////////////////////////////////////////////////
// main
#[tokio::main]
async fn main() -> VoidRes {
    let ctx = zmq::Context::new();
    set_context(ctx.clone())?;
    let rt = tokio::runtime::Runtime::new()?;
    let matches = clap_app!(mjpeg_proxy =>
                            (version: "0.0")
                            (author: "@johnsnewby")
                            (@arg URL: -u --url +required +takes_value "URL to load")
    )
    .get_matches();
    let uri: String = String::from(matches.value_of("URL").unwrap());
    let queuer = rt.spawn(queue_jpegs2(ctx.clone(), uri));

    let make_svc = make_service_fn(|_conn: &hyper::server::conn::AddrStream| async {
        Ok::<_, Infallible>(service_fn(serve_http))
    });
    let addr = (BIND_ADDRESS, PORT).into();
    let server = Server::bind(&addr).serve(make_svc);
    server.await?;
    queuer.await?;
    Ok(())
}

////////////////////////////////////////////////////////////////
// hyper http shit

async fn serve_http(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    let sub = Subscription::new().unwrap();
    let iter = sub.map(|ele| package_jpegs(ele));
    let stream = futures_util::stream::iter(iter);
    let body = Body::wrap_stream(stream);
    let mut response = Response::new(body);
    let mut headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        "multipart/x-mixed-replace; boundary=BoundaryString"
            .parse()
            .unwrap(),
    );
    Ok(response)
}

fn package_jpegs(jpeg: Res<Vec<u8>>) -> Res<Vec<u8>> {
    let ele = jpeg.unwrap();
    let len = ele.len();
    let mut preamble: Vec<u8> = format!(
        "--BoundaryString\r\nContent-type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
        len
    )
    .into_bytes();
    preamble.extend_from_slice(&ele);
    Ok(preamble)
}

struct Subscription {
    socket: Arc<Mutex<zmq::Socket>>,
    msg: zmq::Message,
}

impl Subscription {
    pub fn new() -> Res<Self> {
        let ctx = get_context()?;
        let socket = ctx.socket(zmq::SUB)?;
        socket.connect(QUEUE_NAME)?;
        socket.set_subscribe(&[])?;
        let socket = Arc::new(Mutex::new(socket));
        let mut msg = zmq::Message::new();
        Ok(Self { socket, msg })
    }
}

impl Iterator for Subscription {
    type Item = Res<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Ok(socket) = self.socket.lock() {
            socket.recv(&mut self.msg, 0).unwrap();
            Some(Ok(self.msg.get(..).unwrap().to_vec()))
        } else {
            None
        }
    }
}

////////////////////////////////////////////////////////////////
//
fn get_jpegs(ctx: &zmq::Context) -> VoidRes {
    let recv_socket = ctx.socket(zmq::SUB)?;
    recv_socket.connect(QUEUE_NAME)?;
    recv_socket.set_subscribe(&[])?;
    let mut msg = zmq::Message::new();
    println!("1");
    recv_socket.recv(&mut msg, 0)?;
    println!("2");
    let _bytes: &[u8] = msg.get(..).unwrap();
    validate_jpeg(&msg.to_vec())?;
    Ok(())
}

async fn queue_jpegs2(ctx: zmq::Context, uri: String) -> () {
    match queue_jpegs(&ctx, uri).await {
        Ok(_) => (),
        Err(e) => println!("Error: {:?}", e),
    };
}

async fn queue_jpegs(ctx: &zmq::Context, uri: String) -> VoidRes {
    let mut resp = connect(&uri).await?;
    let send_socket = ctx.socket(zmq::PUB)?;
    send_socket.bind(QUEUE_NAME)?;
    let boundary_separator = get_boundary_separator(&resp)?.unwrap();
    while let Some(bytes) = read_element(&mut resp, &boundary_separator).await? {
        validate_jpeg(&bytes)?;
        //        println!("Queueing");
        send_socket.send(&bytes, 0)?;
    }
    //    println!("OK");
    Ok(())
}

async fn connect(uri: &str) -> Res<Response<hyper::Body>> {
    let client = Client::new();
    let resp = client.get(String::from(uri).parse::<Uri>()?).await?;
    Ok(resp)
}

fn get_boundary_separator(resp: &Response<hyper::Body>) -> Res<Option<String>> {
    let re = regex::Regex::new("boundary=(.+)").unwrap();
    let headers = resp.headers();
    if !headers.contains_key("Content-Type") {
        return Ok(None);
    }
    let content_type = headers["Content-Type"].clone();
    for val in content_type.to_str()?.to_string().split(";") {
        if let Some(captures) = re.captures(val) {
            return Ok(Some(captures[1].to_string()));
        }
    }
    Ok(None)
}

fn validate_jpeg(bytes: &Vec<u8>) -> VoidRes {
    //    println!("{:?}", &bytes[..100]);
    let _decoder = image::jpeg::JpegDecoder::new(Cursor::new(bytes))?;
    Ok(())
}

async fn read_element(
    resp: &mut Response<hyper::Body>,
    boundary_separator: &String,
) -> Res<Option<Vec<u8>>> {
    let re = regex::Regex::new(&format!(
        "--{}\r\nContent-type: image/jpeg\\s+Content-Length:\\s+(\\d+)\r\n",
        boundary_separator
    ))?;
    let mut result: Vec<u8> = vec![];
    if let Some(chunk) = resp.body_mut().data().await {
        let bar = chunk.unwrap();
        let foo = String::from_utf8_lossy(&bar);
        if let Some(captures) = re.captures(&foo) {
            let preamble = &captures[0];
            //            println!("{}", preamble);
            let length: usize = captures[1].to_string().parse()?;
            result.extend(&bar[preamble.len() + 2..]);
            while result.len() < length {
                let chunk = &resp.body_mut().data().await.unwrap()?[..];
                result.extend(chunk);
            }
            assert_eq!(result.len(), length + 2);
            return Ok(Some(result.to_vec()));
        }
    } else {
        return Ok(None);
    }
    Ok(Some(result))
}
