//! End-to-end tests: build packets from the fixtures the way the replay buffer
//! delivers them, mux, and check the result with ffprobe and ffmpeg when they
//! are installed.

mod common;

use std::io::Cursor;

use common::*;
use rec_core::media::{HdrMetadata, TrackInfo, VideoCodec, VideoColor};
use rec_core::replay::Packet;
use rec_core::time::{HNS_PER_SEC, Hns};
use rec_mux::{Mp4Writer, MuxError, write_clip};

const SEC: f64 = HNS_PER_SEC as f64;

fn mux(tracks: Vec<TrackInfo>, packets: &[Packet]) -> Vec<u8> {
    write_clip(Cursor::new(Vec::new()), tracks, packets)
        .expect("mux")
        .into_inner()
}

struct Scenario {
    tracks: Vec<TrackInfo>,
    /// Packets in arrival order.
    packets: Vec<Packet>,
}

/// Video plus audio tracks, all starting at `BASE`.
fn scenario(codec: VideoCodec, audio: &[(u8, &str, &AudioClip)]) -> Scenario {
    let aus = match codec {
        VideoCodec::H264 => h264_aus(),
        VideoCodec::Hevc => hevc_aus(),
        VideoCodec::Av1 => av1_aus(),
    };
    let mut tracks = vec![video_info(codec)];
    let mut packets = video_packets(&aus, 1);
    for (id, name, clip) in audio {
        tracks.push(audio_info(*id, name, clip));
        packets.extend(audio_packets(*id, clip, BASE, 1));
    }
    Scenario {
        tracks,
        packets: arrival_order(packets),
    }
}

fn near(actual: f64, expected: f64, tolerance: f64, what: &str) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "{what}: expected {expected} +- {tolerance}, got {actual}"
    );
}

const AUDIO_FRAME_48K: f64 = 1024.0 / 48000.0;

// ------------------------------------------------------------- happy paths

#[test]
fn h264_with_one_audio_track() {
    let audio = stereo_48k();
    let s = scenario(VideoCodec::H264, &[(1, "Game audio", &audio)]);
    let bytes = mux(s.tracks, &s.packets);

    // Structure: ftyp, moov, then moof+mdat per GOP.
    let kinds: Vec<_> = top_level_boxes(&bytes).iter().map(|b| b.0).collect();
    let expected: Vec<[u8; 4]> = [
        "ftyp", "moov", "moof", "mdat", "moof", "mdat", "moof", "mdat",
    ]
    .iter()
    .map(|s| s.as_bytes().try_into().unwrap())
    .collect();
    assert_eq!(kinds, expected);

    if !have_ffmpeg("h264_with_one_audio_track") {
        return;
    }
    let path = save("h264_one_audio.mp4", &bytes);
    let p = probe(&path);
    assert_eq!(p.streams.len(), 2);
    let v = p.video();
    assert_eq!(v.get("codec_name"), "h264");
    assert_eq!(v.get("profile"), "High");
    assert_eq!((v.usize("width"), v.usize("height")), (320, 240));
    assert_eq!(v.usize("nb_read_frames"), 90);
    near(v.f64("start_time"), 0.0, 0.0005, "video start");
    near(v.f64("duration"), 3.0, 0.002, "video duration");
    assert_eq!(v.get("TAG:handler_name"), "Video");

    let a = p.audio();
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].get("codec_name"), "aac");
    assert_eq!(a[0].get("profile"), "LC");
    assert_eq!(a[0].usize("sample_rate"), 48000);
    assert_eq!(a[0].usize("channels"), 2);
    assert_eq!(a[0].get("TAG:handler_name"), "Game audio");
    near(a[0].f64("start_time"), 0.0, AUDIO_FRAME_48K, "audio start");
    // Audio ends within one frame of the video (it is cut at the video end).
    near(a[0].f64("duration"), 3.0, AUDIO_FRAME_48K, "audio duration");
    // 141 frames start before the 3.0 s video end.
    assert_eq!(a[0].usize("nb_read_frames"), 141);
    near(
        p.format.f64("duration"),
        3.0,
        AUDIO_FRAME_48K,
        "format duration",
    );

    assert_eq!(decode_errors(&path), "");
}

