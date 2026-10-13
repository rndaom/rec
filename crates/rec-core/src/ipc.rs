//! Protocol between `recd` and its clients (`rec` CLI, settings UI, Stream
//! Deck plugin, scripts).
//!
//! Transport is a per-user named pipe carrying one JSON object per line: the
//! client writes a [`Request`], the service answers with one [`Response`].

use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Bumped on incompatible protocol changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// `\\.\pipe\rec-<username>`. Named per user so two signed-in users on one
/// machine each talk to their own service.
pub fn pipe_name() -> String {
    let user = std::env::var("USERNAME").unwrap_or_default();
    let user: String = user
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let user = if user.is_empty() {
        "user".to_string()
    } else {
        user
    };
    format!(r"\\.\pipe\rec-{user}")
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    /// Save a clip. `seconds = None` uses the configured length.
    Save {
        seconds: Option<u32>,
    },
    /// Add a timeline marker at the current time.
    Mark,
    Pause,
    Resume,
    ReloadConfig,
    Quit,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Status(Status),
    Saved { path: PathBuf },
    Error { message: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub protocol: u32,
    pub version: String,
    pub state: RecorderState,
    /// Why the recorder is idle, paused or failing, in words a user can act on.
    pub reason: Option<String>,
    /// What is being captured, e.g. the game's executable or a display name.
    pub target: Option<String>,
    pub encoder: Option<String>,
    pub buffered_seconds: f64,
    pub buffer_bytes: u64,
    /// A problem while recording, e.g. the game can't really be captured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderState {
    /// Waiting for a game; nothing is being captured.
    Idle,
    Recording,
    Paused,
    Error,
}

/// Serializes a message as a single line, including the trailing newline.
pub fn encode_line<T: Serialize>(message: &T) -> String {
    let mut line = serde_json::to_string(message).expect("IPC messages always serialize");
    line.push('\n');
    line
}

pub fn decode_line<T: DeserializeOwned>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip() {
        for request in [
            Request::Status,
            Request::Save { seconds: Some(30) },
            Request::Save { seconds: None },
            Request::Mark,
            Request::Quit,
        ] {
            let line = encode_line(&request);
            assert!(line.ends_with('\n') && !line[..line.len() - 1].contains('\n'));
            assert_eq!(decode_line::<Request>(&line).unwrap(), request);
        }
    }

    #[test]
    fn wire_format_is_stable() {
        assert_eq!(
            encode_line(&Request::Save { seconds: Some(15) }),
            "{\"cmd\":\"save\",\"seconds\":15}\n"
        );
        assert_eq!(
            decode_line::<Request>("{\"cmd\":\"save\"}").unwrap(),
            Request::Save { seconds: None }
        );
    }

    #[test]
    fn status_response_round_trips() {
        let response = Response::Status(Status {
            protocol: PROTOCOL_VERSION,
            version: "0.0.1".into(),
            state: RecorderState::Recording,
            reason: None,
            target: Some("game.exe".into()),
            encoder: Some("NVENC HEVC".into()),
            buffered_seconds: 42.5,
            buffer_bytes: 1 << 20,
            warning: Some("the picture is black".into()),
        });
        let line = encode_line(&response);
        assert!(line.starts_with("{\"result\":\"status\""));
        assert_eq!(decode_line::<Response>(&line).unwrap(), response);
        // Older recd builds send no warning.
        let old = line.replace(",\"warning\":\"the picture is black\"", "");
        let Response::Status(status) = decode_line::<Response>(&old).unwrap() else {
            panic!("not a status");
        };
        assert_eq!(status.warning, None);
    }

    #[test]
    fn pipe_name_is_sanitised() {
        let name = pipe_name();
        assert!(name.starts_with(r"\\.\pipe\rec-"));
        assert!(!name["\\\\.\\pipe\\".len()..].contains(['\\', '/', ' ']));
    }
}
