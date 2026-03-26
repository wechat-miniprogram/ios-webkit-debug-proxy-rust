// Proxy core module
// Connects all components together, translates WebInspector to WebKit Remote Debugging Protocol
// Uses tokio async task model to manage connection lifecycles

use crate::device_listener::{self, DeviceEvent};
use crate::error::{Error, Result};
use crate::http_server::{
    self, DeviceListState, DevicePortState, PageEntry, SharedDeviceListState,
    SharedDevicePortState, WiToWsRegistration, WsToWiMessage,
};
use crate::rpc::{self, RpcApp, RpcMessage, RpcSender};
use crate::webinspector::{self, WebInspectorCodec, WebInspectorHandle, WiEvent};
use crate::{DebuggablePageInfo, DeviceInfo, DeviceListener, PageType};

use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;

/// Proxy bridge - connects all components (async version)
pub struct ProxyBridge {
    /// Available websocket port range (None means auto-assigned by the system)
    device_ports: Option<Range<u16>>,
    /// Port configuration (None means disable HTTP service except WebSocket)
    http_service_config: Option<HttpServiceConfig>,
    /// Simulator WebInspector address
    sim_wi_socket_addr: String,
    /// Event listener
    device_listener: Option<Arc<dyn DeviceListener + Send + Sync>>,
}

pub struct HttpServiceConfig {
    pub device_list_port: u16,
    pub frontend: Option<String>,
}

/// Device runtime state (internal use)
struct DeviceRuntime {
    /// Port number
    port: u16,
    /// Device ID
    device_id: Option<String>,
    /// Device name
    device_name: Option<String>,
    /// Device OS version
    device_os_version: u32,
    /// Whether connected to WebInspector
    connected: bool,
    /// WebInspector handle (for sending data)
    wi_handle: Option<WebInspectorHandle>,
    /// WebInspector codec
    wi_codec: Option<WebInspectorCodec>,
    /// RPC sender
    rpc_sender: Option<RpcSender>,
    /// connection_id
    #[allow(dead_code)]
    connection_id: Option<String>,
    /// App ID set
    app_ids: HashMap<String, bool>,
    /// Current app
    current_app: Option<RpcApp>,
    /// Page list
    pages: HashMap<u32, PageInfo>,
    /// Maximum page number
    max_page_num: u32,
    /// Shared state (shared with HTTP server)
    shared_state: SharedDevicePortState,
    /// axum server cancel token
    server_cancel: Option<CancellationToken>,
    /// WebInspector cancel token
    wi_cancel: Option<CancellationToken>,
    /// WebSocket client mapping: ws_id -> (page_num, sender_id)
    ws_clients: HashMap<String, WsClientInfo>,
    /// WebSocket client message forward channel: ws_id -> mpsc::Sender<Vec<u8>>
    ws_client_txs: HashMap<String, mpsc::Sender<Vec<u8>>>,
}

#[allow(dead_code)]
struct WsClientInfo {
    page_num: u32,
    #[allow(dead_code)]
    sender_id: String,
}

/// Page info
#[derive(Debug, Clone)]
struct PageInfo {
    page_num: u32,
    page_type: PageType,
    app_id: String,
    page_id: u32,
    #[allow(dead_code)]
    connection_id: Option<String>,
    title: Option<String>,
    url: Option<String>,
    sender_id: Option<String>,
    ws_client_id: Option<String>,
}

impl ProxyBridge {
    pub fn new(
        device_ports: Option<Range<u16>>,
        http_service_config: Option<HttpServiceConfig>,
        sim_wi_socket_addr: Option<&str>,
        device_listener: Option<Arc<dyn DeviceListener + Send + Sync>>,
    ) -> Self {
        let sim_wi_socket_addr = sim_wi_socket_addr.unwrap_or(crate::DEFAULT_SIM_WI_SOCKET_ADDR).to_string();
        ProxyBridge {
            device_ports,
            http_service_config,
            sim_wi_socket_addr,
            device_listener,
        }
    }

