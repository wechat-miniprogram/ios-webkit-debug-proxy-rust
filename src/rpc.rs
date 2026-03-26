// RPC module - WebInspector remote procedure call formatter
// Corresponds to the original C code rpc.c / rpc.h

use log::warn;
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::PageType;

/// RPC status
#[allow(dead_code)]
pub(crate) type RpcStatus = Result<()>;

/// App info
#[derive(Debug, Clone)]
pub(crate) struct RpcApp {
    pub(crate) app_id: String,
    #[allow(dead_code)]
    pub(crate) app_name: String,
    #[allow(dead_code)]
    pub(crate) is_proxy: bool,
}

/// Page info
#[derive(Debug, Clone)]
pub(crate) struct RpcPage {
    pub(crate) page_id: u32,
    pub(crate) page_type: PageType,
    pub(crate) connection_id: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) url: Option<String>,
}

/// Generate a new UUID
pub(crate) fn new_uuid() -> String {
    Uuid::new_v4().to_string().to_uppercase()
}

/// RPC message sender interface
pub(crate) struct RpcSender {
    pub(crate) connection_id: String,
}

impl RpcSender {
    pub(crate) fn new(connection_id: String) -> Self {
        RpcSender { connection_id }
    }

    /// Build RPC message plist dictionary
    fn build_message(&self, selector: &str, args: plist::Dictionary) -> plist::Value {
        let mut rpc_dict = plist::Dictionary::new();
        rpc_dict.insert(
            "__selector".to_string(),
            plist::Value::String(selector.to_string()),
        );
        rpc_dict.insert("__argument".to_string(), plist::Value::Dictionary(args));
        plist::Value::Dictionary(rpc_dict)
    }

    fn new_args(&self) -> plist::Dictionary {
        let mut args = plist::Dictionary::new();
        args.insert(
            "WIRConnectionIdentifierKey".to_string(),
            plist::Value::String(self.connection_id.clone()),
        );
        args
    }

    /// Send _rpc_reportIdentifier
    pub(crate) fn build_report_identifier(&self) -> Vec<u8> {
        let args = self.new_args();
        let msg = self.build_message("_rpc_reportIdentifier:", args);
        plist_to_bin(&msg)
    }

    /// Send _rpc_getConnectedApplications
    #[allow(dead_code)]
    pub(crate) fn build_get_connected_applications(&self) -> Vec<u8> {
        let args = self.new_args();
        let msg = self.build_message("_rpc_getConnectedApplications:", args);
        plist_to_bin(&msg)
    }

    /// Send _rpc_forwardGetListing
    pub(crate) fn build_forward_get_listing(&self, app_id: &str) -> Vec<u8> {
        let mut args = self.new_args();
        args.insert(
            "WIRApplicationIdentifierKey".to_string(),
            plist::Value::String(app_id.to_string()),
        );
        let msg = self.build_message("_rpc_forwardGetListing:", args);
        plist_to_bin(&msg)
    }

    /// Send _rpc_forwardIndicateWebView
    #[allow(dead_code)]
    pub(crate) fn build_forward_indicate_web_view(
        &self,
        app_id: &str,
        page_id: u32,
        is_enabled: bool,
    ) -> Vec<u8> {
        let mut args = self.new_args();
        args.insert(
            "WIRApplicationIdentifierKey".to_string(),
            plist::Value::String(app_id.to_string()),
        );
        args.insert(
            "WIRPageIdentifierKey".to_string(),
            plist::Value::Integer(page_id.into()),
        );
        args.insert(
            "WIRIndicateEnabledKey".to_string(),
            plist::Value::Boolean(is_enabled),
        );
        let msg = self.build_message("_rpc_forwardIndicateWebView:", args);
        plist_to_bin(&msg)
    }

    /// Send _rpc_forwardSocketSetup
    pub(crate) fn build_forward_socket_setup(
        &self,
        app_id: &str,
        page_id: u32,
        sender_id: &str,
    ) -> Vec<u8> {
        let mut args = self.new_args();
        args.insert(
            "WIRApplicationIdentifierKey".to_string(),
            plist::Value::String(app_id.to_string()),
        );
        args.insert(
            "WIRAutomaticallyPause".to_string(),
            plist::Value::Boolean(false),
        );
        args.insert(
            "WIRPageIdentifierKey".to_string(),
            plist::Value::Integer(page_id.into()),
        );
        args.insert(
            "WIRSenderKey".to_string(),
            plist::Value::String(sender_id.to_string()),
        );
        let msg = self.build_message("_rpc_forwardSocketSetup:", args);
        plist_to_bin(&msg)
    }

