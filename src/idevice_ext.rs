// idevice extension module
// Corresponds to the original C code idevice_ext.c
// Provides device connection and SSL functionality via libimobiledevice FFI
// Uses tokio-rustls for async TLS

use log::debug;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::pin::Pin;
use std::ptr;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

use crate::error::{Error, Result};

// Platform-specific raw fd/socket conversion
#[cfg(not(target_os = "windows"))]
use std::os::unix::io::FromRawFd;
#[cfg(target_os = "windows")]
use std::os::windows::io::FromRawSocket;

/// Cross-platform C free() wrapper
/// macOS/Linux uses libc::free, Windows uses C runtime free
#[cfg(not(target_os = "windows"))]
unsafe fn c_free(ptr: *mut c_void) {
    unsafe { libc::free(ptr) };
}

#[cfg(target_os = "windows")]
unsafe fn c_free(ptr: *mut c_void) {
    unsafe extern "C" {
        fn free(ptr: *mut c_void);
    }
    unsafe { free(ptr) };
}

// libimobiledevice FFI bindings
#[allow(non_camel_case_types)]
type idevice_t = *mut c_void;
#[allow(non_camel_case_types)]
type lockdownd_client_t = *mut c_void;
#[allow(non_camel_case_types)]
type idevice_connection_t = *mut c_void;
#[allow(non_camel_case_types)]
type lockdownd_service_descriptor_t = *mut LockdownServiceDescriptor;
#[allow(non_camel_case_types)]
type plist_t = *mut c_void;

const IDEVICE_LOOKUP_USBMUX: c_int = 1 << 1;
const IDEVICE_LOOKUP_NETWORK: c_int = 1 << 2;

#[repr(C)]
#[allow(non_camel_case_types)]
struct LockdownServiceDescriptor {
    port: u16,
    ssl_enabled: u8,
    identifier: *mut c_char,
}

unsafe extern "C" {
    fn idevice_new_with_options(
        device: *mut idevice_t,
        udid: *const c_char,
        options: c_int,
    ) -> c_int;
    fn idevice_free(device: idevice_t) -> c_int;
    fn idevice_connect(
        device: idevice_t,
        port: u16,
        connection: *mut idevice_connection_t,
    ) -> c_int;
    fn idevice_connection_get_fd(connection: idevice_connection_t, fd: *mut c_int) -> c_int;

    fn lockdownd_client_new_with_handshake(
        device: idevice_t,
        client: *mut lockdownd_client_t,
        label: *const c_char,
    ) -> c_int;
    fn lockdownd_client_free(client: lockdownd_client_t) -> c_int;
    fn lockdownd_get_value(
        client: lockdownd_client_t,
        domain: *const c_char,
        key: *const c_char,
        value: *mut plist_t,
    ) -> c_int;
    fn lockdownd_start_service(
        client: lockdownd_client_t,
        identifier: *const c_char,
        service: *mut lockdownd_service_descriptor_t,
    ) -> c_int;

    fn plist_get_string_val(node: plist_t, val: *mut *mut c_char);
    fn plist_free(plist: plist_t);
    fn usbmuxd_read_pair_record(
        record_id: *const c_char,
        record_data: *mut *mut c_char,
        record_size: *mut u32,
    ) -> c_int;
}

/// Device connection info (async version)
#[allow(dead_code)]
pub(crate) struct DeviceConnection {
    pub(crate) stream: DeviceStream,
    pub(crate) device_id: String,
    pub(crate) device_name: String,
    pub(crate) device_os_version: u32,
}

/// Device stream type: either plain TCP or TLS-encrypted TCP
pub(crate) enum DeviceStream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for DeviceStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            DeviceStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            DeviceStream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for DeviceStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            DeviceStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            DeviceStream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            DeviceStream::Plain(s) => Pin::new(s).poll_flush(cx),
            DeviceStream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            DeviceStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            DeviceStream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

