use image::ImageDecoder;
use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Arc, Condvar, Mutex};

type Res<T> = crate::Res<T>;
type VoidRes = crate::VoidRes;

lazy_static! {
    static ref SCALERS: Arc<Mutex<HashMap<u16, Arc<Mutex<ScaledSource>>>>> =
        Arc::new(Mutex::new(HashMap::new()));
}

pub fn get_scaler(width: u16) -> Res<Arc<Mutex<zmq::Socket>>> {
    let mut scalers = SCALERS.lock().unwrap();
    let scaler = match scalers.get_mut(&width) {
        Some(s) => {
            debug!("Returning existing scaler for width {}", width);
            s.clone()
        }
        None => {
            debug!("Returning new scaler for width {}", width);
            let ss = ScaledSource::new(width)?;
            let client_count = ss.client_count.clone();
            let source = ss.source.clone();
            let destination = ss.destination.clone();
            std::thread::spawn(move || ScaledSource::run(client_count, source, destination, width));
            let s = Arc::new(Mutex::new(ss));
            scalers.insert(width, s.clone());
            s
        }
    };
    if let Ok(s) = scaler.lock() {
        s.client_connected()?;
        return Ok(Arc::new(Mutex::new(s.get_receive_socket()?)));
    } else {
        error!("Couldn't lock scaler");
    }
    Err(Box::new(simple_error::SimpleError::new(
        "Couldn't get scaler",
    )))
}

#[test]
fn test_scalers() {
    let scaler1 = get_scaler(1).unwrap();
}

pub fn return_scaler(width: Option<u16>) -> VoidRes {
    let mut scalers = SCALERS.lock().unwrap();
    if let Some(w) = width {
        if let Some(mutex) = scalers.get_mut(&w) {
            match mutex.lock() {
                Ok(scaler) => {
                    debug!("Disconnecting scaler at width {}", w);
                    scaler.client_disconnected()?;
                    return Ok(());
                }
                Err(e) => {
                    error!("Could not lock mutex: {}", e.to_string());
                    return Err(Box::new(simple_error::SimpleError::new(format!(
                        "Couldn't lock mutex: {}",
                        e.to_string()
                    ))));
                }
            }
        } else {
            error!("Couldn't get mutex");
            return Err(Box::new(simple_error::SimpleError::new(
                "Couldn't acquire mutex",
            )));
        }
    }
    Ok(())
}

pub struct ScaledSource {
    pub source: crate::Subscription,
    pub destination: Arc<Mutex<zmq::Socket>>,
    pub width: u16,
    pub client_count: Arc<(Mutex<u32>, Condvar)>,
}

impl ScaledSource {
    pub fn new(width: u16) -> Res<Self> {
        let socket = crate::get_context()?.socket(zmq::PUB)?;
        socket.bind(&Self::queue_name(width))?;
        let destination = Arc::new(Mutex::new(socket));
        Ok(Self {
            source: crate::Subscription::new()?,
            destination,
            width,
            client_count: Arc::new((Mutex::new(0), Condvar::new())),
        })
    }

    pub fn get_receive_socket(&self) -> Res<zmq::Socket> {
        let ctx = crate::get_context()?;
        let socket = ctx.socket(zmq::SUB)?;
        socket.connect(&Self::queue_name(self.width))?;
        socket.set_subscribe(&[])?;
        Ok(socket)
    }

    pub fn run(
        pair: Arc<(Mutex<u32>, Condvar)>,
        source: crate::Subscription,
        destination: Arc<Mutex<zmq::Socket>>,
        width: u16,
    ) {
        match Self::inner_run(pair, source, destination, width) {
            Ok(_) => (),
            Err(e) => error!("Error: {}", e.to_string()),
        }
    }

    pub fn inner_run(
        pair: Arc<(Mutex<u32>, Condvar)>,
        source: crate::Subscription,
        destination: Arc<Mutex<zmq::Socket>>,
        width: u16,
    ) -> VoidRes {
        let mut my_source = source;
        loop {
            {
                let (lock, cvar) = &*pair;
                let client_count = match lock.lock() {
                    Ok(x) => x,
                    Err(e) => {
                        return Err(Box::new(simple_error::SimpleError::new(format!(
                            "Couldn't lock mutex: {}",
                            e.to_string()
                        ))))
                    }
                };
                debug!("width: {}, client_count: {}", width, client_count);
                if *client_count == 0 {
                    drop(my_source);
                    debug!("Client count 0, waiting for notify");
                    match cvar.wait(client_count) {
                        Ok(_) => debug!("Woken up by client count > 0"),
                        Err(e) => error!("Error from wait: {}", e.to_string()),
                    }
                    my_source = crate::Subscription::new()?;
                }
            } // clients turned up!
            if let Some(Ok(img)) = my_source.next() {
                let new_image = Self::resize_image(img, width)?;
                match destination.lock() {
                    Ok(x) => x.send(&new_image, 0)?,
                    Err(e) => {
                        return Err(Box::new(simple_error::SimpleError::new(format!(
                            "Couldn't lock mutex: {}",
                            e.to_string()
                        ))))
                    }
                }
            } else {
                warn!("Couldn't get image!");
            }
        }
    }

    pub fn resize_image(img: Vec<u8>, width: u16) -> Res<Vec<u8>> {
        let mut ele: Vec<u8> = vec![];
        let decoder = image::jpeg::JpegDecoder::new(Cursor::new(img))?;
        let dimensions = decoder.dimensions();
        let scale: f32 = width as f32 / dimensions.0 as f32;
        let new_height: u16 = (dimensions.1 as f32 * scale) as u16;
        let image = image::DynamicImage::from_decoder(decoder)?.resize_exact(
            width.into(),
            new_height as u32,
            image::imageops::FilterType::Nearest,
        );
        let mut cursor = Cursor::new(ele.clone());
        let mut encoder = image::jpeg::JPEGEncoder::new(&mut cursor);
        encoder.encode(
            &image.to_bytes(),
            width.into(),
            new_height.into(),
            image.color(),
        )?;
        ele.extend_from_slice(&cursor.into_inner());
        Ok(ele)
    }

    pub fn client_connected(&self) -> VoidRes {
        let (lock, cvar) = &*self.client_count;
        if let Ok(mut client_count) = lock.lock() {
            *client_count += 1;
            cvar.notify_all();
            Ok(())
        } else {
            Err(Box::new(simple_error::SimpleError::new(
                "Couldn't lock client count",
            )))
        }
    }

    pub fn client_disconnected(&self) -> VoidRes {
        let (lock, _cvar) = &*self.client_count;
        if let Ok(mut client_count) = lock.lock() {
            *client_count -= 1;
            Ok(())
        } else {
            Err(Box::new(simple_error::SimpleError::new(
                "Could not lock CLIENT_COUNT",
            )))
        }
    }

    fn queue_name(width: u16) -> String {
        let name = format!("inproc://scale{}", width);
        name
    }
}
