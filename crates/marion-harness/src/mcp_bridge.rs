use std::ffi::OsString;

pub const AGENT_ID_ENV: &str = "MARION_AGENT_ID";
pub const AGENT_TYPE_ENV: &str = "MARION_AGENT_TYPE";
pub const AUTH_ENV: &str = "MARION_AUTH";
pub const BASE_URL_ENV: &str = "MARION_BASE_URL";
pub const DEPTH_ENV: &str = "MARION_DEPTH";
pub const NODE_TOKEN_ENV: &str = "MARION_NODE_TOKEN";
pub const READY_FILE_ENV: &str = "MARION_READY_FILE";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarionMcpBridge {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
}
