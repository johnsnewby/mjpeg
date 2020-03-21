# Proxy for motion jpeg images

This proxy exists in order that I can share the my raspberry pi camera, streaming using [mjpeg over http](https://en.wikipedia.org/wiki/Motion_JPEG#M-JPEG_over_HTTP), to more clients from a machine on the internet. It works with [motion](https://motion-project.github.io/), and maybe with other sources, who knows?

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
- May or may not crap out after a while
- Still fetches a feed in the absence of any clients
- 8MB binary just for this!