    /// Start proxy (async entry point)
    pub async fn run(&mut self, cancellation: CancellationToken) -> Result<()> {
        // Start device list HTTP server
        let device_list_state: SharedDeviceListState = Arc::new(RwLock::new(DeviceListState {
            devices: Vec::new(),
        }));

        if let Some(ref config) = self.http_service_config {
            let list_port = config.device_list_port;
            if list_port > 0 {
                let listener = TcpListener::bind(format!("0.0.0.0:{}", list_port))
                    .await
                    .map_err(|e| Error::PortBind(format!("Failed to bind device list port {}: {}", list_port, e)))?;
                info!("Device list port: :{}", list_port);

                let router = http_server::create_device_list_router(device_list_state.clone());
                let cancel = cancellation.clone();
                tokio::spawn(async move {
                    axum::serve(listener, router)
                        .with_graceful_shutdown(cancel.cancelled_owned())
                        .await
                        .ok();
                });
            }
        }

        // Start device listener
        let mut dl_rx = device_listener::spawn_device_listener(cancellation.clone());

        // Device runtime table: device_id -> DeviceRuntime
        let mut devices: HashMap<String, DeviceRuntime> = HashMap::new();

        // Channel: receive WebInspector events from each device
        let (wi_event_tx, mut wi_event_rx) = mpsc::channel::<(String, WiEvent)>(256);
        // Channel: receive WebSocket client messages from each device
        let (ws_msg_tx, mut ws_msg_rx) = mpsc::channel::<(String, WsToWiMessage)>(256);
        // Channel: receive WebSocket client registrations
        let (ws_reg_tx, mut ws_reg_rx) = mpsc::channel::<(String, WiToWsRegistration)>(256);

        // Simulator device auto-connect
        self.connect_device(
            "SIMULATOR".to_string(),
            &mut devices,
            &device_list_state,
            &wi_event_tx,
            &ws_msg_tx,
            &ws_reg_tx,
            &cancellation,
        )
        .await;

        // Main event loop
        loop {
            tokio::select! {
                // Device connect/disconnect events
                event = dl_rx.recv() => {
                    match event {
                        Some(DeviceEvent::Attached { device_id, device_num: _ }) => {
                            info!("Device connected: {}", device_id);
                            self.connect_device(
                                device_id,
                                &mut devices,
                                &device_list_state,
                                &wi_event_tx,
                                &ws_msg_tx,
                                &ws_reg_tx,
                                &cancellation,
                            ).await;
                        }
                        Some(DeviceEvent::Detached { device_id, device_num: _ }) => {
                            info!("Device disconnected: {}", device_id);
                            self.disconnect_device(&device_id, &mut devices, &device_list_state).await;
                        }
                        None => {
                            warn!("Device listener channel closed");
                            break;
                        }
                    }
                }

                // WebInspector events
                event = wi_event_rx.recv() => {
                    if let Some((device_id, wi_event)) = event {
                        self.handle_wi_event(&device_id, wi_event, &mut devices, &device_list_state, &wi_event_tx, &ws_msg_tx, &ws_reg_tx, &cancellation).await;
                    }
                }

                // WebSocket client messages -> forward to WebInspector
                msg = ws_msg_rx.recv() => {
                    if let Some((device_id, ws_msg)) = msg {
                        self.handle_ws_message(&device_id, ws_msg, &mut devices).await;
                    }
                }

                // WebSocket client registrations
                reg = ws_reg_rx.recv() => {
                    if let Some((device_id, registration)) = reg {
                        self.handle_ws_registration(&device_id, registration, &mut devices).await;
                    }
                }

                // Cancellation signal
                _ = cancellation.cancelled() => {
                    break;
                }
            }
        }

        // Clean up all devices
        let device_ids: Vec<String> = devices.keys().cloned().collect();
        for device_id in device_ids {
            self.disconnect_device(&device_id, &mut devices, &device_list_state)
                .await;
        }

        info!("Proxy has been shut down");
        Ok(())
    }

