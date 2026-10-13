//! One recording session: capture + encode pipelines feeding the replay
//! buffer for one target. Placeholder until `rec-video` and `rec-audio` are
//! merged.

use std::sync::{Arc, Mutex};

use rec_core::config::Config;
use rec_core::media::TrackInfo;
use rec_core::replay::ReplayBuffer;

use crate::win::Hwnd;

/// What a session records.
#[derive(Clone, Debug)]
pub struct Target {
    pub hwnd: Hwnd,
    /// Friendly name used for folders and file names.
    pub game: String,
    pub exe_name: String,
    /// Record the whole display instead of just the window.
    pub display: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Health {
    Ok,
    SourceClosed,
    Failed(String),
}

#[derive(Debug)]
pub struct Session {
    pub target: Target,
    buffer: Arc<Mutex<ReplayBuffer>>,
    tracks: Vec<TrackInfo>,
}

impl Session {
    pub fn start(_config: &Config, _target: Target) -> Result<Self, String> {
        Err("capture is not available in this build".into())
    }

    pub fn health(&self) -> Health {
        Health::Ok
    }

    pub fn buffer(&self) -> Arc<Mutex<ReplayBuffer>> {
        self.buffer.clone()
    }

    pub fn tracks(&self) -> Vec<TrackInfo> {
        self.tracks.clone()
    }

    /// Short description of the pipeline, e.g. "HEVC 2560x1440@60 on RX 7800 XT".
    pub fn describe(&self) -> String {
        String::new()
    }
}
