//! Private, versioned control framing for the pane-owned execution helper.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};

pub const VERSION: u32 = 1;
pub const TERMINAL_SESSION_VERSION: u32 = 3;
pub const MAX_FRAME: usize = 262_144;

#[derive(Deserialize, Serialize)]
pub struct Bootstrap {
    pub version: u32,
    pub execution_token: String,
    pub authentication: String,
}

#[derive(Deserialize, Serialize)]
pub struct Request {
    pub version: u32,
    pub execution_token: String,
    pub authentication: String,
    pub operation: Operation,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum Operation {
    Hello,
    Start {
        mutation_id: String,
        program: String,
        args: Vec<String>,
        cwd: Option<String>,
        env: BTreeMap<String, String>,
    },
    Status,
    Term {
        mutation_id: String,
    },
    Sweep {
        mutation_id: String,
    },
    // V3 only: release a reaped terminal-session owner, not descendant cleanup.
    RetireOwner {
        mutation_id: String,
    },
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Response {
    pub version: u32,
    pub execution_token: String,
    pub started: bool,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<i32>,
    pub group_consumed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_session_ended: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup_error: Option<String>,
    pub error: Option<String>,
}

pub fn read_frame<T: serde::de::DeserializeOwned>(input: &mut impl Read) -> std::io::Result<T> {
    let mut length = [0; 4];
    input.read_exact(&mut length)?;
    let length = usize::try_from(u32::from_be_bytes(length)).map_err(std::io::Error::other)?;
    if length > MAX_FRAME {
        return Err(std::io::Error::other("supervisor frame exceeds limit"));
    }
    let mut bytes = vec![0; length];
    input.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(std::io::Error::other)
}

pub fn write_frame<T: Serialize>(output: &mut impl Write, value: &T) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    if bytes.len() > MAX_FRAME {
        return Err(std::io::Error::other("supervisor frame exceeds limit"));
    }
    output.write_all(
        &u32::try_from(bytes.len())
            .map_err(std::io::Error::other)?
            .to_be_bytes(),
    )?;
    output.write_all(&bytes)?;
    output.flush()
}