    /// Connect device
    #[allow(clippy::too_many_arguments)]
    async fn connect_device(
        &self,
        device_id: String,
        devices: &mut HashMap<String, DeviceRuntime>,
        device_list_state: &SharedDeviceListState,
        wi_event_tx: &mpsc::Sender<(String, WiEvent)>,
        ws_msg_tx: &mpsc::Sender<(String, WsToWiMessage)>,
        ws_reg_tx: &mpsc::Sender<(String, WiToWsRegistration)>,
        cancellation: &CancellationToken,
    ) {
        if devices.contains_key(&device_id) {
            debug!("Device {} already connected, skipping", device_id);
            return;
        }

        // Allocate port from device_ports, if None then auto-assigned by the system
        let port: u16;
        let listener: TcpListener;

        if let Some(ref device_ports) = self.device_ports {
            // Find available port
            let used_ports: Vec<u16> = devices.values().map(|d| d.port).collect();
            let mut found_port: Option<u16> = None;
            for p in device_ports.clone() {
                if !used_ports.contains(&p) {
                    found_port = Some(p);
                    break;
                }
            }

            port = match found_port {
                Some(p) => p,
                None => {
                    error!("Cannot allocate port for device {}", device_id);
                    return;
                }
            };

            // Bind TCP port
            listener = match TcpListener::bind(format!("0.0.0.0:{}", port)).await {
                Ok(l) => l,
                Err(e) => {
                    error!("Failed to bind port {}: {}", port, e);
                    return;
                }
            };
        } else {
            // System auto-assign port
            listener = match TcpListener::bind("0.0.0.0:0").await {
                Ok(l) => l,
                Err(e) => {
                    error!("Failed to auto-assign port: {}", e);
                    return;
                }
            };
            port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
            if port == 0 {
                error!("Cannot get auto-assigned port number");
                return;
            }
        }

        // Connect to WebInspector
        let is_sim = device_id == "SIMULATOR";
        let wi_cancel = cancellation.child_token();

        let (wi_handle, wi_codec, device_name, device_os_version) = if is_sim {
            // Simulator - connect to TCP or Unix domain socket
            match connect_sim_webinspector(&self.sim_wi_socket_addr).await {
                Ok(stream) => {
                    let (handle, mut event_rx) =
                        webinspector::spawn_webinspector(stream, true, false, wi_cancel.clone());

                    // Forward WI events to unified channel
                    let device_id_clone = device_id.clone();
                    let tx = wi_event_tx.clone();
                    tokio::spawn(async move {
                        while let Some(event) = event_rx.recv().await {
                            if tx.send((device_id_clone.clone(), event)).await.is_err() {
                                break;
                            }
                        }
                    });

                    let codec = WebInspectorCodec::new(true, false);
                    (Some(handle), Some(codec), Some("SIMULATOR".to_string()), 0)
                }
                Err(e) => {
                    debug!("Failed to connect to simulator WebInspector: {}", e);
                    return;
                }
            }
        } else {
            // Real device
            match crate::idevice_ext::connect_to_device(Some(&device_id)).await {
                Ok(conn) => {
                    let os_version = conn.device_os_version;
                    let name = conn.device_name.clone();
                    let partials_supported = os_version < 0xb0000;

                    let (handle, mut event_rx) = webinspector::spawn_webinspector(
                        conn.stream,
                        false,
                        partials_supported,
                        wi_cancel.clone(),
                    );

                    // Forward WI events
                    let device_id_clone = device_id.clone();
                    let tx = wi_event_tx.clone();
                    tokio::spawn(async move {
                        while let Some(event) = event_rx.recv().await {
                            if tx.send((device_id_clone.clone(), event)).await.is_err() {
                                break;
                            }
                        }
                    });

                    let codec = WebInspectorCodec::new(false, partials_supported);
                    (Some(handle), Some(codec), Some(name), os_version)
                }
                Err(e) => {
                    error!("Failed to connect to device {}: {}", device_id, e);
                    return;
                }
            }
        };

        // Create shared state
        let device_id_for_ws = device_id.clone();
        let ws_to_wi_tx = ws_msg_tx.clone();
        let wrapped_ws_to_wi_tx = {
            let device_id_clone = device_id_for_ws.clone();
            let (tx, mut rx) = mpsc::channel::<WsToWiMessage>(64);
            let outer_tx = ws_to_wi_tx;
            tokio::spawn(async move {
                while let Some(msg) = rx.recv().await {
                    if outer_tx.send((device_id_clone.clone(), msg)).await.is_err() {
                        break;
                    }
                }
            });
            tx
        };

        let wrapped_ws_reg_tx = {
            let device_id_clone = device_id.clone();
            let (tx, mut rx) = mpsc::channel::<WiToWsRegistration>(64);
            let outer_tx = ws_reg_tx.clone();
            tokio::spawn(async move {
                while let Some(reg) = rx.recv().await {
                    if outer_tx.send((device_id_clone.clone(), reg)).await.is_err() {
                        break;
                    }
                }
            });
            tx
        };

        let shared_state: SharedDevicePortState = Arc::new(RwLock::new(DevicePortState {
            port,
            device_id: Some(device_id.clone()),
            device_name: device_name.clone(),
            device_os_version,
            connected: false,
            pages: Vec::new(),
            frontend: self
                .http_service_config
                .as_ref()
                .and_then(|c| c.frontend.clone()),
            ws_to_wi_tx: Some(wrapped_ws_to_wi_tx),
            wi_to_ws_register_tx: Some(wrapped_ws_reg_tx),
        }));

        // Start axum HTTP/WebSocket server
        let router = if self.http_service_config.is_some() {
            http_server::create_device_router(shared_state.clone())
        } else {
            http_server::create_device_ws_only_router(shared_state.clone())
        };
        let server_cancel = cancellation.child_token();
        let sc = server_cancel.clone();
        tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(sc.cancelled_owned())
                .await
                .ok();
        });

        // Send reportIdentifier
        let connection_id = rpc::new_uuid();
        let rpc_sender = RpcSender::new(connection_id.clone());

        if let (Some(handle), Some(codec)) = (&wi_handle, &wi_codec) {
            let msg_data = rpc_sender.build_report_identifier();
            if let Err(e) = handle.send_plist(codec, &msg_data).await {
                error!("Failed to send reportIdentifier: {}", e);
            }
        }

        info!(
            "Device {} connected to :{} ({})",
            device_id,
            port,
            device_name.as_deref().unwrap_or("Unknown")
        );

        // Save runtime state
        devices.insert(
            device_id.clone(),
            DeviceRuntime {
                port,
                device_id: Some(device_id.clone()),
                device_name,
                device_os_version,
                connected: false,
                wi_handle,
                wi_codec,
                rpc_sender: Some(rpc_sender),
                connection_id: Some(connection_id),
                app_ids: HashMap::new(),
                current_app: None,
                pages: HashMap::new(),
                max_page_num: 0,
                shared_state: shared_state.clone(),
                server_cancel: Some(server_cancel),
                wi_cancel: Some(wi_cancel),
                ws_clients: HashMap::new(),
                ws_client_txs: HashMap::new(),
            },
        );

        // Update device list
        self.update_device_list(devices, device_list_state).await;
    }

    /// Disconnect device
    async fn disconnect_device(
        &self,
        device_id: &str,
        devices: &mut HashMap<String, DeviceRuntime>,
        device_list_state: &SharedDeviceListState,
    ) {
        if let Some(runtime) = devices.remove(device_id) {
            // Trigger device disconnect event
            if runtime.connected {
                if let Some(ref listener) = self.device_listener {
                    listener.device_disconnected(DeviceInfo {
                        device_id: runtime.device_id.clone().unwrap_or_default(),
                        device_name: runtime.device_name.clone().unwrap_or_default(),
                        device_os_version: runtime.device_os_version,
                        port: runtime.port,
                    });
                }
            }
            // Cancel WebInspector task
            if let Some(cancel) = runtime.wi_cancel {
                cancel.cancel();
            }
            // Cancel HTTP server
            if let Some(cancel) = runtime.server_cancel {
                cancel.cancel();
            }
            info!("Device {} disconnected (:{})，resources cleaned up", device_id, runtime.port);
        }

        // Update device list
        self.update_device_list(devices, device_list_state).await;
    }

    /// Update device list shared state
    async fn update_device_list(
        &self,
        devices: &HashMap<String, DeviceRuntime>,
        device_list_state: &SharedDeviceListState,
    ) {
        let mut state = device_list_state.write().await;
        state.devices = devices
            .values()
            .filter(|d| d.connected && d.device_id.is_some())
            .map(|d| DeviceInfo {
                device_id: d.device_id.clone().unwrap_or_default(),
                device_name: d.device_name.clone().unwrap_or_default(),
                device_os_version: d.device_os_version,
                port: d.port,
            })
            .collect();
    }

    /// Handle WebInspector event
    #[allow(clippy::too_many_arguments)]
    async fn handle_wi_event(
        &self,
        device_id: &str,
        event: WiEvent,
        devices: &mut HashMap<String, DeviceRuntime>,
        device_list_state: &SharedDeviceListState,
        wi_event_tx: &mpsc::Sender<(String, WiEvent)>,
        ws_msg_tx: &mpsc::Sender<(String, WsToWiMessage)>,
        ws_reg_tx: &mpsc::Sender<(String, WiToWsRegistration)>,
        cancellation: &CancellationToken,
    ) {
        match event {
            WiEvent::RecvPlist(rpc_dict) => match rpc::parse_rpc_message(&rpc_dict) {
                Ok(msg) => {
                    self.handle_rpc_message(
                        device_id,
                        msg,
                        devices,
                        device_list_state,
                        wi_event_tx,
                        ws_msg_tx,
                        ws_reg_tx,
                        cancellation,
                    )
                    .await;
                }
                Err(e) => warn!("RPC message parsing failed (device {}): {}", device_id, e),
            },
        }
    }

    /// Handle RPC message
    #[allow(clippy::too_many_arguments)]
    async fn handle_rpc_message(
        &self,
        device_id: &str,
        msg: RpcMessage,
        devices: &mut HashMap<String, DeviceRuntime>,
        device_list_state: &SharedDeviceListState,
        _wi_event_tx: &mpsc::Sender<(String, WiEvent)>,
        _ws_msg_tx: &mpsc::Sender<(String, WsToWiMessage)>,
        _ws_reg_tx: &mpsc::Sender<(String, WiToWsRegistration)>,
        _cancellation: &CancellationToken,
    ) {
        match msg {
            RpcMessage::ReportSetup => {
                if let Some(runtime) = devices.get_mut(device_id) {
                    runtime.connected = true;
                    info!(
                        "Connected :{} to {} ({})",
                        runtime.port,
                        runtime.device_name.as_deref().unwrap_or("Unknown"),
                        device_id
                    );
                    // Update shared state
                    let mut state = runtime.shared_state.write().await;
                    state.connected = true;
                }
                // Update device list
                self.update_device_list(devices, device_list_state).await;
                // Trigger device connected event
                self.notify_device_connected(device_id, devices);
            }

            RpcMessage::ReportConnectedApplicationList(apps) => {
                let mut newly_connected = false;
                if let Some(runtime) = devices.get_mut(device_id) {
                    if !runtime.connected {
                        runtime.connected = true;
                        newly_connected = true;
                        info!(
                            "Connected :{} to {} ({})",
                            runtime.port,
                            runtime.device_name.as_deref().unwrap_or("Unknown"),
                            device_id
                        );
                        let mut state = runtime.shared_state.write().await;
                        state.connected = true;
                    }
                }
                if newly_connected {
                    self.update_device_list(devices, device_list_state).await;
                    // Trigger device connected event
                    self.notify_device_connected(device_id, devices);
                }
                for app in &apps {
                    self.add_app_id(device_id, &app.app_id, devices).await;
                    if let Some(runtime) = devices.get_mut(device_id) {
                        runtime.current_app = Some(app.clone());
                    }
                }
            }

            RpcMessage::ApplicationConnected(app) => {
                if let Some(runtime) = devices.get_mut(device_id) {
                    runtime.current_app = Some(app.clone());
                }
                self.add_app_id(device_id, &app.app_id, devices).await;
            }

            RpcMessage::ApplicationDisconnected(app) => {
                self.remove_app_id(device_id, &app.app_id, devices).await;
            }

            RpcMessage::ApplicationSentListing { app_id, pages } => {
                self.handle_page_listing(device_id, &app_id, pages, devices)
                    .await;
            }

            RpcMessage::ApplicationSentData {
                app_id: _,
                dest_id,
                data,
            } => {
                self.handle_app_data(device_id, &dest_id, &data, devices)
                    .await;
            }

            RpcMessage::ApplicationUpdated { app_id: _, dest_id } => {
                self.add_app_id(device_id, &dest_id, devices).await;
            }

            RpcMessage::Unknown(selector) => {
                debug!("Ignoring unknown RPC (device {}): {}", device_id, selector);
            }
        }
    }

    /// Add app ID
    async fn add_app_id(
        &self,
        device_id: &str,
        app_id: &str,
        devices: &mut HashMap<String, DeviceRuntime>,
    ) {
        let already_known = if let Some(runtime) = devices.get_mut(device_id) {
            if runtime.app_ids.contains_key(app_id) {
                true
            } else {
                runtime.app_ids.insert(app_id.to_string(), true);
                false
            }
        } else {
            return;
        };

        if !already_known {
            self.send_forward_get_listing(device_id, app_id, devices)
                .await;
        }
    }

    /// Remove app ID
    async fn remove_app_id(
        &self,
        device_id: &str,
        app_id: &str,
        devices: &mut HashMap<String, DeviceRuntime>,
    ) {
        if let Some(runtime) = devices.get_mut(device_id) {
            runtime.app_ids.remove(app_id);
            // Remove all pages for this app
            let page_nums: Vec<u32> = runtime
                .pages
                .iter()
                .filter(|(_, p)| p.app_id == app_id)
                .map(|(&num, _)| num)
                .collect();
            for num in page_nums {
                runtime.pages.remove(&num);
            }
            // Update shared state
            Self::sync_pages(runtime).await;

            // Trigger page list change event
            self.notify_page_list_changed(runtime);
        }
    }

    /// Send forwardGetListing
    async fn send_forward_get_listing(
        &self,
        device_id: &str,
        app_id: &str,
        devices: &mut HashMap<String, DeviceRuntime>,
    ) {
        if let Some(runtime) = devices.get(device_id) {
            if let (Some(rpc_sender), Some(handle), Some(codec)) =
                (&runtime.rpc_sender, &runtime.wi_handle, &runtime.wi_codec)
            {
                let msg_data = rpc_sender.build_forward_get_listing(app_id);
                if let Err(e) = handle.send_plist(codec, &msg_data).await {
                    warn!("Failed to send forwardGetListing: {}", e);
                }
            }
        }
    }

    /// Handle page listing
    async fn handle_page_listing(
        &self,
        device_id: &str,
        app_id: &str,
        pages: Vec<rpc::RpcPage>,
        devices: &mut HashMap<String, DeviceRuntime>,
    ) {
        if let Some(runtime) = devices.get_mut(device_id) {
            // Add new pages
            for page in &pages {
                let existing = runtime
                    .pages
                    .values()
                    .find(|p| p.app_id == app_id && p.page_id == page.page_id);

                if let Some(existing) = existing {
                    let page_num = existing.page_num;
                    if let Some(p) = runtime.pages.get_mut(&page_num) {
                        p.title = page.title.clone();
                        p.url = page.url.clone();
                        p.connection_id = page.connection_id.clone();
                    }
                } else {
                    runtime.max_page_num += 1;
                    let page_num = runtime.max_page_num;
                    runtime.pages.insert(
                        page_num,
                        PageInfo {
                            page_num,
                            page_type: page.page_type,
                            app_id: app_id.to_string(),
                            page_id: page.page_id,
                            connection_id: page.connection_id.clone(),
                            title: page.title.clone(),
                            url: page.url.clone(),
                            sender_id: None,
                            ws_client_id: None,
                        },
                    );
                }
            }

            // Remove old pages
            let to_remove: Vec<u32> = runtime
                .pages
                .iter()
                .filter(|(_, p)| {
                    p.app_id == app_id && !pages.iter().any(|rp| rp.page_id == p.page_id)
                })
                .map(|(&num, _)| num)
                .collect();
            for num in to_remove {
                runtime.pages.remove(&num);
            }

            // Sync to shared state
            Self::sync_pages(runtime).await;

            // Trigger page list change event
            self.notify_page_list_changed(runtime);
        }
    }

    /// Handle app sent data - forward to WebSocket client
    async fn handle_app_data(
        &self,
        device_id: &str,
        dest_id: &str,
        data: &[u8],
        devices: &mut HashMap<String, DeviceRuntime>,
    ) {
        if let Some(runtime) = devices.get(device_id) {
            // Find the corresponding ws client
            if let Some(tx) = runtime.ws_client_txs.get(dest_id) {
                if tx.send(data.to_vec()).await.is_err() {
                    debug!("WebSocket client {} channel closed", dest_id);
                }
            }
        }
    }

    /// Handle WebSocket client message
    async fn handle_ws_message(
        &self,
        device_id: &str,
        msg: WsToWiMessage,
        devices: &mut HashMap<String, DeviceRuntime>,
    ) {
        if msg.is_close {
            // Close debug channel
            self.stop_devtools(device_id, msg.page_num, devices).await;
            // Remove client
            if let Some(runtime) = devices.get_mut(device_id) {
                runtime.ws_clients.remove(&msg.ws_id);
                runtime.ws_client_txs.remove(&msg.ws_id);
            }
            return;
        }

        if msg.data.is_empty() {
            // Initialize connection (forwardSocketSetup)
            self.send_forward_socket_setup(device_id, &msg.ws_id, msg.page_num, devices)
                .await;
            return;
        }

        // Forward data to WebInspector
        if let Some(runtime) = devices.get(device_id) {
            let page_info = runtime.pages.values().find(|p| p.page_num == msg.page_num);

            if let Some(page) = page_info {
                if let (Some(rpc_sender), Some(handle), Some(codec)) =
                    (&runtime.rpc_sender, &runtime.wi_handle, &runtime.wi_codec)
                {
                    let sender_id = page.sender_id.as_deref().unwrap_or(&msg.ws_id);
                    let msg_data = rpc_sender.build_forward_socket_data(
                        &page.app_id,
                        page.page_id,
                        sender_id,
                        &msg.data,
                    );
                    if let Err(e) = handle.send_plist(codec, &msg_data).await {
                        warn!("Failed to forward data to WebInspector: {}", e);
                    }
                }
            }
        }
    }

    /// Handle WebSocket client registration
    async fn handle_ws_registration(
        &self,
        device_id: &str,
        registration: WiToWsRegistration,
        devices: &mut HashMap<String, DeviceRuntime>,
    ) {
        if let Some(runtime) = devices.get_mut(device_id) {
            runtime.ws_clients.insert(
                registration.ws_id.clone(),
                WsClientInfo {
                    page_num: registration.page_num,
                    sender_id: registration.ws_id.clone(),
                },
            );
            runtime
                .ws_client_txs
                .insert(registration.ws_id.clone(), registration.tx);
        }
    }

    /// Send forwardSocketSetup
    async fn send_forward_socket_setup(
        &self,
        device_id: &str,
        ws_id: &str,
        page_num: u32,
        devices: &mut HashMap<String, DeviceRuntime>,
    ) {
        // First close existing debug channel
        let old_ws_id = if let Some(runtime) = devices.get(device_id) {
            runtime
                .pages
                .values()
                .find(|p| p.page_num == page_num)
                .and_then(|p| p.ws_client_id.clone())
        } else {
            None
        };

        if let Some(ref old_id) = old_ws_id {
            if old_id != ws_id {
                self.stop_devtools(device_id, page_num, devices).await;
            }
        }

        if let Some(runtime) = devices.get_mut(device_id) {
            // Update page state
            let page = runtime.pages.values_mut().find(|p| p.page_num == page_num);

            if let Some(page) = page {
                page.sender_id = Some(ws_id.to_string());
                page.ws_client_id = Some(ws_id.to_string());

                let app_id = page.app_id.clone();
                let page_id = page.page_id;

                if let (Some(rpc_sender), Some(handle), Some(codec)) =
                    (&runtime.rpc_sender, &runtime.wi_handle, &runtime.wi_codec)
                {
                    let msg_data = rpc_sender.build_forward_socket_setup(&app_id, page_id, ws_id);
                    if let Err(e) = handle.send_plist(codec, &msg_data).await {
                        warn!("Failed to send forwardSocketSetup: {}", e);
                    }
                }
            }
        }
    }

    /// Stop DevTools debugging
    async fn stop_devtools(
        &self,
        device_id: &str,
        page_num: u32,
        devices: &mut HashMap<String, DeviceRuntime>,
    ) {
        if let Some(runtime) = devices.get_mut(device_id) {
            let page = runtime.pages.values().find(|p| p.page_num == page_num);

            if let Some(page) = page {
                let sender_id = match &page.sender_id {
                    Some(id) => id.clone(),
                    None => return,
                };
                let app_id = page.app_id.clone();
                let page_id = page.page_id;

                if let (Some(rpc_sender), Some(handle), Some(codec)) =
                    (&runtime.rpc_sender, &runtime.wi_handle, &runtime.wi_codec)
                {
                    let msg_data = rpc_sender.build_forward_did_close(&app_id, page_id, &sender_id);
                    if let Err(e) = handle.send_plist(codec, &msg_data).await {
                        warn!("Failed to send forwardDidClose: {}", e);
                    }
                }
            }

            // Clean up page state
            if let Some(page) = runtime.pages.values_mut().find(|p| p.page_num == page_num) {
                page.sender_id = None;
                page.ws_client_id = None;
            }
        }
    }

    /// Notify device connected event
    fn notify_device_connected(&self, device_id: &str, devices: &HashMap<String, DeviceRuntime>) {
        if let Some(ref listener) = self.device_listener {
            if let Some(runtime) = devices.get(device_id) {
                listener.device_connected(DeviceInfo {
                    device_id: runtime.device_id.clone().unwrap_or_default(),
                    device_name: runtime.device_name.clone().unwrap_or_default(),
                    device_os_version: runtime.device_os_version,
                    port: runtime.port,
                });
            }
        }
    }

    /// Notify page list change event
    fn notify_page_list_changed(&self, runtime: &DeviceRuntime) {
        if let Some(ref listener) = self.device_listener {
            let pages: Vec<DebuggablePageInfo> = runtime
                .pages
                .values()
                .map(|p| DebuggablePageInfo {
                    device_id: runtime.device_id.clone().unwrap_or_default(),
                    page_num: p.page_num,
                    page_type: p.page_type,
                    name: p.title.clone().unwrap_or_default(),
                    web_socket_debugger_url: format!(
                        "ws://localhost:{}/devtools/page/{}",
                        runtime.port, p.page_num
                    ),
                    device_os_version: runtime.device_os_version,
                })
                .collect();
            listener.device_page_list_changed(
                DeviceInfo {
                    device_id: runtime.device_id.clone().unwrap_or_default(),
                    device_name: runtime.device_name.clone().unwrap_or_default(),
                    device_os_version: runtime.device_os_version,
                    port: runtime.port,
                },
                pages,
            );
        }
    }

    /// Sync page list to shared state
    async fn sync_pages(runtime: &DeviceRuntime) {
        let mut state = runtime.shared_state.write().await;
        let mut pages: Vec<PageEntry> = runtime
            .pages
            .values()
            .map(|p| PageEntry {
                page_num: p.page_num,
                app_id: p.app_id.clone(),
                page_id: p.page_id,
                title: p.title.clone(),
                url: p.url.clone(),
                sender_id: p.sender_id.clone(),
            })
            .collect();
        pages.sort_by_key(|p| p.page_num);
        state.pages = pages;
    }
}

