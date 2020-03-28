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
use once_cell::sync::OnceCell;
use std::borrow::Cow;
use std::io::Cursor;
use std::iter::Iterator;
use std::net::SocketAddr;
use std::sync::{Arc, Condvar, Mutex};

type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
type VoidRes = Res<()>;

const PORT: u16 = 8888;
const BIND_ADDRESS: [u8; 4] = [127, 0, 0, 1];
const QUEUE_NAME: &str = "inproc://jpegs";

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
    static ref CLIENT_COUNT: Arc<(Mutex<u32>, Condvar)> =
        Arc::new((Mutex::new(0u32), Condvar::new()));
}

fn client_connected() {
    let (lock, cvar) = &*(*CLIENT_COUNT);
    if let Ok(mut client_count) = lock.lock() {
        *client_count += 1;
        cvar.notify_all();
    } else {
        error!("Could not lock CLIENT_COUNT");
    }
}

fn client_disconnected() {
    let (lock, _cvar) = &*(*CLIENT_COUNT);
    if let Ok(mut client_count) = lock.lock() {
        *client_count -= 1;
    } else {
        error!("Could not lock CLIENT_COUNT");
    }
}

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
    let addr = std::net::SocketAddr::new(ip_addr, port);
    let queuer = rt.spawn(queue_jpegs2(ctx.clone(), uri));
    let config = tiny_http::ServerConfig::<SocketAddr> { addr, ssl: None };
    let server = tiny_http::Server::new(config).unwrap();

    while let Ok(request) = server.recv() {
        std::thread::spawn(move || handle_request_outer(request));
    }
    queuer.await?;
    Ok(())
}

fn handle_request_outer(request: tiny_http::Request) {
    client_connected();
    let _f = finally_block::finally(client_disconnected);
    let url = url::Url::parse(&format!("http://example.com{}", request.url())).unwrap();
    let width = match url.query_pairs().find(|x| x.0 == Cow::Borrowed("width")) {
        Some(x) => Some(x.1.to_string().parse::<u16>().unwrap()),
        None => None,
    };
    match handle_request(request, width) {
        Ok(_) => (),
        Err(e) => warn!("Error handling request: {:?}", e),
    }
}

fn handle_request(request: tiny_http::Request, width: Option<u16>) -> VoidRes {
    let sub = Subscription::new()?;
    let iter = sub.map(move |x| package_jpegs(x, width));
    let mut writer = request.into_writer();
    let headers = "HTTP/1.0 200 OK\r\nServer: Motion/4.1.1\r\nConnection: close\r\nMax-Age: 0\r\nExpires: 0\r\nCache-Control: no-cache, private\r\nPragma: no-cache\r\nContent-Type: multipart/x-mixed-replace; boundary=BoundaryString\r\n\r\n";
    writer.write_all(String::from(headers).as_bytes())?;
    for ele in iter {
        writer.write_all(&ele?)?;
    }
    debug!("Iterator ended, ending http stream");
    Ok(())
}

