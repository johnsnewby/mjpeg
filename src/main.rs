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
use hyper::{Client, Response, Uri};
use image::ImageEncoder;
use once_cell::sync::OnceCell;
use std::io::Cursor;
use std::iter::Iterator;
use std::net::SocketAddr;
use std::sync::atomic::AtomicU32;
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

lazy_static! {
    static ref CLIENT_COUNT: AtomicU32 = AtomicU32::new(0u32);
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
                            (@arg PORT: -p --port +takes_value "Port for http server to listen on")
                            (@arg BIND: -b --bind-address +takes_value "Address to bind http server to")
                            (@arg CROP: -c --crop +takes_value "Crop, in form x,y,width,height (px)")
    )
    .get_matches();
    let uri: String = String::from(matches.value_of("URL").unwrap());
    let ip_addr: std::net::IpAddr = {
        if let Some(bind_address) = matches.value_of("BIND") {
            String::from(bind_address).parse().unwrap()
        } else {
            BIND_ADDRESS.into()
        }
    };
    let port: u16 = {
        if let Some(port) = matches.value_of("PORT") {
            String::from(port).parse().unwrap()
        } else {
            PORT
        }
    };
    let mut crop_param: Option<(u32, u32, u32, u32)> = None;
    if let Some(crop) = matches.value_of("CROP") {
        let d: Vec<u32> = String::from(crop)
            .split(",")
            .map(|x| x.parse::<u32>().unwrap())
            .collect();
        if d.len() != 4 {
            panic!("Dimensions in form x,y,w,h");
        }
        crop_param = Some((d[0], d[1], d[2], d[3]));
    }
    let addr = std::net::SocketAddr::new(ip_addr, port);
    let queuer = rt.spawn(queue_jpegs2(ctx.clone(), uri, crop_param));
    let config = tiny_http::ServerConfig::<SocketAddr> { addr, ssl: None };
    let server = tiny_http::Server::new(config).unwrap();

    loop {
        if let Ok(request) = server.recv() {
            std::thread::spawn(move || handle_request_outer(request));
        } else {
            break;
        }
    }
    queuer.await?;
    Ok(())
}

fn handle_request_outer(request: tiny_http::Request) {
    match handle_request(request) {
        Ok(_) => (),
        Err(e) => warn!("Error handling request: {:?}", e),
    }
}

fn handle_request(request: tiny_http::Request) -> VoidRes {
    let sub = Subscription::new()?;
    let iter = sub.map(|ele| package_jpegs(ele));
    let mut writer = request.into_writer();
    let headers = "HTTP/1.0 200 OK\r\nServer: Motion/4.1.1\r\nConnection: close\r\nMax-Age: 0\r\nExpires: 0\r\nCache-Control: no-cache, private\r\nPragma: no-cache\r\nContent-Type: multipart/x-mixed-replace; boundary=BoundaryString\r\n\r\n";
    writer.write(String::from(headers).as_bytes())?;
    for ele in iter {
        writer.write(&ele?)?;
    }
    debug!("Iterator ended, ending http stream");
    Ok(())
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
        if self.first_call {
            debug!("First, sending cached");
            self.first_call = false;
            return Some(Ok(get_last_image()));
        }
        if let Ok(socket) = self.socket.lock() {
            socket.recv(&mut self.msg, 0).unwrap();
            debug!(
                "Received {} bytes from queue",
                self.msg.get(..).unwrap().len()
            );
            Some(Ok(self.msg.get(..).unwrap().to_vec()))
        } else {
            warn!("Failed to acquire lock");
            None
        }
    }
}

////////////////////////////////////////////////////////////////
// wrapper b/c error catching.
async fn queue_jpegs2(ctx: zmq::Context, uri: String, crop: Option<(u32, u32, u32, u32)>) -> () {
    loop {
        match queue_jpegs(&ctx, uri.clone(), crop).await {
            Ok(_) => (),
            Err(e) => error!("Error: {:?}\nBacktrace:\n{:?}", e, e.backtrace()),
        };
    }
}

async fn queue_jpegs(
    ctx: &zmq::Context,
    uri: String,
    crop: Option<(u32, u32, u32, u32)>,
) -> VoidRes {
    let mut resp = connect(&uri).await?;
    let send_socket = ctx.socket(zmq::PUB)?;
    send_socket.bind(QUEUE_NAME)?;
    let boundary_separator = get_boundary_separator(&resp)?.unwrap();
    while let Some(mut bytes) = read_element(&mut resp, &boundary_separator).await? {
        bytes = validate_and_crop_jpeg(bytes, crop)?;
        debug!("Queueing");
        send_socket.send(&bytes, 0)?;
        set_last_image(bytes.to_vec());
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

fn validate_and_crop_jpeg(bytes: Vec<u8>, crop: Option<(u32, u32, u32, u32)>) -> Res<Vec<u8>> {
    let mut bytes = bytes;
    let mut image = image::load_from_memory(&bytes)?; // sanity check
    if let Some(c) = crop {
        image = image.crop(c.0, c.1, c.2, c.3);
        let new_image = vec![];
        let mut cursor = Cursor::new(new_image);
        let encoder = image::jpeg::JPEGEncoder::new(&mut cursor);
        encoder.write_image(&image.to_bytes(), c.2, c.3, image.color())?;
        bytes = cursor.into_inner();
    }
    Ok(bytes)
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
        let bar = chunk?;
        let foo = String::from_utf8_lossy(&bar);
        if let Some(captures) = re.captures(&foo) {
            let preamble = &captures[0];
            let length: usize = captures[1].to_string().parse()?;
            debug!("Preamble: {}", preamble);
	    debug!("Length: {}", length);
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