/// Connect to the iOS device's WebInspector service (async version)
///
/// Note: libimobiledevice FFI calls are still synchronous blocking (executed in spawn_blocking),
/// but after obtaining the fd, it is converted to a tokio TcpStream for async operations.
///
/// # Safety
/// This function calls libimobiledevice C FFI, requires the relevant libraries to be installed
pub(crate) async fn connect_to_device(device_id: Option<&str>) -> Result<DeviceConnection> {
    // Convert device_id to owned string for cross-thread transfer
    let device_id_owned = device_id.map(String::from);

    // libimobiledevice FFI is blocking, execute in spawn_blocking
    let ffi_result =
        tokio::task::spawn_blocking(move || connect_to_device_sync(device_id_owned.as_deref()))
            .await
            .map_err(|e| Error::TaskJoin(format!("FFI task failed: {}", e)))??;

    // Convert raw fd/socket to tokio TcpStream
    #[cfg(not(target_os = "windows"))]
    let std_stream = unsafe { std::net::TcpStream::from_raw_fd(ffi_result.fd) };
    #[cfg(target_os = "windows")]
    let std_stream = unsafe { std::net::TcpStream::from_raw_socket(ffi_result.fd as u64) };
    std_stream
        .set_nonblocking(true)
        .map_err(|e| Error::DeviceConnection(format!("Failed to set non-blocking: {}", e)))?;
    let tcp_stream = TcpStream::from_std(std_stream)
        .map_err(|e| Error::DeviceConnection(format!("Failed to convert to tokio TcpStream: {}", e)))?;

    // If SSL is needed, use tokio-rustls for async TLS handshake
    let stream = if ffi_result.ssl_enabled {
        let tls_stream = enable_ssl_async(&ffi_result.device_id, tcp_stream).await?;
        DeviceStream::Tls(Box::new(tls_stream))
    } else {
        DeviceStream::Plain(tcp_stream)
    };

    Ok(DeviceConnection {
        stream,
        device_id: ffi_result.device_id,
        device_name: ffi_result.device_name,
        device_os_version: ffi_result.device_os_version,
    })
}

/// FFI connection result (internal use)
struct FfiConnectionResult {
    fd: i32,
    device_id: String,
    device_name: String,
    device_os_version: u32,
    ssl_enabled: bool,
}