#[test]
fn hevc_with_two_audio_tracks() {
    let game = stereo_48k();
    let mic = mono_44k();
    let s = scenario(
        VideoCodec::Hevc,
        &[(1, "Game audio", &game), (2, "Microphone", &mic)],
    );
    let bytes = mux(s.tracks, &s.packets);

    if !have_ffmpeg("hevc_with_two_audio_tracks") {
        return;
    }
    let path = save("hevc_two_audio.mp4", &bytes);
    let p = probe(&path);
    assert_eq!(p.streams.len(), 3);
    let v = p.video();
    assert_eq!(v.get("codec_name"), "hevc");
    assert_eq!(v.get("codec_tag_string"), "hvc1");
    assert_eq!(v.get("profile"), "Main");
    assert_eq!((v.usize("width"), v.usize("height")), (320, 240));
    assert_eq!(v.usize("nb_read_frames"), 90);
    near(v.f64("start_time"), 0.0, 0.0005, "video start");
    near(v.f64("duration"), 3.0, 0.002, "video duration");

    let a = p.audio();
    assert_eq!(a.len(), 2);
    assert_eq!(a[0].usize("sample_rate"), 48000);
    assert_eq!(a[0].usize("channels"), 2);
    assert_eq!(a[0].get("TAG:handler_name"), "Game audio");
    assert_eq!(a[1].usize("sample_rate"), 44100);
    assert_eq!(a[1].usize("channels"), 1);
    assert_eq!(a[1].get("TAG:handler_name"), "Microphone");
    // First audio track is the default; the others are present but not enabled.
    assert_eq!(a[0].get("DISPOSITION:default"), "1");
    assert_eq!(a[1].get("DISPOSITION:default"), "0");
    for s in &a {
        near(s.f64("start_time"), 0.0, AUDIO_FRAME_48K, "audio start");
        near(s.f64("duration"), 3.0, AUDIO_FRAME_48K, "audio duration");
    }
    // 3.0 s of 44.1 kHz audio: frames starting before the end.
    assert_eq!(a[0].usize("nb_read_frames"), 141);
    assert_eq!(a[1].usize("nb_read_frames"), 130);

    assert_eq!(decode_errors(&path), "");
}

#[test]
fn three_audio_tracks_one_of_them_silent() {
    // Game, voice and a microphone that never produced a packet.
    let game = stereo_48k();
    let voice = mono_44k();
    let mic = stereo_48k();
    let mut s = scenario(
        VideoCodec::H264,
        &[(1, "Game audio", &game), (2, "Voice chat", &voice)],
    );
    s.tracks.push(audio_info(3, "Microphone", &mic));
    let bytes = mux(s.tracks, &s.packets);

    if !have_ffmpeg("three_audio_tracks") {
        return;
    }
    let path = save("three_audio.mp4", &bytes);
    let p = probe(&path);
    let a = p.audio();
    assert_eq!(a.len(), 3);
    let names: Vec<_> = a.iter().map(|s| s.get("TAG:handler_name")).collect();
    assert_eq!(names, ["Game audio", "Voice chat", "Microphone"]);
    let defaults: Vec<_> = a.iter().map(|s| s.get("DISPOSITION:default")).collect();
    assert_eq!(defaults, ["1", "0", "0"]);
    assert_eq!(a[0].usize("nb_read_frames"), 141);
    assert_eq!(a[1].usize("nb_read_frames"), 130);
    assert_eq!(p.video().usize("nb_read_frames"), 90);
    near(p.format.f64("duration"), 3.0, AUDIO_FRAME_48K, "duration");
    assert_eq!(decode_errors(&path), "");
}

#[test]
fn long_clip_keeps_audio_and_video_in_sync() {
    // 30 s made by repeating the fixtures; a 44.1 kHz track is the worst case
    // for Hns to timescale rounding.
    const LOOPS: usize = 10;
    let mono = mono_44k();
    let mut video = Vec::new();
    for l in 0..LOOPS {
        for (i, au) in h264_aus().into_iter().enumerate() {
            let n = l * 90 + i;
            let pts = frame_pts(n);
            video.push(Packet {
                track: 0,
                segment: 1,
                pts,
                dts: pts,
                duration: HNS_PER_SEC / 30,
                keyframe: au.keyframe,
                data: au.data.into(),
            });
        }
    }
    let frames = (30 * 44100 / 1024) as usize;
    let audio: Vec<Packet> = (0..frames)
        .map(|n| {
            let pts = audio_pts(BASE, 44100, n);
            Packet {
                track: 1,
                segment: 1,
                pts,
                dts: pts,
                duration: 1024 * HNS_PER_SEC / 44100,
                keyframe: true,
                data: mono.frames[n % mono.frames.len()].as_slice().into(),
            }
        })
        .collect();
    let mut packets = video;
    packets.extend(audio);
    let packets = arrival_order(packets);
    let bytes = mux(
        vec![video_info(VideoCodec::H264), audio_info(1, "Mic", &mono)],
        &packets,
    );
    assert_eq!(offsets_of(&bytes, b"moof").len(), 30);
    if !have_ffmpeg("long_clip") {
        return;
    }
    let path = save("long.mp4", &bytes);
    let p = probe(&path);
    assert_eq!(p.video().usize("nb_read_frames"), 90 * LOOPS);
    near(p.format.f64("duration"), 30.0, 0.03, "duration");
    // The last audio frame sits exactly where its index says it should.
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "packet=pts_time",
            "-of",
            "csv=p=0",
        ])
        .arg(&path)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let times: Vec<f64> = text.lines().filter_map(|l| l.trim().parse().ok()).collect();
    assert!(times.len() >= frames - 1, "{} audio packets", times.len());
    for (n, t) in times.iter().enumerate() {
        near(*t, n as f64 * 1024.0 / 44100.0, 0.0001, "audio packet time");
    }
    assert_eq!(decode_errors(&path), "");
}

