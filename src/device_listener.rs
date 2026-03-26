// Device listener module
// Corresponds to the original C code device_listener.c
// Listens for iOS device connect/disconnect events via usbmuxd socket
// Uses tokio async I/O

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use log::{debug, info, warn};
use std::collections::HashMap;
use std::io::Cursor;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::error::{Error, Result};

// macOS/Linux: connect to usbmuxd via Unix socket
#[cfg(not(target_os = "windows"))]
use std::path::Path;
#[cfg(not(target_os = "windows"))]
use tokio::net::UnixStream;

// Windows: connect to usbmuxd via TCP
#[cfg(target_os = "windows")]
use tokio::net::TcpStream;

#[cfg(not(target_os = "windows"))]
const USBMUXD_FILE_PATH: &str = "/var/run/usbmuxd";

#[cfg(target_os = "windows")]
const USBMUXD_SOCKET_PORT: u16 = 27015;

const TYPE_PLIST: u32 = 8;
const LIBUSBMUX_VERSION: u32 = 3;

/// Reconnect delay (seconds)
const RECONNECT_DELAY_SECS: u64 = 2;

/// Device event type
#[derive(Debug, Clone)]
pub(crate) enum DeviceEvent {
    Attached {
        device_id: String,
        #[allow(dead_code)]
        device_num: i32,
    },
    Detached {
        device_id: String,
        #[allow(dead_code)]
        device_num: i32,
    },
}

/// Device listener (internal protocol parsing state)
struct DeviceListenerParser {
    /// Known device mapping: device_num -> device_id
    device_num_to_id: HashMap<u64, String>,
    /// Input buffer
    buffer: Vec<u8>,
    /// Whether the length has been read
    has_length: bool,
    /// Current message body length
    body_length: usize,
}

impl DeviceListenerParser {
    fn new() -> Self {
        DeviceListenerParser {
            device_num_to_id: HashMap::new(),
            buffer: Vec::new(),
            has_length: false,
            body_length: 0,
        }
    }

    /// Process received data
    fn on_recv(&mut self, data: &[u8]) -> Result<Vec<DeviceEvent>> {
        self.buffer.extend_from_slice(data);
        let mut events = Vec::new();

        loop {
            if !self.has_length && self.buffer.len() >= 4 {
                let mut cursor = Cursor::new(&self.buffer[..4]);
                self.body_length = cursor.read_u32::<LittleEndian>().unwrap() as usize;
                self.has_length = true;
            }

            if self.has_length && self.buffer.len() >= self.body_length {
                let packet: Vec<u8> = self.buffer.drain(..self.body_length).collect();
                self.has_length = false;

                match self.parse_packet(&packet) {
                    Ok(Some(event)) => events.push(event),
                    Ok(None) => {} // Result ack or unknown message
                    Err(e) => {
                        warn!("Failed to parse packet: {}", e);
                    }
                }
            } else {
                break;
            }
        }

        Ok(events)
    }

    /// Parse a single packet
    fn parse_packet(&mut self, packet: &[u8]) -> Result<Option<DeviceEvent>> {
        if packet.len() < 16 {
            return Err(Error::Usbmuxd("Packet too short".to_string()));
        }

        let mut cursor = Cursor::new(packet);
        let len = cursor.read_u32::<LittleEndian>().unwrap() as usize;
        let version = cursor.read_u32::<LittleEndian>().unwrap();
        let ptype = cursor.read_u32::<LittleEndian>().unwrap();
        let _tag = cursor.read_u32::<LittleEndian>().unwrap();

        if len != packet.len() {
            return Err(Error::Usbmuxd(format!("Length mismatch: {} != {}", len, packet.len())));
        }

        if version != 1 || ptype != TYPE_PLIST {
            return Ok(None); // Ignore non-plist messages
        }

        let xml = &packet[16..];
        let xml_str = String::from_utf8_lossy(xml);

        // Extract key information using simple XML parsing
        self.parse_plist_message(&xml_str)
    }

    /// Parse plist message
    fn parse_plist_message(&mut self, xml: &str) -> Result<Option<DeviceEvent>> {
        // Parse plist
        let dict: plist::Value =
            plist::from_bytes(xml.as_bytes()).map_err(|e| Error::Plist(format!("plist parsing failed: {}", e)))?;

        let dict = dict.as_dictionary().ok_or(Error::Plist("Not a dictionary type".to_string()))?;

        let message_type = dict
            .get("MessageType")
            .and_then(|v| v.as_string())
            .unwrap_or("");

        match message_type {
            "Result" => {
                if let Some(number) = dict.get("Number").and_then(|v| v.as_unsigned_integer()) {
                    if number != 0 {
                        return Err(Error::Usbmuxd(format!("Listen request failed, error code: {}", number)));
                    }
                }
                debug!("Listen request succeeded");
                Ok(None)
            }
            "Attached" => {
                if let Some(props) = dict.get("Properties").and_then(|v| v.as_dictionary()) {
                    let device_num = props
                        .get("DeviceID")
                        .and_then(|v| v.as_unsigned_integer())
                        .unwrap_or(0);

                    let mut device_id = props
                        .get("SerialNumber")
                        .and_then(|v| v.as_string())
                        .unwrap_or("")
                        .to_string();

                    // If the serial number is 24 characters, insert a dash to make it standard format
                    if device_id.len() == 24 {
                        device_id = format!("{}-{}", &device_id[..8], &device_id[8..]);
                    }

                    self.device_num_to_id.insert(device_num, device_id.clone());

                    Ok(Some(DeviceEvent::Attached {
                        device_id,
                        device_num: device_num as i32,
                    }))
                } else {
                    Err(Error::Usbmuxd("Missing Properties".to_string()))
                }
            }
            "Detached" => {
                if let Some(device_num) = dict.get("DeviceID").and_then(|v| v.as_unsigned_integer())
                {
                    if let Some(device_id) = self.device_num_to_id.remove(&device_num) {
                        Ok(Some(DeviceEvent::Detached {
                            device_id,
                            device_num: device_num as i32,
                        }))
                    } else {
                        warn!("Unknown device number {} detached", device_num);
                        Ok(None)
                    }
                } else {
                    Err(Error::Usbmuxd("Missing DeviceID".to_string()))
                }
            }
            _ => {
                debug!("Unknown message type: {}", message_type);
                Ok(None)
            }
        }
    }
}

