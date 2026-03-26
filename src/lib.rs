// iOS WebKit Debug Proxy - Rust rewrite
// Based on Google BSD license https://developers.google.com/google-bsd-license
// Original author Copyright 2012 Google Inc. wrightt@google.com

pub(crate) mod device_listener;
pub mod error;
pub(crate) mod http_server;
pub(crate) mod idevice_ext;
pub(crate) mod proxy;
pub(crate) mod rpc;
pub(crate) mod webinspector;

pub use error::{Error, Result};
pub use proxy::{HttpServiceConfig, ProxyBridge};

pub const DEFAULT_SIM_WI_SOCKET_ADDR: &str = "127.0.0.1:27753";

/// Device state change listener
pub trait DeviceListener: Send + Sync {
    /// Device connected
    fn device_connected(&self, device_info: DeviceInfo);

    /// Device disconnected
    fn device_disconnected(&self, device_info: DeviceInfo);

    /// Device page list changed
    fn device_page_list_changed(&self, device_info: DeviceInfo, pages: Vec<DebuggablePageInfo>);
}

/// Device info (for device list display)
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub device_id: String,
    pub device_name: String,
    pub device_os_version: u32,
    pub port: u16,
}

/// Debuggable page info
#[derive(Debug, Clone)]
pub struct DebuggablePageInfo {
    pub device_id: String,
    pub page_num: u32,
    pub page_type: PageType,
    pub name: String,
    pub web_socket_debugger_url: String,
    pub device_os_version: u32,
}

/// Page type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageType {
    Unknown,
    JavaScript,
    WebPage,
}

pub fn parse_device_os_version(version: u32) -> (u32, u32, u32) {
    let major = version >> 16;
    let minor = (version >> 8) & 0xff;
    let patch = version & 0xff;
    (major, minor, patch)
}
