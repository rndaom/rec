//! Clip file writing via `rec-mux`.

use std::io::{Seek, Write};

use rec_core::media::TrackInfo;
use rec_core::replay::Packet;

pub fn write<W: Write + Seek>(
    out: W,
    tracks: Vec<TrackInfo>,
    packets: &[Packet],
) -> Result<(), String> {
    let mut out = rec_mux::write_clip(out, tracks, packets)
        .map_err(|e| format!("could not write the clip: {e}"))?;
    out.flush()
        .map_err(|e| format!("could not write the clip: {e}"))
}