#[test]
fn av1_with_one_audio_track() {
    let audio = stereo_48k();
    let s = scenario(VideoCodec::Av1, &[(1, "Game audio", &audio)]);
    let bytes = mux(s.tracks, &s.packets);
    assert_eq!(offsets_of(&bytes, b"moof").len(), 3);

    if !have_ffmpeg("av1_with_one_audio_track") {
        return;
    }
    let path = save("av1_one_audio.mp4", &bytes);
    let p = probe(&path);
    let v = p.video();
    assert_eq!(v.get("codec_name"), "av1");
    assert_eq!(v.get("codec_tag_string"), "av01");
    assert_eq!(v.get("profile"), "Main");
    assert_eq!(v.get("pix_fmt"), "yuv420p");
    assert_eq!((v.usize("width"), v.usize("height")), (320, 240));
    assert_eq!(v.usize("nb_read_frames"), 90);
    near(v.f64("start_time"), 0.0, 0.0005, "video start");
    near(v.f64("duration"), 3.0, 0.002, "video duration");
    let a = &p.audio()[0];
    near(a.f64("start_time"), 0.0, AUDIO_FRAME_48K, "audio start");
    near(a.f64("duration"), 3.0, AUDIO_FRAME_48K, "audio duration");
    assert_eq!(decode_errors(&path), "");

    // Cut after the first fragment.
    let cut = offsets_of(&bytes, b"moof")[1];
    let path = save("av1_cut.mp4", &bytes[..cut]);
    assert_eq!(decoded_video_frames(&path), 30);
    assert_eq!(decode_errors(&path), "");
}

#[test]
fn clip_starting_at_a_later_keyframe() {
    // Snapshot taken mid-stream: it starts with non-key frames (dropped) and
    // contains all the audio, including the audio before the clip start.
    let audio = stereo_48k();
    let aus = h264_aus();
    let mut packets = video_packets(&aus, 1)[15..].to_vec();
    packets.extend(audio_packets(1, &audio, BASE, 1));
    let packets = arrival_order(packets);
    let bytes = mux(
        vec![
            video_info(VideoCodec::H264),
            audio_info(1, "Game audio", &audio),
        ],
        &packets,
    );
    assert_eq!(offsets_of(&bytes, b"moof").len(), 2);

    if !have_ffmpeg("clip_starting_at_a_later_keyframe") {
        return;
    }
    let path = save("later_keyframe.mp4", &bytes);
    let p = probe(&path);
    assert_eq!(p.video().usize("nb_read_frames"), 60);
    near(p.video().f64("start_time"), 0.0, 0.0005, "video start");
    near(p.video().f64("duration"), 2.0, 0.002, "video duration");
    let a = &p.audio()[0];
    near(a.f64("start_time"), 0.0, AUDIO_FRAME_48K, "audio start");
    near(a.f64("duration"), 2.0, AUDIO_FRAME_48K, "audio end");
    assert_eq!(decode_errors(&path), "");
}

#[test]
fn audio_starting_after_the_clip_start_is_delayed_not_shifted() {
    // The replay buffer begins at a video keyframe, so the audio that
    // arrived before it is missing: the first audio frame lies after the
    // keyframe and must stay at its true offset.
    let audio = stereo_48k();
    let s = scenario(VideoCodec::H264, &[(1, "Game audio", &audio)]);
    let key = frame_pts(30);
    let start = s
        .packets
        .iter()
        .position(|p| p.track == 0 && p.pts == key)
        .unwrap();
    let snapshot = &s.packets[start..];
    let first_audio = snapshot.iter().find(|p| p.track == 1).unwrap();
    let lead = (first_audio.pts - key) as f64 / SEC;
    assert!(lead > 0.05, "the test needs a real gap, got {lead}");
    let bytes = mux(s.tracks, snapshot);

    if !have_ffmpeg("audio_starting_after_the_clip_start") {
        return;
    }
    let path = save("audio_lead.mp4", &bytes);
    let p = probe(&path);
    assert_eq!(p.video().usize("nb_read_frames"), 60);
    near(p.video().f64("start_time"), 0.0, 0.0005, "video start");
    let a = &p.audio()[0];
    near(a.f64("start_time"), lead, AUDIO_FRAME_48K, "audio start");
    near(a.f64("duration"), 2.0, AUDIO_FRAME_48K, "audio end");
    assert_eq!(decode_errors(&path), "");
}

#[test]
fn video_only_clip() {
    let aus = h264_aus();
    let bytes = mux(vec![video_info(VideoCodec::H264)], &video_packets(&aus, 1));
    if !have_ffmpeg("video_only_clip") {
        return;
    }
    let path = save("video_only.mp4", &bytes);
    let p = probe(&path);
    assert_eq!(p.streams.len(), 1);
    assert_eq!(p.video().usize("nb_read_frames"), 90);
    near(p.format.f64("duration"), 3.0, 0.002, "duration");
    assert_eq!(decode_errors(&path), "");
}