/// Create Listen request packet
fn create_listen_packet() -> Vec<u8> {
    // Build plist XML
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>ClientVersionString</key>
	<string>device_listener</string>
	<key>MessageType</key>
	<string>Listen</string>
	<key>ProgName</key>
	<string>libusbmuxd</string>
	<key>kLibUSBMuxVersion</key>
	<integer>{}</integer>
</dict>
</plist>"#,
        LIBUSBMUX_VERSION
    );

    let xml_bytes = xml.as_bytes();
    let length = 16 + xml_bytes.len();

    let mut packet = Vec::with_capacity(length);
    // length (little-endian)
    packet.write_u32::<LittleEndian>(length as u32).unwrap();
    // version: 1
    packet.write_u32::<LittleEndian>(1).unwrap();
    // type: plist
    packet.write_u32::<LittleEndian>(TYPE_PLIST).unwrap();
    // tag: 1
    packet.write_u32::<LittleEndian>(1).unwrap();
    // xml body
    packet.extend_from_slice(xml_bytes);

    packet
}

/// Async connect to usbmuxd
/// macOS/Linux: connects via Unix domain socket
/// Windows: connects via TCP localhost:27015
#[cfg(not(target_os = "windows"))]
async fn connect_usbmuxd() -> std::result::Result<impl AsyncRead + AsyncWrite + Unpin, std::io::Error> {
    let path = Path::new(USBMUXD_FILE_PATH);
    if !path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("usbmuxd socket does not exist: {}", USBMUXD_FILE_PATH),
        ));
    }
    UnixStream::connect(path).await
}

#[cfg(target_os = "windows")]
async fn connect_usbmuxd() -> std::result::Result<impl AsyncRead + AsyncWrite + Unpin, std::io::Error> {
    TcpStream::connect(("localhost", USBMUXD_SOCKET_PORT)).await
}

/// Start device listener async task
///
/// Returns an mpsc::Receiver for receiving device events.
/// The task runs in the background and auto-reconnects on disconnection.
pub(crate) fn spawn_device_listener(
    cancellation: tokio_util::sync::CancellationToken,
) -> mpsc::Receiver<DeviceEvent> {
    let (tx, rx) = mpsc::channel(64);

    tokio::spawn(async move {
        loop {
            if cancellation.is_cancelled() {
                break;
            }

            match run_device_listener(&tx, &cancellation).await {
                Ok(()) => {
                    // Normal exit (cancelled)
                    break;
                }
                Err(e) => {
                    warn!(
                        "Device listener disconnected: {}, reconnecting in {} seconds...",
                        e, RECONNECT_DELAY_SECS
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(RECONNECT_DELAY_SECS)) => {}
                        _ = cancellation.cancelled() => { break; }
                    }
                }
            }
        }
        debug!("Device listener task exited");
    });

    rx
}

/// Run a single device listener connection
async fn run_device_listener(
    tx: &mpsc::Sender<DeviceEvent>,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = connect_usbmuxd()
        .await
        .map_err(|e| Error::Usbmuxd(format!("Failed to connect to usbmuxd: {}", e)))?;

    // Send Listen request
    let packet = create_listen_packet();
    stream
        .write_all(&packet)
        .await
        .map_err(|e| Error::Usbmuxd(format!("Failed to send Listen request: {}", e)))?;

    info!("Connected to usbmuxd, listening for device events");

    let mut parser = DeviceListenerParser::new();
    let mut buf = vec![0u8; 4096];

    loop {
        tokio::select! {
            result = stream.read(&mut buf) => {
                match result {
                    Ok(0) => {
                        return Err(Error::Usbmuxd("usbmuxd connection closed".to_string()));
                    }
                    Ok(n) => {
                        match parser.on_recv(&buf[..n]) {
                            Ok(events) => {
                                for event in events {
                                    if tx.send(event).await.is_err() {
                                        // Receiver closed
                                        return Ok(());
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("Failed to parse device event: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        return Err(Error::Usbmuxd(format!("Failed to read usbmuxd data: {}", e)));
                    }
                }
            }
            _ = cancellation.cancelled() => {
                return Ok(());
            }
        }
    }
}
