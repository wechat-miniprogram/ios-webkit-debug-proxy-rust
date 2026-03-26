# ios-webkit-debug-proxy

A rust rewrite of [ios-webkit-debug-proxy](https://github.com/google/ios-webkit-debug-proxy).

Notice: this is an AI-assisted rewrite which has not been heavily tested.

[![Crates.io](https://img.shields.io/crates/v/ios-webkit-debug-proxy)](https://crates.io/crates/ios-webkit-debug-proxy)
[![docs.rs](https://img.shields.io/docsrs/ios-webkit-debug-proxy)](https://docs.rs/ios-webkit-debug-proxy)


## How to Build

### System Dependencies

`libimobiledevice` `libusbmuxd` must be installed in your system.

For macOS:

```sh
brew install libimobiledevice libusbmuxd
```

For Linux (Debian/Ubuntu as example):

```sh
sudo apt install libimobiledevice-dev libusbmuxd-dev
```

For Windows:

Install via [MSYS2/MinGW](https://www.msys2.org/) or [vcpkg](https://vcpkg.io/), or build `libimobiledevice` and `libusbmuxd` from source.

### Run

Then run normal `cargo` commands to build or run.

It can also be used as a rust lib.


## Rewrite Details

Keeps main logic of the original C implementation.

The original C implementation contains manual common protocol implementation, such as `sha1` `hash_table` and HTTP. They are replaced by common rust libs. `OpenSSL` are replaced by `rustls` for easier compilation.


## LICENSE

Both the original license and MIT.