fn package_jpegs(jpeg: Res<Vec<u8>>, width: Option<u16>) -> Res<Vec<u8>> {
    let mut ele = jpeg.unwrap();
    if let Some(new_width) = width {
        let decoder = image::jpeg::JpegDecoder::new(Cursor::new(ele.clone()))?;
        let dimensions = decoder.dimensions();
        let scale: f32 = new_width as f32 / dimensions.0 as f32;
        let new_height: u16 = (dimensions.1 as f32 * scale) as u16;
        if scale != 1.0 {
            debug!("Resizing to width {}", new_width);
            let image = image::DynamicImage::from_decoder(decoder)?.resize_exact(
                new_width.into(),
                new_height as u32,
                image::imageops::FilterType::Nearest,
            );
            ele.clear();
            let mut cursor = Cursor::new(ele.clone());
            let mut encoder = image::jpeg::JPEGEncoder::new(&mut cursor);
            encoder.encode(
                &image.to_bytes(),
                new_width.into(),
                new_height.into(),
                image.color(),
            )?;
            ele.extend_from_slice(&cursor.into_inner());
        }
    }
    let len = ele.len();
    let mut preamble: Vec<u8> = format!(
        "--BoundaryString\r\nContent-type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
        len
    )
    .into_bytes();
    preamble.extend_from_slice(&ele);
    preamble.extend_from_slice(b"\r\n");
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
async fn queue_jpegs2(ctx: zmq::Context, uri: String) {
    let send_socket = ctx.socket(zmq::PUB).unwrap();
    send_socket.bind(QUEUE_NAME).unwrap();
    let ss_mut = Arc::new(Mutex::new(send_socket));
    loop {
        match queue_jpegs(&ctx, uri.clone(), &ss_mut).await {
            Ok(_) => (),
            Err(e) => error!("Error: {:?}\nBacktrace:\n{:?}", e, e.backtrace()),
        };
        {
            let (lock, cvar) = &*(*CLIENT_COUNT);
            let client_count = lock.lock().unwrap();
            if *client_count != 0 {
                continue;
            }
            debug!(
                "Client count is {}, waiting for condition variable",
                *client_count
            );
            match cvar.wait_timeout(client_count, std::time::Duration::from_secs(30)) {
                Ok((_, result)) => {
                    if !result.timed_out() {
                        debug!("Notify!");
                    }
                }
                Err(e) => error!("Error waiting for clients {}", e.to_string()),
            }
        }
    }
}

async fn queue_jpegs(
    _ctx: &zmq::Context,
    uri: String,
    send_socket: &Arc<Mutex<zmq::Socket>>,
) -> VoidRes {
    let pair = (*CLIENT_COUNT).clone();
    let mut resp = connect(&uri).await?;
    let boundary_separator = get_boundary_separator(&resp)?.unwrap();
    let mut buffer: Vec<u8> = vec![];
    while let Some(mut bytes) = read_element(&mut resp, &boundary_separator, &mut buffer).await? {
        bytes = validate_jpeg(bytes)?;
        debug!("Queueing");
        send_socket.lock().unwrap().send(&bytes, 0)?;
        set_last_image(bytes.to_vec());
        let (lock, _) = &*pair;
        match lock.lock() {
            Ok(x) => {
                debug!("Client count is {}", x);
                if *x == 0 {
                    debug!("No clients, returning");
                    // quit if no clients.
                    return Ok(());
                }
            }
            Err(e) => {
                return Err(Box::new(simple_error::SimpleError::new(format!(
                    "Error accessing condition variable: {}",
                    e.to_string()
                ))))
            }
        }
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
    for val in content_type.to_str()?.to_string().split(';') {
        if let Some(captures) = re.captures(val) {
            return Ok(Some(captures[1].to_string()));
        }
    }
    Ok(None)
}

use image::ImageDecoder;
fn validate_jpeg(bytes: Vec<u8>) -> Res<Vec<u8>> {
    let decoder = image::jpeg::JpegDecoder::new(Cursor::new(bytes.clone()))?;
    trace!(
        "Dimensions: {} {}",
        decoder.dimensions().0,
        decoder.dimensions().1,
    );
    Ok(bytes)
}

async fn read_element(
    resp: &mut Response<hyper::Body>,
    boundary_separator: &str,
    buffer: &mut Vec<u8>,
) -> Res<Option<Vec<u8>>> {
    trace!(
        "read_element: buffer is {}",
        String::from_utf8_lossy(buffer)
    );
    let re = regex::Regex::new(&format!(
        "--{}\r\nContent-type: image/jpeg\\s+Content-Length:\\s+(\\d+)\r\n\r\n",
        boundary_separator
    ))?;
    let mut result: Vec<u8> = vec![];
    if let Some(chunk) = resp.body_mut().data().await {
        buffer.extend_from_slice(&chunk?);
        let as_str = String::from_utf8_lossy(&buffer);
        if let Some(captures) = re.captures(&as_str) {
            let preamble = &captures[0];
            let mut length: usize = captures[1].to_string().parse()?;
            length += 2; // CRLF
            trace!("Preamble: {}", preamble);
            trace!("Length: {}", length);
            let preamble_length = preamble.len();
            result.extend(&buffer[preamble_length..]);
            trace!("{:?}", result);
            while result.len() < length {
                let chunk = &resp.body_mut().data().await.unwrap()?[..];
                result.extend(chunk);
            }
            buffer.clear();
            if result.len() > length {
                buffer.extend(result[length..].to_vec());
                result.truncate(length);
                trace!("Leftover input, passing {:?} to next invocation", buffer);
            }
            return Ok(Some(result.to_vec()));
        } else {
            warn!("Failed to match, input was {}", as_str);
        }
    } else {
        return Ok(None);
    }
    Ok(Some(result))
}
