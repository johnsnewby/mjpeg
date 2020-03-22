#![feature(decl_macro)]
#![feature(backtrace)]
#[macro_use]
extern crate clap;
extern crate hyper;
#[macro_use]
extern crate log;
#[macro_use]
extern crate lazy_static;
use hyper::body::HttpBody;
use hyper::header::{HeaderName, CACHE_CONTROL, CONTENT_TYPE, EXPIRES, PRAGMA};
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Client, Request, Response, Server, Uri};
use once_cell::sync::OnceCell;
use std::convert::Infallible;
use std::io::Cursor;
use std::iter::Iterator;
use std::sync::{Arc, Mutex};

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

lazy_static! {
    static ref LAST_IMAGE: Mutex<Vec<u8>> = Mutex::new(vec!());
}

fn get_last_image() -> Vec<u8> {
    LAST_IMAGE.lock().unwrap().clone()
}

fn set_last_image(new: Vec<u8>) {
    let mut last_image = LAST_IMAGE.lock().unwrap();
    last_image.clear();
    last_image.extend_from_slice(&new);
}

////////////////////////////////////////////////////////////////
// main
#[tokio::main]
async fn main() -> VoidRes {
    env_logger::init();
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
    let server = Server::bind(&addr)
        .tcp_nodelay(true)
        .http1_max_buf_size(8192)
        .serve(make_svc);
    let server_future = rt.spawn(server);
    server_future.await;
    queuer.await?;
    Ok(())
}

////////////////////////////////////////////////////////////////
// hyper http shit

async fn serve_http(_req: Request<Body>) -> Result<Response<Body>, Infallible> {
    let sub = Subscription::new().unwrap();
    let iter = sub.map(|ele| package_jpegs(ele));
    let stream = futures_util::stream::iter(iter);
    let body = Body::wrap_stream(stream);
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        "multipart/x-mixed-replace; boundary=BoundaryString"
            .parse()
            .unwrap(),
    );
    headers.insert(
        "MAX_AGE".parse::<HeaderName>().unwrap(),
        "0".parse().unwrap(),
    );
    headers.insert(EXPIRES, "0".parse().unwrap());
    headers.insert(CACHE_CONTROL, "no-cache, private".parse().unwrap());
    headers.insert(PRAGMA, "no-cache".parse().unwrap());
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
    preamble.extend_from_slice("\r\n".as_bytes());
    debug!("Returning JPEG packaged for http stream");
    Ok(preamble)
}

struct Subscription {
    socket: Arc<Mutex<zmq::Socket>>,
    msg: zmq::Message,
    first_call: bool,
}

impl Subscription {
    pub fn new() -> Res<Self> {
        let ctx = get_context()?;
        let socket = ctx.socket(zmq::SUB)?;
        socket.connect(QUEUE_NAME)?;
        socket.set_subscribe(&[])?;
        let socket = Arc::new(Mutex::new(socket));
        let msg = zmq::Message::new();
        Ok(Self {
            socket,
            msg,
            first_call: true,
        })
    }
}

impl Iterator for Subscription {
    type Item = Res<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        // if self.first_call {
        //     debug!("First, sending cached");
        //     self.first_call = false;
        //     return Some(Ok(get_last_image()));
        // }
        if let Ok(socket) = self.socket.lock() {
            socket.recv(&mut self.msg, 0).unwrap();
            debug!(
                "Received {} bytes from queue",
                self.msg.get(..).unwrap().len()
            );
            Some(Ok(self.msg.get(..).unwrap().to_vec()))
        } else {
            None
        }
    }
}

////////////////////////////////////////////////////////////////
// wrapper b/c error catching.
async fn queue_jpegs2(ctx: zmq::Context, uri: String) -> () {
    loop {
        match queue_jpegs(&ctx, uri.clone()).await {
            Ok(_) => (),
            Err(e) => error!("Error: {:?}\nBacktrace:\n{:?}", e, e.backtrace()),
        };
    }
}

async fn queue_jpegs(ctx: &zmq::Context, uri: String) -> VoidRes {
    let mut resp = connect(&uri).await?;
    let send_socket = ctx.socket(zmq::PUB)?;
    send_socket.bind(QUEUE_NAME)?;
    let boundary_separator = get_boundary_separator(&resp)?.unwrap();
    while let Some(bytes) = read_element(&mut resp, &boundary_separator).await? {
        validate_jpeg(&bytes)?;
        debug!("Queueing");
        send_socket.send(&bytes, 0)?;
        //set_last_image(bytes.to_vec());
    }
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
            let length: usize = captures[1].to_string().parse()?;
            result.extend(&bar[preamble.len() + 2..]);
            while result.len() < length {
                let chunk = &resp.body_mut().data().await.unwrap()?[..];
                result.extend(chunk);
            }
            if result.len() != length + 2 {
                warn!("Expected length {} got {}", result.len(), length + 2);
            }
            return Ok(Some(result.to_vec()));
        }
    } else {
        return Ok(None);
    }
    Ok(Some(result))
}
