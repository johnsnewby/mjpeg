# Proxy for motion jpeg images

This proxy exists in order that I can share the my raspberry pi camera, streaming using [mjpeg over http](https://en.wikipedia.org/wiki/Motion_JPEG#M-JPEG_over_HTTP), to more clients from a machine on the internet. It works with [motion](https://motion-project.github.io/), and maybe with other sources, who knows?

It was written in order that the [PigeonCam](http://tauben.newby.org) does not use too much of my precious bandwidth.

## Features

- If there are no clients, only requests one JPEG every 30 seconds
- Caches the last JPEG for better user experience
- Maintains connections with its clients even if the back-end goes away briefly
- If you use the `?width=XX` parameter, it will resize for you. For multiple clients at the same size, the calculation is only done once.


## Requirements
Needs [zeromq](https://zeromq.org/). On Debian-y systems install with
```
apt install libzmq3-dev pkg-config
```

## Running it
```
cargo run -- -u http://webcam.local:8080
```

## Known Problems
Actually, currently none.
