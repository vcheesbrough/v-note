use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthResponse {
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MetaResponse {
    pub app_name: String,
    pub app_version: String,
    pub protocol_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MeResponse {
    pub sub: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}
