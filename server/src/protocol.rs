//! Wire protocol shared between the MCP server and the Vim plugin.
//!
//! Messages are newline-delimited JSON over the Unix socket. This module
//! defines the exact shapes the VimScript plugin sends and expects, so the
//! plugin needs no changes.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Info a Vim instance reports when it registers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceInfo {
    pub pid: i64,
    pub cwd: String,
    #[serde(default)]
    pub main_file: String,
    #[serde(default)]
    pub buffers: Vec<String>,
    #[serde(default)]
    pub version: Value,
}

/// A message arriving from a Vim instance.
///
/// Vim sends either a `register` / `state_update` push (tagged by `type`) or a
/// response to a request we sent (identified by `id`). We deserialize
/// leniently: presence of `type` vs `id` disambiguates.
#[derive(Debug, Deserialize)]
pub struct IncomingMessage {
    #[serde(rename = "type", default)]
    pub msg_type: Option<String>,
    #[serde(default)]
    pub instance_id: Option<String>,
    #[serde(default)]
    pub info: Option<InstanceInfo>,
    #[serde(default)]
    pub state: Option<Value>,
    // Response fields
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<ResponseError>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResponseError {
    pub message: String,
}

/// A request we send to Vim: `{"id":N,"method":"...","params":{...}}\n`.
#[derive(Debug, Serialize)]
pub struct Request {
    pub id: i64,
    pub method: &'static str,
    pub params: Value,
}

/// Acknowledgment we send back after a successful registration.
#[derive(Debug, Serialize)]
pub struct Registered {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub instance_id: String,
}

impl Registered {
    pub fn new(instance_id: String) -> Self {
        Registered {
            msg_type: "registered",
            instance_id,
        }
    }
}
