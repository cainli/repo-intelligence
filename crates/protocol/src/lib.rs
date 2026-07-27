use serde::Serialize;

pub const PROTOCOL_VERSION: &str = "1";

#[derive(Clone, Debug, Serialize)]
pub struct Envelope<T: Serialize> {
    pub protocol_version: &'static str,
    pub status: &'static str,
    pub data: T,
}

impl<T: Serialize> Envelope<T> {
    pub fn success(data: T) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            status: "success",
            data,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ErrorEnvelope {
    pub protocol_version: &'static str,
    pub status: &'static str,
    pub error: ProtocolError,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProtocolError {
    pub code: String,
    pub message: String,
}

impl ErrorEnvelope {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            status: "error",
            error: ProtocolError {
                code: code.into(),
                message: message.into(),
            },
        }
    }
}