// =====================
// Simulator WebInspector connection helpers
// =====================

/// Simulator stream type: TCP or Unix domain socket
enum SimStream {
    Tcp(tokio::net::TcpStream),
    #[cfg(not(target_os = "windows"))]
    Unix(tokio::net::UnixStream),
}

impl AsyncRead for SimStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            SimStream::Tcp(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(not(target_os = "windows"))]
            SimStream::Unix(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for SimStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            SimStream::Tcp(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(not(target_os = "windows"))]
            SimStream::Unix(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            SimStream::Tcp(s) => Pin::new(s).poll_flush(cx),
            #[cfg(not(target_os = "windows"))]
            SimStream::Unix(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            SimStream::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(not(target_os = "windows"))]
            SimStream::Unix(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// Connect to simulator WebInspector
/// Supports two address formats:
/// - TCP: `hostname:port` (e.g. `localhost:27753`)
/// - Unix domain socket: `unix:/path/to/socket` (macOS/Linux only)
async fn connect_sim_webinspector(addr: &str) -> std::result::Result<SimStream, std::io::Error> {
    #[cfg(not(target_os = "windows"))]
    if let Some(path) = addr.strip_prefix("unix:") {
        let stream = tokio::net::UnixStream::connect(path).await?;
        return Ok(SimStream::Unix(stream));
    }

    let stream = tokio::net::TcpStream::connect(addr).await?;
    Ok(SimStream::Tcp(stream))
}