/// Synchronous FFI connection logic (called in spawn_blocking)
fn connect_to_device_sync(device_id: Option<&str>) -> Result<FfiConnectionResult> {
    unsafe {
        let mut phone: idevice_t = ptr::null_mut();
        let mut client: lockdownd_client_t = ptr::null_mut();
        let mut connection: idevice_connection_t = ptr::null_mut();
        let mut service: lockdownd_service_descriptor_t = ptr::null_mut();

        // Get device
        let udid_cstr = device_id.map(|id| CString::new(id).unwrap());
        let udid_ptr = udid_cstr
            .as_ref()
            .map(|s| s.as_ptr())
            .unwrap_or(ptr::null());

        if idevice_new_with_options(
            &mut phone,
            udid_ptr,
            IDEVICE_LOOKUP_USBMUX | IDEVICE_LOOKUP_NETWORK,
        ) != 0
        {
            return Err(Error::DeviceConnection("Device not found, please make sure it is connected".to_string()));
        }

        // Connect to lockdownd
        let label = CString::new("ios_webkit_debug_proxy").unwrap();
        let ld_ret = lockdownd_client_new_with_handshake(phone, &mut client, label.as_ptr());
        if ld_ret != 0 {
            idevice_free(phone);
            return Err(Error::DeviceConnection(lockdownd_error_string(ld_ret)));
        }

        // Get device info
        let mut result_device_id = String::new();
        let mut result_device_name = String::new();
        let mut result_os_version: u32 = 0;

        // Get UniqueDeviceID
        let mut node: plist_t = ptr::null_mut();
        let key = CString::new("UniqueDeviceID").unwrap();
        if lockdownd_get_value(client, ptr::null(), key.as_ptr(), &mut node) == 0 && !node.is_null()
        {
            let mut val: *mut c_char = ptr::null_mut();
            plist_get_string_val(node, &mut val);
            if !val.is_null() {
                result_device_id = CStr::from_ptr(val).to_string_lossy().to_string();
                c_free(val as *mut c_void);
            }
            plist_free(node);
        }

        // Get DeviceName
        node = ptr::null_mut();
        let key = CString::new("DeviceName").unwrap();
        if lockdownd_get_value(client, ptr::null(), key.as_ptr(), &mut node) == 0 && !node.is_null()
        {
            let mut val: *mut c_char = ptr::null_mut();
            plist_get_string_val(node, &mut val);
            if !val.is_null() {
                result_device_name = CStr::from_ptr(val).to_string_lossy().to_string();
                c_free(val as *mut c_void);
            }
            plist_free(node);
        }

        // Get ProductVersion
        node = ptr::null_mut();
        let key = CString::new("ProductVersion").unwrap();
        if lockdownd_get_value(client, ptr::null(), key.as_ptr(), &mut node) == 0 && !node.is_null()
        {
            let mut val: *mut c_char = ptr::null_mut();
            plist_get_string_val(node, &mut val);
            if !val.is_null() {
                let version_str = CStr::from_ptr(val).to_string_lossy().to_string();
                let parts: Vec<u32> = version_str
                    .split('.')
                    .filter_map(|s| s.parse().ok())
                    .collect();
                if parts.len() >= 2 {
                    result_os_version = ((parts[0] & 0xFF) << 16)
                        | ((parts[1] & 0xFF) << 8)
                        | (if parts.len() >= 3 { parts[2] & 0xFF } else { 0 });
                }
                c_free(val as *mut c_void);
            }
            plist_free(node);
        }

        // Start webinspector service
        let service_name = CString::new("com.apple.webinspector").unwrap();
        let ld_ret = lockdownd_start_service(client, service_name.as_ptr(), &mut service);
        if ld_ret != 0 || service.is_null() || (*service).port == 0 {
            lockdownd_client_free(client);
            idevice_free(phone);
            return Err(Error::DeviceConnection(format!(
                "Cannot start com.apple.webinspector service, error code: {}",
                ld_ret
            )));
        }

        let ssl_enabled = (*service).ssl_enabled == 1;

        // Connect to webinspector
        if idevice_connect(phone, (*service).port, &mut connection) != 0 {
            lockdownd_client_free(client);
            idevice_free(phone);
            return Err(Error::DeviceConnection("idevice_connect failed".to_string()));
        }

        lockdownd_client_free(client);

        // Extract fd
        let mut fd: c_int = -1;
        if idevice_connection_get_fd(connection, &mut fd) != 0 {
            idevice_free(phone);
            return Err(Error::DeviceConnection("Cannot get connection file descriptor".to_string()));
        }

        // Do not free connection, because we need to use the fd
        c_free(connection as *mut c_void);
        idevice_free(phone);

        debug!(
            "Connected to device: {} ({}), OS version: 0x{:x}, SSL: {}",
            result_device_name, result_device_id, result_os_version, ssl_enabled
        );

        Ok(FfiConnectionResult {
            fd,
            device_id: result_device_id,
            device_name: result_device_name,
            device_os_version: result_os_version,
            ssl_enabled,
        })
    }
}

/// Custom certificate verifier - skip all verification
/// Apple devices use self-signed certificates not in the standard CA chain
#[derive(Debug)]
struct NoVerifier;

impl rustls::client::danger::ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls_pki_types::CertificateDer<'_>,
        _intermediates: &[rustls_pki_types::CertificateDer<'_>],
        _server_name: &rustls_pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls_pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls_pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls_pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ED25519,
        ]
    }
}