#[test]
fn irregular_timestamps_do_not_break_the_file() {
    // Capture and audio clocks jitter by a few milliseconds.
    let audio = stereo_48k();
    let mut s = scenario(VideoCodec::H264, &[(1, "Game audio", &audio)]);
    let mut seed = 12345u32;
    let mut rnd = |range_ms: i64| {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        (i64::from(seed >> 16) % (2 * range_ms + 1) - range_ms) * 10_000
    };
    for p in &mut s.packets {
        let j = if p.track == 0 { rnd(3) } else { rnd(4) };
        let jittered: Hns = p.pts + j;
        // Video must stay in order; the first keyframe defines time zero.
        if p.track != 0 || p.pts != frame_pts(0) {
            p.pts = jittered;
            p.dts = jittered;
        }
    }
    let bytes = mux(s.tracks, &s.packets);
    if !have_ffmpeg("irregular_timestamps") {
        return;
    }
    let path = save("jitter.mp4", &bytes);
    let p = probe(&path);
    assert_eq!(p.video().usize("nb_read_frames"), 90);
    near(p.format.f64("duration"), 3.0, 0.03, "duration");
    let a = &p.audio()[0];
    near(a.f64("start_time"), 0.0, 0.03, "audio start");
    assert!((140..=142).contains(&a.usize("nb_read_frames")));
    assert_eq!(decode_errors(&path), "");
}

#[test]
fn lost_audio_packets_leave_a_gap_not_a_shift() {
    let audio = stereo_48k();
    let mut s = scenario(VideoCodec::H264, &[(1, "Game audio", &audio)]);
    let all = audio_packets(1, &audio, BASE, 1);
    let lost: Vec<Hns> = all[60..70].iter().map(|p| p.pts).collect();
    s.packets.retain(|p| p.track == 0 || !lost.contains(&p.pts));
    let bytes = mux(s.tracks, &s.packets);
    if !have_ffmpeg("lost_audio_packets") {
        return;
    }
    let path = save("audio_gap.mp4", &bytes);
    let p = probe(&path);
    let a = &p.audio()[0];
    assert_eq!(a.usize("nb_read_frames"), 141 - 10);
    // The timeline keeps its length, so audio after the gap stays in sync.
    near(a.f64("duration"), 3.0, AUDIO_FRAME_48K, "audio duration");
    assert_eq!(decode_errors(&path), "");
}

// ------------------------------------------------------ streaming / crash

#[test]
fn streaming_post_roll_matches_a_one_shot_mux() {
    let audio = stereo_48k();
    let s = scenario(VideoCodec::H264, &[(1, "Game audio", &audio)]);
    let one_shot = mux(s.tracks.clone(), &s.packets);

    // Snapshot first, then the rest arrives live, one packet at a time.
    let split = s.packets.len() * 2 / 3;
    let mut w = Mp4Writer::new(Cursor::new(Vec::new()), s.tracks.clone()).unwrap();
    for p in &s.packets[..split] {
        w.write(p).unwrap();
    }
    let fragments_after_snapshot = w.fragments_written();
    assert!(
        (1..3).contains(&fragments_after_snapshot),
        "completed GOPs are already on disk, got {fragments_after_snapshot}"
    );
    assert!(w.bytes_written() > 0);
    for p in &s.packets[split..] {
        w.write(p).unwrap();
    }
    let streamed = w.finish().unwrap().into_inner();

    // Everything after the init segment is identical; the init segment only
    // differs in the creation time.
    let first_moof = offsets_of(&one_shot, b"moof")[0];
    assert_eq!(first_moof, offsets_of(&streamed, b"moof")[0]);
    assert_eq!(&one_shot[first_moof..], &streamed[first_moof..]);

    if !have_ffmpeg("streaming_post_roll") {
        return;
    }
    let path = save("streamed.mp4", &streamed);
    let p = probe(&path);
    assert_eq!(p.video().usize("nb_read_frames"), 90);
    near(p.format.f64("duration"), 3.0, AUDIO_FRAME_48K, "duration");
    assert_eq!(decode_errors(&path), "");
}

#[test]
fn file_without_finish_plays_its_complete_fragments() {
    // A crash leaves a file with no patched durations and a half-written
    // fragment at the end.
    let audio = stereo_48k();
    let s = scenario(VideoCodec::H264, &[(1, "Game audio", &audio)]);
    let mut sink = Cursor::new(Vec::new());
    {
        let mut w = Mp4Writer::new(&mut sink, s.tracks.clone()).unwrap();
        for p in &s.packets {
            w.write(p).unwrap();
        }
        assert_eq!(w.fragments_written(), 2);
        // dropped without finish()
    }
    let bytes = sink.into_inner();
    assert_eq!(offsets_of(&bytes, b"moof").len(), 2);

    if !have_ffmpeg("file_without_finish") {
        return;
    }
    let path = save("unfinished.mp4", &bytes);
    assert_eq!(decoded_video_frames(&path), 60);
    assert_eq!(decode_errors(&path), "");

    // Cut after the first fragment.
    let second = offsets_of(&bytes, b"moof")[1];
    let path = save("unfinished_1.mp4", &bytes[..second]);
    assert_eq!(decoded_video_frames(&path), 30);
    assert_eq!(decode_errors(&path), "");

    // Cut in the middle of the second fragment's data: the first still plays.
    let mid = second + (bytes.len() - second) / 2;
    let path = save("unfinished_mid.mp4", &bytes[..mid]);
    let frames = decoded_video_frames(&path);
    assert!((30..60).contains(&frames), "got {frames} frames");
}