    /// Send _rpc_forwardSocketData
    pub(crate) fn build_forward_socket_data(
        &self,
        app_id: &str,
        page_id: u32,
        sender_id: &str,
        data: &[u8],
    ) -> Vec<u8> {
        let mut args = self.new_args();
        args.insert(
            "WIRApplicationIdentifierKey".to_string(),
            plist::Value::String(app_id.to_string()),
        );
        args.insert(
            "WIRPageIdentifierKey".to_string(),
            plist::Value::Integer(page_id.into()),
        );
        args.insert(
            "WIRSenderKey".to_string(),
            plist::Value::String(sender_id.to_string()),
        );
        args.insert(
            "WIRSocketDataKey".to_string(),
            plist::Value::Data(data.to_vec()),
        );
        let msg = self.build_message("_rpc_forwardSocketData:", args);
        plist_to_bin(&msg)
    }

    /// Send _rpc_forwardDidClose
    pub(crate) fn build_forward_did_close(
        &self,
        app_id: &str,
        page_id: u32,
        sender_id: &str,
    ) -> Vec<u8> {
        let mut args = self.new_args();
        args.insert(
            "WIRApplicationIdentifierKey".to_string(),
            plist::Value::String(app_id.to_string()),
        );
        args.insert(
            "WIRPageIdentifierKey".to_string(),
            plist::Value::Integer(page_id.into()),
        );
        args.insert(
            "WIRSenderKey".to_string(),
            plist::Value::String(sender_id.to_string()),
        );
        let msg = self.build_message("_rpc_forwardDidClose:", args);
        plist_to_bin(&msg)
    }
}

/// RPC message receive type
#[derive(Debug)]
pub(crate) enum RpcMessage {
    ReportSetup,
    ReportConnectedApplicationList(Vec<RpcApp>),
    ApplicationConnected(RpcApp),
    ApplicationDisconnected(RpcApp),
    ApplicationSentListing {
        app_id: String,
        pages: Vec<RpcPage>,
    },
    ApplicationSentData {
        #[allow(dead_code)]
        app_id: String,
        dest_id: String,
        data: Vec<u8>,
    },
    ApplicationUpdated {
        #[allow(dead_code)]
        app_id: String,
        dest_id: String,
    },
    Unknown(String),
}

/// Parse received RPC plist
pub(crate) fn parse_rpc_message(rpc_dict: &plist::Value) -> Result<RpcMessage> {
    let dict = rpc_dict.as_dictionary().ok_or(Error::Rpc("RPC message is not a dictionary".to_string()))?;

    let selector = dict
        .get("__selector")
        .and_then(|v| v.as_string())
        .ok_or(Error::Rpc("Missing __selector".to_string()))?;

    let args = dict
        .get("__argument")
        .and_then(|v| v.as_dictionary())
        .ok_or(Error::Rpc("Missing __argument".to_string()))?;

    match selector {
        "_rpc_reportSetup:" => Ok(RpcMessage::ReportSetup),

        "_rpc_reportConnectedApplicationList:" => {
            let apps = parse_apps(args)?;
            Ok(RpcMessage::ReportConnectedApplicationList(apps))
        }

        "_rpc_applicationConnected:" => {
            let app = parse_app(args)?;
            Ok(RpcMessage::ApplicationConnected(app))
        }

        "_rpc_applicationDisconnected:" => {
            let app = parse_app(args)?;
            Ok(RpcMessage::ApplicationDisconnected(app))
        }

        "_rpc_applicationSentListing:" => {
            let app_id = get_required_string(args, "WIRApplicationIdentifierKey")?;
            let pages = if let Some(listing) = args.get("WIRListingKey") {
                parse_pages(listing)?
            } else {
                Vec::new()
            };
            Ok(RpcMessage::ApplicationSentListing { app_id, pages })
        }

        "_rpc_applicationSentData:" => {
            let app_id = get_required_string(args, "WIRApplicationIdentifierKey")?;
            let dest_id = get_required_string(args, "WIRDestinationKey")?;
            let data = get_required_data(args, "WIRMessageDataKey")?;
            Ok(RpcMessage::ApplicationSentData {
                app_id,
                dest_id,
                data,
            })
        }

        "_rpc_applicationUpdated:" => {
            // May have WIRHostApplicationIdentifierKey or WIRApplicationNameKey
            let app_id = get_required_string(args, "WIRHostApplicationIdentifierKey")
                .or_else(|_| get_required_string(args, "WIRApplicationNameKey"))?;
            let dest_id = get_required_string(args, "WIRApplicationIdentifierKey")?;
            Ok(RpcMessage::ApplicationUpdated { app_id, dest_id })
        }

        "_rpc_reportConnectedDriverList:" | "_rpc_reportCurrentState:" => {
            Ok(RpcMessage::Unknown(selector.to_string()))
        }

        _ => {
            warn!("Unknown RPC selector: {}", selector);
            Ok(RpcMessage::Unknown(selector.to_string()))
        }
    }
}