/// Enable async SSL for device connection (using tokio-rustls)
///
/// Reads certificate and private key from usbmuxd pair record, establishes async TLS connection using tokio-rustls.
async fn enable_ssl_async(
    device_id: &str,
    tcp_stream: TcpStream,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    // Read pair record (synchronous FFI operation, executed in spawn_blocking)
    let device_id_owned = device_id.to_string();
    let tls_config = tokio::task::spawn_blocking(move || build_tls_config(&device_id_owned))
        .await
        .map_err(|e| Error::TaskJoin(format!("TLS config task failed: {}", e)))??;

    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config));

    // Use "localhost" as SNI (Apple devices don't care about SNI)
    let server_name = rustls_pki_types::ServerName::try_from("localhost")
        .map_err(|e| Error::Tls(format!("ServerName creation failed: {}", e)))?
        .to_owned();

    let tls_stream = connector
        .connect(server_name, tcp_stream)
        .await
        .map_err(|e| Error::Tls(format!("TLS handshake failed: {}", e)))?;

    debug!("Async TLS connection established (device: {})", device_id);

    Ok(tls_stream)
}

/// Build TLS configuration (synchronous, called in spawn_blocking)
fn build_tls_config(device_id: &str) -> Result<rustls::ClientConfig> {
    use rustls_pemfile::{certs, private_key};
    use std::io::Cursor;

    unsafe {
        // Read pair record
        let udid_cstr = CString::new(device_id).unwrap();
        let mut record_data: *mut c_char = ptr::null_mut();
        let mut record_size: u32 = 0;

        if usbmuxd_read_pair_record(udid_cstr.as_ptr(), &mut record_data, &mut record_size) < 0 {
            return Err(Error::Tls("Cannot read pair record".to_string()));
        }

        if record_data.is_null() || record_size == 0 {
            return Err(Error::Tls("Pair record is empty".to_string()));
        }

        // Parse pair record plist
        let record_bytes =
            std::slice::from_raw_parts(record_data as *const u8, record_size as usize);
        let record_plist: plist::Value =
            plist::from_bytes(record_bytes).map_err(|e| Error::Plist(format!("Pair record parsing failed: {}", e)))?;

        c_free(record_data as *mut c_void);

        let record_dict = record_plist.as_dictionary().ok_or(Error::Plist("Pair record is not a dictionary".to_string()))?;

        let root_cert_data = record_dict
            .get("RootCertificate")
            .and_then(|v| v.as_data())
            .ok_or(Error::Tls("Missing RootCertificate".to_string()))?;

        let root_privkey_data = record_dict
            .get("RootPrivateKey")
            .and_then(|v| v.as_data())
            .ok_or(Error::Tls("Missing RootPrivateKey".to_string()))?;

        // Parse PEM certificate
        let cert_chain: Vec<rustls_pki_types::CertificateDer<'static>> =
            certs(&mut Cursor::new(root_cert_data))
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| Error::Tls(format!("Certificate parsing failed: {}", e)))?;

        if cert_chain.is_empty() {
            return Err(Error::Tls("No valid certificate found".to_string()));
        }

        // Parse PEM private key
        let privkey = private_key(&mut Cursor::new(root_privkey_data))
            .map_err(|e| Error::Tls(format!("Private key parsing failed: {}", e)))?
            .ok_or(Error::Tls("No valid private key found".to_string()))?;

        // Create rustls configuration
        let config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifier))
            .with_client_auth_cert(cert_chain, privkey)
            .map_err(|e| Error::Tls(format!("rustls configuration failed: {}", e)))?;

        Ok(config)
    }
}

fn lockdownd_error_string(code: c_int) -> String {
    match code {
        -17 => "Please enter the passcode on the device and try again".to_string(),
        -18 => "Please accept the trust dialog on the device screen and try again".to_string(),
        -19 => "User denied the trust dialog, please re-plug the device and try again".to_string(),
        -6 | -15 => "Device is not paired with this host, please re-plug the device and try again".to_string(),
        _ => format!("Cannot connect to lockdownd, error code: {}", code),
    }
}
