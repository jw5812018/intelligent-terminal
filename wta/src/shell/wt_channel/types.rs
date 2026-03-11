use serde::{Deserialize, Serialize};

/// Wire format for requests to the WT protocol server.
/// Sent as: `{"type":"request","id":"...","method":"...","params":{...}}\n`
#[derive(Debug, Serialize)]
pub(crate) struct WireRequest<'a> {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub id: String,
    pub method: &'a str,
    pub params: serde_json::Value,
}

/// Wire format for responses from the WT protocol server.
/// Received as: `{"type":"response","id":"...","result":...,"error":...}\n`
#[derive(Debug, Deserialize)]
pub(crate) struct WireResponse {
    pub id: String,
    pub result: Option<serde_json::Value>,
    pub error: Option<WireError>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireError {
    pub code: String,
    pub message: String,
}