#[test]
fn finished_file_cut_after_n_fragments_plays_n_gops() {
    for codec in [VideoCodec::H264, VideoCodec::Hevc] {
        let audio = stereo_48k();
        let s = scenario(codec, &[(1, "Game audio", &audio)]);
        let bytes = mux(s.tracks, &s.packets);
        let moofs = offsets_of(&bytes, b"moof");
        assert_eq!(moofs.len(), 3);
        if !have_ffmpeg("finished_file_cut") {
            return;
        }
        for (n, &cut) in moofs.iter().enumerate().skip(1) {
            let path = save(&format!("cut_{codec:?}_{n}.mp4"), &bytes[..cut]);
            assert_eq!(
                decoded_video_frames(&path),
                30 * n,
                "{codec:?} cut after {n}"
            );
            assert_eq!(decode_errors(&path), "", "{codec:?} cut after {n}");
        }
        // Mid-fragment cut: the complete fragments before it still decode.
        let mid = (moofs[2] + bytes.len()) / 2;
        let path = save(&format!("cut_{codec:?}_mid.mp4"), &bytes[..mid]);
        assert!(decoded_video_frames(&path) >= 60);
    }
}

// ------------------------------------------------------------------ errors

fn writer(tracks: Vec<TrackInfo>) -> Mp4Writer<Cursor<Vec<u8>>> {
    Mp4Writer::new(Cursor::new(Vec::new()), tracks).unwrap()
}

fn strip_nals(data: &[u8], drop: impl Fn(u8) -> bool) -> Vec<u8> {
    // Test-side Annex-B rewriter for 4-byte start codes.
    let mut out = Vec::new();
    let mut i = 0;
    let mut starts = Vec::new();
    while i + 4 <= data.len() {
        if data[i..i + 4] == [0, 0, 0, 1] {
            starts.push(i);
            i += 4;
        } else {
            i += 1;
        }
    }
    for (k, &s) in starts.iter().enumerate() {
        let e = starts.get(k + 1).copied().unwrap_or(data.len());
        if !drop(data[s + 4]) {
            out.extend_from_slice(&data[s..e]);
        }
    }
    out
}

#[test]
fn keyframe_without_parameter_sets_is_an_error() {
    let aus = h264_aus();
    let mut packets = video_packets(&aus, 1);
    packets[0].data = strip_nals(&packets[0].data, |h| matches!(h & 0x1F, 7 | 8)).into();
    let mut w = writer(vec![video_info(VideoCodec::H264)]);
    assert!(matches!(
        w.write(&packets[0]),
        Err(MuxError::MissingParameterSets(_))
    ));
    // Only the SPS missing.
    let mut p = video_packets(&aus, 1)[0].clone();
    p.data = strip_nals(&p.data, |h| h & 0x1F == 7).into();
    assert!(matches!(
        w.write(&p),
        Err(MuxError::MissingParameterSets("H.264 SPS"))
    ));

    // HEVC without a VPS.
    let aus = hevc_aus();
    let mut p = video_packets(&aus, 1)[0].clone();
    p.data = strip_nals(&p.data, |h| (h >> 1) & 0x3F == 32).into();
    let mut w = writer(vec![video_info(VideoCodec::Hevc)]);
    assert!(matches!(
        w.write(&p),
        Err(MuxError::MissingParameterSets("HEVC VPS"))
    ));
    // The writer is still usable and can start on a good keyframe.
    w.write(&video_packets(&aus, 1)[0]).unwrap();
}

#[test]
fn av1_keyframe_without_sequence_header_is_an_error() {
    let mut p = video_packets(&av1_aus(), 1)[0].clone();
    // Temporal delimiter (2 bytes), then the sequence header OBU.
    assert_eq!((p.data[2] >> 3) & 0x0F, 1, "fixture layout");
    let seq_len = 2 + usize::from(p.data[3]);
    let mut stripped = p.data[..2].to_vec();
    stripped.extend_from_slice(&p.data[2 + seq_len..]);
    p.data = stripped.into();
    let mut w = writer(vec![video_info(VideoCodec::Av1)]);
    assert!(matches!(
        w.write(&p),
        Err(MuxError::MissingParameterSets("AV1 sequence header"))
    ));
}

#[test]
fn corrupt_parameter_sets_and_payloads_are_errors() {
    let aus = h264_aus();
    let good = video_packets(&aus, 1);
    let mut w = writer(vec![video_info(VideoCodec::H264)]);

    // Not Annex-B at all.
    let mut p = good[0].clone();
    p.data = vec![0x65u8; 64].into();
    assert!(matches!(w.write(&p), Err(MuxError::Malformed { .. })));
    // Empty payload.
    p.data = Vec::<u8>::new().into();
    assert!(w.write(&p).is_err());
    // Truncated SPS.
    p.data = vec![
        0, 0, 0, 1, 0x67, 0x64, 0, 0, 0, 0, 1, 0x68, 0xEE, 0, 0, 0, 1, 0x65, 0x88,
    ]
    .into();
    assert!(w.write(&p).is_err());

    // A broken access unit in the middle of a clip does not poison the writer.
    w.write(&good[0]).unwrap();
    w.write(&good[1]).unwrap();
    let mut bad = good[2].clone();
    bad.data = vec![1, 2, 3, 4].into();
    assert!(matches!(w.write(&bad), Err(MuxError::Malformed { .. })));
    for p in &good[2..] {
        w.write(p).unwrap();
    }
    let bytes = w.finish().unwrap().into_inner();
    if have_ffmpeg("corrupt_parameter_sets") {
        let path = save("after_bad_au.mp4", &bytes);
        assert_eq!(probe(&path).video().usize("nb_read_frames"), 90);
    }
}

