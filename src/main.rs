#[macro_use]
extern crate clap;
extern crate hyper;
use hyper::body::HttpBody;
use hyper::{Client, Request, Response, Server, Uri};
use std::io::Cursor;
//use tokio::io::{stdout, AsyncWriteExt as _};
type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
type VoidRes = Res<()>;

const QUEUE_NAME: &'static str = "inproc://jpegs";

#[tokio::main]
async fn main() -> VoidRes {
    let ctx = zmq::Context::new();
    let mut rt = tokio::runtime::Runtime::new()?;
    let matches = clap_app!(mjpeg_proxy =>
                            (version: "0.0")
                            (author: "@johnsnewby")
                            (@arg URL: -u --url +required +takes_value "URL to load")
    )
    .get_matches();
    let uri: String = String::from(matches.value_of("URL").unwrap());
    let queuer = rt.spawn(queue_jpegs2(ctx.clone(), uri));
    println!("pre-ok");
    get_jpegs(&ctx)?;
    println!("OK");
    queuer.await;
    Ok(())
}

fn get_jpegs(ctx: &zmq::Context) -> VoidRes {
    let recv_socket = ctx.socket(zmq::SUB)?;
    recv_socket.connect(QUEUE_NAME)?;
    recv_socket.set_subscribe(&[])?;
    let mut msg = zmq::Message::with_size(200000);
    println!("1");
    recv_socket.recv(&mut msg, 200000)?;
    println!("2");
    let bytes: &[u8] = msg.get(..).unwrap();
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
    let decoder = image::jpeg::JpegDecoder::new(Cursor::new(bytes))?;
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
