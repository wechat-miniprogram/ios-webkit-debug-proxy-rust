// WebInspector module
// Corresponds to the original C code webinspector.c
// Handles iOS WebInspector protocol packet sending and receiving
// Uses tokio async I/O for read/write

use log::{debug, warn};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::error::{Error, Result};

/// Maximum RPC length (for fragmentation)
const MAX_RPC_LEN: usize = 8096 - 500;

/// Maximum message body length (prevents malicious packets)
const MAX_BODY_LENGTH: usize = 1 << 26;

/// WebInspector received event
pub(crate) enum WiEvent {
    /// Received a complete RPC plist
    RecvPlist(plist::Value),
}

/// WebInspector protocol encoder (pure data processing, no I/O)
pub(crate) struct WebInspectorCodec {
    /// Whether this is a simulator
    is_sim: bool,
    /// Whether partial messages are supported
    partials_supported: bool,
    /// Partial message buffer
    partial_buffer: Vec<u8>,
}

impl WebInspectorCodec {
    pub(crate) fn new(is_sim: bool, partials_supported: bool) -> Self {
        WebInspectorCodec {
            is_sim,
            partials_supported,
            partial_buffer: Vec::new(),
        }
    }

    /// Build send packet (prepend 4-byte big-endian length header)
    fn build_packet(data: &[u8]) -> Vec<u8> {
        let data_len = data.len();
        let mut packet = Vec::with_capacity(data_len + 4);
        // Big-endian 4-byte length
        packet.push(((data_len >> 24) & 0xFF) as u8);
        packet.push(((data_len >> 16) & 0xFF) as u8);
        packet.push(((data_len >> 8) & 0xFF) as u8);
        packet.push((data_len & 0xFF) as u8);
        packet.extend_from_slice(data);
        packet
    }

    /// Encode plist message into a list of packets to send
    pub(crate) fn encode_plist(&self, rpc_bin: &[u8]) -> Vec<Vec<u8>> {
        let mut packets = Vec::new();

        if !self.partials_supported {
            // Send complete message directly
            let packet = Self::build_packet(rpc_bin);
            packets.push(packet);

            if !self.is_sim {
                // Send empty packet to facilitate processing
                let empty_packet = Self::build_packet(&[]);
                packets.push(empty_packet);
            }
            return packets;
        }

        // When fragmentation is supported, split by MAX_RPC_LEN
        let mut offset = 0;
        loop {
            let is_partial = rpc_bin.len() - offset > MAX_RPC_LEN;
            let chunk_end = if is_partial {
                offset + MAX_RPC_LEN
            } else {
                rpc_bin.len()
            };
            let chunk = &rpc_bin[offset..chunk_end];

            // Wrap in WIR*MessageKey
            let key = if is_partial {
                "WIRPartialMessageKey"
            } else {
                "WIRFinalMessageKey"
            };

            let mut wi_dict = plist::Dictionary::new();
            wi_dict.insert(key.to_string(), plist::Value::Data(chunk.to_vec()));
            let wi_value = plist::Value::Dictionary(wi_dict);

            let mut data = Vec::new();
            wi_value
                .to_writer_binary(&mut data)
                .expect("plist serialization failed");

            let packet = Self::build_packet(&data);
            packets.push(packet);

            if !is_partial {
                break;
            }
            offset = chunk_end;
        }

        packets
    }

    /// Parse 4-byte big-endian length
    fn parse_length(buf: &[u8]) -> Result<usize> {
        if buf.len() < 4 {
            return Err(Error::WebInspector("Insufficient data to parse length".to_string()));
        }
        let length = ((buf[0] as usize) << 24)
            | ((buf[1] as usize) << 16)
            | ((buf[2] as usize) << 8)
            | (buf[3] as usize);

        if MAX_BODY_LENGTH > 0 && length > MAX_BODY_LENGTH {
            return Err(Error::WebInspector(format!("Invalid packet header: length {} exceeds maximum", length)));
        }
        Ok(length)
    }

    /// Parse plist data, handle fragmentation
    fn parse_plist(&mut self, data: &[u8]) -> Result<Option<plist::Value>> {
        if !self.partials_supported {
            let value: plist::Value =
                plist::from_bytes(data).map_err(|e| Error::Plist(format!("plist parsing failed: {}", e)))?;
            return Ok(Some(value));
        }

        // Parse outer wrapper
        let wi_dict: plist::Value =
            plist::from_bytes(data).map_err(|e| Error::Plist(format!("plist parsing failed: {}", e)))?;

        let wi_dict = wi_dict.as_dictionary().ok_or(Error::WebInspector("Outer wrapper is not a dictionary".to_string()))?;

        // Check if it's a complete or partial message
        let (rpc_data, is_partial) = if let Some(rpc) = wi_dict.get("WIRFinalMessageKey") {
            let data = rpc.as_data().ok_or(Error::WebInspector("WIRFinalMessageKey is not data type".to_string()))?;
            (data.to_vec(), false)
        } else if let Some(rpc) = wi_dict.get("WIRPartialMessageKey") {
            let data = rpc.as_data().ok_or(Error::WebInspector("WIRPartialMessageKey is not data type".to_string()))?;
            (data.to_vec(), true)
        } else {
            return Err(Error::WebInspector("Missing WIRFinalMessageKey or WIRPartialMessageKey".to_string()));
        };

        // Append to partial buffer
        if is_partial || !self.partial_buffer.is_empty() {
            self.partial_buffer.extend_from_slice(&rpc_data);
            if is_partial {
                return Ok(None); // Waiting for more data
            }
        }

        // Parse the complete RPC plist
        let full_data = if !self.partial_buffer.is_empty() {
            std::mem::take(&mut self.partial_buffer)
        } else {
            rpc_data
        };

        let rpc_value: plist::Value =
            plist::from_bytes(&full_data).map_err(|e| Error::Plist(format!("RPC plist parsing failed: {}", e)))?;

        Ok(Some(rpc_value))
    }