#[test]
fn unknown_track_is_an_error() {
    let audio = stereo_48k();
    let mut w = writer(vec![
        video_info(VideoCodec::H264),
        audio_info(1, "Game audio", &audio),
    ]);
    let mut p = audio_packets(1, &audio, BASE, 1)[0].clone();
    p.track = 7;
    assert!(matches!(w.write(&p), Err(MuxError::UnknownTrack(7))));
}

#[test]
fn non_monotonic_video_pts_is_an_error_and_recoverable() {
    let aus = h264_aus();
    let packets = video_packets(&aus, 1);
    let mut w = writer(vec![video_info(VideoCodec::H264)]);
    w.write(&packets[0]).unwrap();
    w.write(&packets[1]).unwrap();
    w.write(&packets[2]).unwrap();
    // Repeated and backwards timestamps.
    assert!(matches!(
        w.write(&packets[2]),
        Err(MuxError::NonMonotonicVideo { .. })
    ));
    assert!(matches!(
        w.write(&packets[1]),
        Err(MuxError::NonMonotonicVideo { .. })
    ));
    // A keyframe that goes backwards must not flush the open GOP.
    let mut back = packets[3].clone();
    back.keyframe = true;
    back.pts = packets[1].pts;
    assert!(matches!(
        w.write(&back),
        Err(MuxError::NonMonotonicVideo { .. })
    ));
    assert_eq!(w.fragments_written(), 0);
    for p in &packets[3..] {
        w.write(p).unwrap();
    }
    let bytes = w.finish().unwrap().into_inner();
    if have_ffmpeg("non_monotonic") {
        let path = save("non_monotonic.mp4", &bytes);
        assert_eq!(probe(&path).video().usize("nb_read_frames"), 90);
    }
}

#[test]
fn packets_from_another_segment_are_rejected() {
    let audio = stereo_48k();
    let tracks = vec![
        video_info(VideoCodec::H264),
        audio_info(1, "Game audio", &audio),
    ];
    let video = video_packets(&h264_aus(), 5);
    let mut w = writer(tracks.clone());
    w.write(&video[0]).unwrap();
    let mut v2 = video[1].clone();
    v2.segment = 6;
    assert!(matches!(
        w.write(&v2),
        Err(MuxError::SegmentChange {
            expected: 5,
            found: 6
        })
    ));
    let a = audio_packets(1, &audio, BASE, 4);
    assert!(matches!(
        w.write(&a[0]),
        Err(MuxError::SegmentChange {
            expected: 5,
            found: 4
        })
    ));
    // A new segment on a keyframe later in the clip is rejected too.
    for p in &video[1..30] {
        w.write(p).unwrap();
    }
    let mut key = video[30].clone();
    key.segment = 9;
    assert!(matches!(w.write(&key), Err(MuxError::SegmentChange { .. })));
    assert_eq!(w.fragments_written(), 0);

    // Audio held back before the first video packet is checked against the
    // video's segment once it shows up.
    let mut w = writer(tracks);
    w.write(&a[0]).unwrap();
    assert!(matches!(
        w.write(&video[0]),
        Err(MuxError::SegmentChange {
            expected: 5,
            found: 4
        })
    ));
}

#[test]
fn invalid_track_lists_are_rejected() {
    let audio = stereo_48k();
    let ok_audio = audio_info(1, "a", &audio);
    let new = |tracks: Vec<TrackInfo>| Mp4Writer::new(Cursor::new(Vec::new()), tracks).map(|_| ());
    assert!(matches!(new(vec![]), Err(MuxError::InvalidTracks(_))));
    assert!(matches!(
        new(vec![ok_audio.clone()]),
        Err(MuxError::InvalidTracks(_))
    ));
    // Duplicate ids.
    let mut dup = audio_info(1, "b", &audio);
    dup.id = 1;
    assert!(matches!(
        new(vec![video_info(VideoCodec::H264), ok_audio.clone(), dup]),
        Err(MuxError::InvalidTracks(_))
    ));
    // Video not on track 0, audio on track 0, two videos.
    let mut v = video_info(VideoCodec::H264);
    v.id = 3;
    assert!(matches!(new(vec![v]), Err(MuxError::InvalidTracks(_))));
    let mut a0 = ok_audio.clone();
    a0.id = 0;
    assert!(matches!(
        new(vec![video_info(VideoCodec::H264), a0]),
        Err(MuxError::InvalidTracks(_))
    ));
    // Empty AudioSpecificConfig, zero rate, zero size.
    let mut empty = audio.format.clone();
    empty.codec_config.clear();
    let bad = TrackInfo {
        id: 2,
        name: "x".into(),
        kind: rec_core::media::TrackKind::Audio(empty),
    };
    assert!(matches!(
        new(vec![video_info(VideoCodec::H264), bad]),
        Err(MuxError::InvalidTracks(_))
    ));
    let mut zero = audio.format.clone();
    zero.sample_rate = 0;
    let bad = TrackInfo {
        id: 2,
        name: "x".into(),
        kind: rec_core::media::TrackKind::Audio(zero),
    };
    assert!(matches!(
        new(vec![video_info(VideoCodec::H264), bad]),
        Err(MuxError::InvalidTracks(_))
    ));
    let mut v = video_info(VideoCodec::H264);
    if let rec_core::media::TrackKind::Video(f) = &mut v.kind {
        f.width = 0;
    }
    assert!(matches!(new(vec![v]), Err(MuxError::InvalidTracks(_))));
    assert!(new(vec![video_info(VideoCodec::H264), ok_audio]).is_ok());
}