/// Parse app dictionary
fn parse_apps(args: &plist::Dictionary) -> Result<Vec<RpcApp>> {
    let app_dict = args
        .get("WIRApplicationDictionaryKey")
        .and_then(|v| v.as_dictionary())
        .ok_or(Error::Rpc("Missing WIRApplicationDictionaryKey".to_string()))?;

    let mut apps = Vec::new();
    for (_key, value) in app_dict {
        if let Some(app_info) = value.as_dictionary() {
            if let Ok(app) = parse_app(app_info) {
                apps.push(app);
            }
        }
    }
    Ok(apps)
}

/// Parse a single app
fn parse_app(dict: &plist::Dictionary) -> Result<RpcApp> {
    let app_id = get_required_string(dict, "WIRApplicationIdentifierKey")?;
    let app_name = dict
        .get("WIRApplicationNameKey")
        .and_then(|v| v.as_string())
        .unwrap_or("")
        .to_string();
    let is_proxy = dict
        .get("WIRIsApplicationProxyKey")
        .and_then(|v| v.as_boolean())
        .unwrap_or(false);

    Ok(RpcApp {
        app_id,
        app_name,
        is_proxy,
    })
}

/// Parse page list
fn parse_pages(listing: &plist::Value) -> Result<Vec<RpcPage>> {
    let dict = listing
        .as_dictionary()
        .ok_or(Error::Rpc("WIRListingKey is not a dictionary".to_string()))?;

    let mut pages = Vec::new();
    for (_key, value) in dict {
        if let Some(page_dict) = value.as_dictionary() {
            let page_id = page_dict
                .get("WIRPageIdentifierKey")
                .and_then(|v| v.as_unsigned_integer())
                .ok_or(Error::Rpc("Missing WIRPageIdentifierKey".to_string()))? as u32;

            let page_type = page_dict
                .get("WIRTypeKey")
                .and_then(|v| {
                    let ty = match v.as_string()? {
                        "WIRTypeWebPage" => PageType::WebPage,
                        "WIRTypeJavaScript" => PageType::JavaScript,
                        _ => PageType::Unknown,
                    };
                    Some(ty)
                })
                .unwrap_or(PageType::Unknown);

            let connection_id = page_dict
                .get("WIRConnectionIdentifierKey")
                .and_then(|v| v.as_string())
                .map(String::from);

            let title = page_dict
                .get("WIRTitleKey")
                .and_then(|v| v.as_string())
                .map(String::from);

            let url = page_dict
                .get("WIRURLKey")
                .and_then(|v| v.as_string())
                .map(String::from);

            pages.push(RpcPage {
                page_id,
                page_type: page_type,
                connection_id,
                title,
                url,
            });
        }
    }
    Ok(pages)
}

// Helper functions
fn get_required_string(dict: &plist::Dictionary, key: &str) -> Result<String> {
    dict.get(key)
        .and_then(|v| v.as_string())
        .map(String::from)
        .ok_or_else(|| Error::Rpc(format!("Missing required string field: {}", key)))
}

fn get_required_data(dict: &plist::Dictionary, key: &str) -> Result<Vec<u8>> {
    dict.get(key)
        .and_then(|v| v.as_data())
        .map(|d| d.to_vec())
        .ok_or_else(|| Error::Rpc(format!("Missing required data field: {}", key)))
}

/// Serialize plist Value to binary format
pub(crate) fn plist_to_bin(value: &plist::Value) -> Vec<u8> {
    let mut buf = Vec::new();
    value.to_writer_binary(&mut buf).expect("plist serialization failed");
    buf
}

/// Deserialize plist Value from binary data
#[allow(dead_code)]
pub(crate) fn plist_from_bin(data: &[u8]) -> Result<plist::Value> {
    plist::from_bytes(data).map_err(|e| Error::Plist(format!("plist deserialization failed: {}", e)))
}