    /// Decode a complete packet body (without length header) into an event
    fn decode_body(&mut self, body: &[u8]) -> Result<Option<WiEvent>> {
        match self.parse_plist(body)? {
            Some(rpc_dict) => Ok(Some(WiEvent::RecvPlist(rpc_dict))),
            None => Ok(None), // Partial message, continue waiting
        }
    }
}

/// WebInspector async connection manager
///
/// Encapsulates read/write separated async tasks:
/// - Read task: reads data from device stream, decodes and sends WiEvent via mpsc channel
/// - Write channel: sends packets to write via mpsc::Sender
pub(crate) struct WebInspectorHandle {
    /// Send data to WebInspector (already encoded raw bytes)
    pub(crate) write_tx: mpsc::Sender<Vec<u8>>,
}

impl WebInspectorHandle {
    /// Send plist message to WebInspector
    pub(crate) async fn send_plist(
        &self,
        codec: &WebInspectorCodec,
        rpc_bin: &[u8],
    ) -> Result<()> {
        let packets = codec.encode_plist(rpc_bin);
        for packet in packets {
            self.write_tx
                .send(packet)
                .await
                .map_err(|_| Error::ChannelClosed)?;
        }
        Ok(())
    }
}

/// Start WebInspector async read/write tasks
///
/// Returns (WebInspectorHandle, mpsc::Receiver<WiEvent>)
/// - handle: for sending data to WebInspector
/// - event_rx: for receiving decoded events from WebInspector
pub(crate) fn spawn_webinspector<S>(
    stream: S,
    is_sim: bool,
    partials_supported: bool,
    cancellation: tokio_util::sync::CancellationToken,
) -> (WebInspectorHandle, mpsc::Receiver<WiEvent>)
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let (event_tx, event_rx) = mpsc::channel(64);
    let (write_tx, write_rx) = mpsc::channel::<Vec<u8>>(64);

    let (read_half, write_half) = tokio::io::split(stream);

    // Start read task
    let read_cancel = cancellation.clone();
    tokio::spawn(async move {
        if let Err(e) =
            wi_read_loop(read_half, is_sim, partials_supported, event_tx, read_cancel).await
        {
            debug!("WebInspector read task ended: {}", e);
        }
    });

    // Start write task
    let write_cancel = cancellation;
    tokio::spawn(async move {
        if let Err(e) = wi_write_loop(write_half, write_rx, write_cancel).await {
            debug!("WebInspector write task ended: {}", e);
        }
    });

    let handle = WebInspectorHandle { write_tx };
    (handle, event_rx)
}

/// WebInspector read loop
async fn wi_read_loop<R: AsyncRead + Unpin>(
    mut reader: R,
    is_sim: bool,
    partials_supported: bool,
    event_tx: mpsc::Sender<WiEvent>,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let mut codec = WebInspectorCodec::new(is_sim, partials_supported);
    let mut len_buf = [0u8; 4];

    loop {
        // Read 4-byte big-endian length header
        tokio::select! {
            result = reader.read_exact(&mut len_buf) => {
                match result {
                    Ok(_) => {}
                    Err(e) => return Err(Error::WebInspector(format!("Failed to read length header: {}", e))),
                }
            }
            _ = cancellation.cancelled() => {
                return Ok(());
            }
        }

        let body_length = WebInspectorCodec::parse_length(&len_buf)?;

        if body_length == 0 {
            // Empty packet, skip
            continue;
        }

        // Read message body
        let mut body = vec![0u8; body_length];
        tokio::select! {
            result = reader.read_exact(&mut body) => {
                match result {
                    Ok(_) => {}
                    Err(e) => return Err(Error::WebInspector(format!("Failed to read message body: {}", e))),
                }
            }
            _ = cancellation.cancelled() => {
                return Ok(());
            }
        }

        // Decode
        match codec.decode_body(&body) {
            Ok(Some(event)) => {
                if event_tx.send(event).await.is_err() {
                    return Ok(()); // Receiver closed
                }
            }
            Ok(None) => {} // Partial message
            Err(e) => {
                warn!("WebInspector packet decode failed: {}", e);
            }
        }
    }
}

/// WebInspector write loop
async fn wi_write_loop<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut write_rx: mpsc::Receiver<Vec<u8>>,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<()> {
    loop {
        tokio::select! {
            msg = write_rx.recv() => {
                match msg {
                    Some(data) => {
                    writer
                            .write_all(&data)
                            .await
                            .map_err(|e| Error::WebInspector(format!("Write failed: {}", e)))?;
                        writer
                            .flush()
                            .await
                            .map_err(|e| Error::WebInspector(format!("Flush failed: {}", e)))?;
                    }
                    None => {
                        // Sender closed
                        return Ok(());
                    }
                }
            }
            _ = cancellation.cancelled() => {
                return Ok(());
            }
        }
    }
}