#[test]
fn finishing_without_a_keyframe_is_an_error() {
    let packets = video_packets(&h264_aus(), 1);
    let w = writer(vec![video_info(VideoCodec::H264)]);
    assert!(matches!(w.finish(), Err(MuxError::NoVideo)));
    // Only non-key frames: all dropped.
    let mut w = writer(vec![video_info(VideoCodec::H264)]);
    for p in &packets[1..10] {
        w.write(p).unwrap();
    }
    assert!(matches!(w.finish(), Err(MuxError::NoVideo)));
}

#[test]
fn empty_audio_packet_is_an_error() {
    let audio = stereo_48k();
    let mut w = writer(vec![
        video_info(VideoCodec::H264),
        audio_info(1, "Game audio", &audio),
    ]);
    let mut p = audio_packets(1, &audio, BASE, 1)[0].clone();
    p.data = Vec::<u8>::new().into();
    assert!(matches!(w.write(&p), Err(MuxError::Malformed { .. })));
}

#[test]
fn single_keyframe_clip_is_valid() {
    // One GOP only: a single fragment, flushed at finish().
    let audio = stereo_48k();
    let mut s = scenario(VideoCodec::H264, &[(1, "Game audio", &audio)]);
    s.packets.retain(|p| p.track != 0 || p.pts < frame_pts(10));
    let bytes = mux(s.tracks, &s.packets);
    assert_eq!(offsets_of(&bytes, b"moof").len(), 1);
    if !have_ffmpeg("single_keyframe_clip") {
        return;
    }
    let path = save("single.mp4", &bytes);
    let p = probe(&path);
    assert_eq!(p.video().usize("nb_read_frames"), 10);
    near(
        p.video().f64("duration"),
        10.0 / 30.0,
        0.002,
        "video duration",
    );
    assert_eq!(decode_errors(&path), "");
}

/// A sink that fails after a number of bytes.
struct Failing {
    inner: Cursor<Vec<u8>>,
    budget: usize,
}

