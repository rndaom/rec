//! Clip file writing. Placeholder until `rec-mux` is merged.

use std::io::{Seek, Write};

use rec_core::media::TrackInfo;
use rec_core::replay::Packet;

pub fn write<W: Write + Seek>(
    _out: W,
    _tracks: Vec<TrackInfo>,
    _packets: &[Packet],
) -> Result<(), String> {
    Err("the MP4 writer is not available in this build".into())
}
