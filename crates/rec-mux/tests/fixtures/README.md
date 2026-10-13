# Test fixtures

Small elementary streams used by the integration tests, all generated with
FFmpeg 9.x (`ffmpeg` is only needed to regenerate them, never at runtime).
Total size is about 230 KB. The tests expect 3 s of 30 fps video (90 frames,
an IDR every 30 frames, no B-frames, one slice per picture) and slightly more
than 3 s of AAC audio.

Run the commands from this directory.

```sh
# H.264 (Annex-B), High profile, SPS+PPS repeated before every IDR
ffmpeg -y -f lavfi -i testsrc2=size=320x240:rate=30:duration=3 \
  -c:v libx264 -preset medium -profile:v high -pix_fmt yuv420p \
  -bf 0 -g 30 -keyint_min 30 -sc_threshold 0 -crf 32 \
  -x264-params "slices=1:repeat-headers=1:aud=0" \
  -f h264 h264_320x240_30fps.h264

# HEVC (Annex-B), Main profile, VPS+SPS+PPS repeated before every IDR
ffmpeg -y -f lavfi -i testsrc2=size=320x240:rate=30:duration=3 \
  -c:v libx265 -preset medium -pix_fmt yuv420p -crf 32 \
  -x265-params "bframes=0:keyint=30:min-keyint=30:scenecut=0:repeat-headers=1:open-gop=0:log-level=error" \
  -f hevc hevc_320x240_30fps.h265

# HEVC Main 10 (Annex-B), 10-bit 4:2:0, same structure as the Main fixture.
# It has no colour VUI and no SEI, so HDR10 tags in the tests can only come
# from the MP4 container.
ffmpeg -y -f lavfi -i testsrc2=size=320x240:rate=30:duration=3 \
  -c:v libx265 -preset medium -pix_fmt yuv420p10le -crf 36 \
  -x265-params "bframes=0:keyint=30:min-keyint=30:scenecut=0:repeat-headers=1:open-gop=0:log-level=error" \
  -f hevc hevc10_320x240_30fps.h265

# AV1 (IVF; every frame is a temporal unit of low-overhead OBUs, the
# sequence header is repeated on each keyframe), low-delay structure so the
# output order equals the decode order
ffmpeg -y -f lavfi -i testsrc2=size=320x240:rate=30:duration=3 \
  -c:v libsvtav1 -pix_fmt yuv420p -g 30 -crf 45 -preset 8 \
  -svtav1-params "keyint=30:scd=0:pred-struct=1:lookahead=0:irefresh-type=2" \
  -f ivf av1_320x240_30fps.ivf

# AAC-LC 48 kHz stereo, 3.2 s (ADTS; the tests strip the headers and build
# the AudioSpecificConfig)
ffmpeg -y -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=3.2" \
  -ac 2 -c:a aac -profile:a aac_low -b:a 48k -f adts aac_48k_stereo.aac

# AAC-LC 44.1 kHz mono, 3.2 s
ffmpeg -y -f lavfi -i "sine=frequency=880:sample_rate=44100:duration=3.2" \
  -ac 1 -c:a aac -profile:a aac_low -b:a 24k -f adts aac_44k_mono.aac
```