impl std::io::Write for Failing {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.len() > self.budget {
            return Err(std::io::Error::other("disk full"));
        }
        self.budget -= buf.len();
        self.inner.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl std::io::Seek for Failing {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

#[test]
fn io_errors_are_reported_and_poison_the_writer() {
    let packets = video_packets(&h264_aus(), 1);
    let sink = Failing {
        inner: Cursor::new(Vec::new()),
        budget: 3000,
    };
    let mut w = Mp4Writer::new(sink, vec![video_info(VideoCodec::H264)]).unwrap();
    let mut failed = None;
    for (i, p) in packets.iter().enumerate() {
        if let Err(e) = w.write(p) {
            failed = Some((i, e));
            break;
        }
    }
    let (i, e) = failed.expect("the sink must run out of space");
    assert!(matches!(e, MuxError::Io(_)), "{e:?}");
    assert!(i >= 30);
    assert!(matches!(w.write(&packets[i + 1]), Err(MuxError::Poisoned)));
    assert!(matches!(w.finish(), Err(MuxError::Poisoned)));
}

// ------------------------------------------------------------------ colour

fn hdr10() -> VideoColor {
    VideoColor::Hdr10(HdrMetadata {
        red: [35_400, 14_600],
        green: [8_500, 39_850],
        blue: [6_550, 2_300],
        white: [15_635, 16_450],
        max_luminance: 10_000_000, // 1000 cd/m2
        min_luminance: 50,         // 0.005 cd/m2
        max_cll: 1000,
        max_fall: 400,
    })
}

fn fixture_aus(codec: VideoCodec) -> Vec<VideoAu> {
    match codec {
        VideoCodec::H264 => h264_aus(),
        VideoCodec::Hevc => hevc_aus(),
        VideoCodec::Av1 => av1_aus(),
    }
}

fn mux_video(codec: VideoCodec, color: VideoColor, aus: &[VideoAu]) -> Vec<u8> {
    mux(
        vec![video_info_with_color(codec, color)],
        &video_packets(aus, 1),
    )
}

#[test]
fn every_codec_describes_sdr_colour_with_a_colr_box_only() {
    for (codec, config_box) in [
        (VideoCodec::H264, b"avcC"),
        (VideoCodec::Hevc, b"hvcC"),
        (VideoCodec::Av1, b"av1C"),
    ] {
        let bytes = mux_video(codec, VideoColor::Sdr, &fixture_aus(codec));
        let (_, boxes) = video_sample_entry(&bytes);
        let kinds: Vec<_> = boxes.iter().map(|b| b.0).collect();
        assert_eq!(kinds, [*config_box, *b"colr"], "{codec:?}");
        // nclx, BT.709 primaries / transfer / matrix, limited range.
        assert_eq!(
            boxes[1].1,
            [b'n', b'c', b'l', b'x', 0, 1, 0, 1, 0, 1, 0],
            "{codec:?}"
        );
    }
}

#[test]
fn every_codec_describes_hdr10_with_colr_mdcv_clli() {
    // The muxer trusts VideoFormat::color, so the 8-bit fixtures are enough to
    // check the boxes for each sample entry type.
    for (codec, config_box) in [
        (VideoCodec::H264, b"avcC"),
        (VideoCodec::Hevc, b"hvcC"),
        (VideoCodec::Av1, b"av1C"),
    ] {
        let bytes = mux_video(codec, hdr10(), &fixture_aus(codec));
        let (_, boxes) = video_sample_entry(&bytes);
        let kinds: Vec<_> = boxes.iter().map(|b| b.0).collect();
        assert_eq!(
            kinds,
            [*config_box, *b"colr", *b"mdcv", *b"clli"],
            "{codec:?}"
        );
        assert_eq!(
            boxes[1].1,
            [b'n', b'c', b'l', b'x', 0, 9, 0, 16, 0, 9, 0],
            "{codec:?}"
        );
        // green, blue, red, white point, max / min luminance
        let mdcv: Vec<u8> = [
            [
                8_500u16, 39_850, 6_550, 2_300, 35_400, 14_600, 15_635, 16_450,
            ]
            .iter()
            .flat_map(|v| v.to_be_bytes())
            .collect::<Vec<_>>(),
            10_000_000u32.to_be_bytes().to_vec(),
            50u32.to_be_bytes().to_vec(),
        ]
        .concat();
        assert_eq!(boxes[2].1, mdcv, "{codec:?}");
        assert_eq!(boxes[3].1, [0x03, 0xE8, 0x01, 0x90], "{codec:?}");
    }
}

#[test]
fn sdr_colour_is_reported_by_ffprobe() {
    if !have_ffmpeg("sdr_colour_is_reported_by_ffprobe") {
        return;
    }
    for (codec, name) in [
        (VideoCodec::H264, "sdr_h264.mp4"),
        (VideoCodec::Hevc, "sdr_hevc.mp4"),
        (VideoCodec::Av1, "sdr_av1.mp4"),
    ] {
        let path = save(
            name,
            &mux_video(codec, VideoColor::Sdr, &fixture_aus(codec)),
        );
        let p = probe(&path);
        let v = p.video();
        assert_eq!(v.get("color_primaries"), "bt709", "{codec:?}");
        assert_eq!(v.get("color_transfer"), "bt709", "{codec:?}");
        assert_eq!(v.get("color_space"), "bt709", "{codec:?}");
        assert_eq!(v.get("color_range"), "tv", "{codec:?}");
        assert!(video_side_data(&path).is_empty(), "{codec:?}");
        assert_eq!(decode_errors(&path), "", "{codec:?}");
    }
}

#[test]
fn hdr10_main10_hevc_track_is_reported_by_ffprobe() {
    let bytes = mux_video(VideoCodec::Hevc, hdr10(), &hevc10_aus());
    let (entry, _) = video_sample_entry(&bytes);
    assert_eq!(&entry, b"hvc1");

    if !have_ffmpeg("hdr10_main10_hevc_track_is_reported_by_ffprobe") {
        return;
    }
    let path = save("hdr10_hevc.mp4", &bytes);
    let p = probe(&path);
    let v = p.video();
    // hvcC is built from the 10-bit SPS.
    assert_eq!(v.get("codec_name"), "hevc");
    assert_eq!(v.get("profile"), "Main 10");
    assert_eq!(v.get("pix_fmt"), "yuv420p10le");
    assert_eq!(v.usize("nb_read_frames"), 90);
    // The bitstream has no colour VUI, so these come from colr.
    assert_eq!(v.get("color_primaries"), "bt2020");
    assert_eq!(v.get("color_transfer"), "smpte2084");
    assert_eq!(v.get("color_space"), "bt2020nc");
    assert_eq!(v.get("color_range"), "tv");

    let side = video_side_data(&path);
    let find = |kind: &str| {
        side.iter()
            .find(|s| s.get("side_data_type").is_some_and(|t| t == kind))
            .unwrap_or_else(|| panic!("no {kind} side data in {side:?}"))
    };
    let mastering = find("Mastering display metadata");
    for (key, expected) in [
        ("red_x", "35400/50000"),
        ("red_y", "14600/50000"),
        ("green_x", "8500/50000"),
        ("green_y", "39850/50000"),
        ("blue_x", "6550/50000"),
        ("blue_y", "2300/50000"),
        ("white_point_x", "15635/50000"),
        ("white_point_y", "16450/50000"),
        ("min_luminance", "50/10000"),
        ("max_luminance", "10000000/10000"),
    ] {
        assert_eq!(
            mastering.get(key).map(String::as_str),
            Some(expected),
            "{key}"
        );
    }
    let light = find("Content light level metadata");
    assert_eq!(light.get("max_content").map(String::as_str), Some("1000"));
    assert_eq!(light.get("max_average").map(String::as_str), Some("400"));

    assert_eq!(decode_errors(&path), "");
}
