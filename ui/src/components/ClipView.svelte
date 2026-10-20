<script lang="ts">
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import { ask } from "@tauri-apps/plugin-dialog";
  import { api, fileUrl, type Detail } from "../lib/api";
  import { dragOut } from "../lib/drag";
  import { bytes, duration as fmt, inField, precise, when } from "../lib/format";
  import Timeline from "./Timeline.svelte";
  import ExportDialog from "./ExportDialog.svelte";

  let {
    key,
    recording,
    onclose,
    onopen,
    onsay,
  }: {
    key: string;
    recording: boolean;
    onclose: () => void;
    onopen: (key: string) => void;
    onsay: (text: string) => void;
  } = $props();

  let detail = $state<Detail | null>(null);
  let error = $state<string | null>(null);
  let video: HTMLVideoElement | undefined = $state();
  let time = $state(0);
  let length = $state(0);
  let paused = $state(true);
  let playError = $state<string | null>(null);
  let inPoint = $state<number | null>(null);
  let outPoint = $state<number | null>(null);
  let snapped = $state<{ start: number; end: number } | null>(null);
  let trimming = $state(false);
  let exporting = $state(false);
  let volume = $state(1);

  const duration = $derived(length || detail?.duration || 0);
  const track = $derived(
    detail?.tracks.find((t) => t.role === "game") ??
      detail?.tracks.find((t) => t.role === "mix") ??
      detail?.tracks[0],
  );
  const hasRange = $derived(inPoint != null || outPoint != null);
  const jumps = $derived(
    (detail?.events ?? [])
      .filter((e) => e.kind !== "cut")
      .map((e) => e.t)
      .sort((a, b) => a - b),
  );

  async function load() {
    try {
      detail = await api.detail(key);
      error = null;
    } catch (e) {
      error = String(e);
    }
  }

  // Where a lossless trim really cuts, shown on the timeline as it changes.
  // Only the newest request's answer counts; slower older ones are dropped.
  let planRequest = 0;
  $effect(() => {
    const [a, b] = [inPoint ?? 0, outPoint ?? duration];
    const id = ++planRequest;
    if (!hasRange || !detail) {
      snapped = null;
      return;
    }
    const timer = setTimeout(async () => {
      let answer: { start: number; end: number } | null = null;
      try {
        answer = await api.trimPlan(key, a, b);
      } catch {
        answer = null;
      }
      if (id === planRequest) snapped = answer;
    }, 120);
    return () => clearTimeout(timer);
  });

  function seek(t: number) {
    if (video) video.currentTime = Math.min(Math.max(t, 0), duration);
    time = t;
  }

  function toggle() {
    if (!video) return;
    if (video.paused) video.play().catch(() => {});
    else video.pause();
  }

  function step(frames: number) {
    if (!video) return;
    video.pause();
    seek(video.currentTime + frames / (detail?.fps || 60));
  }

  function jump(dir: 1 | -1) {
    const now = video?.currentTime ?? time;
    const next = dir > 0 ? jumps.find((t) => t > now + 0.05) : [...jumps].reverse().find((t) => t < now - 0.3);
    if (next != null) seek(next);
  }

  async function saveTrim() {
    if (!detail) return;
    trimming = true;
    try {
      const done = await api.trim(key, inPoint ?? 0, outPoint ?? duration);
      onsay(`Saved a trimmed copy (${precise(done.start)} to ${precise(done.end)})`);
      inPoint = outPoint = null;
      onopen(done.key);
    } catch (e) {
      onsay(String(e));
    } finally {
      trimming = false;
    }
  }

  async function star() {
    if (!detail) return;
    try {
      await api.star(key, !detail.starred);
      await load();
    } catch (e) {
      onsay(String(e));
    }
  }

  async function remove() {
    if (!detail) return;
    const ok = await ask(`Move ${detail.file} to the Recycle Bin?`, { title: "Delete clip", kind: "warning", okLabel: "Delete" });
    if (!ok) return;
    try {
      video?.pause();
      video?.removeAttribute("src");
      video?.load();
      await api.remove([key]);
      onsay("Moved to the Recycle Bin");
      onclose();
    } catch (e) {
      onsay(String(e));
    }
  }

  async function copy() {
    try {
      await api.copy([key]);
      onsay("Copied: paste into Explorer, Discord or a chat");
    } catch (e) {
      onsay(String(e));
    }
  }

  function onerror() {
    const code = video?.error?.code;
    const codec = detail?.codec?.toUpperCase() ?? "this";
    playError =
      code === 4 || code === 3
        ? `This window can't play ${codec} video on this PC. Windows needs the codec's video extension (HEVC or AV1 Video Extensions from the Microsoft Store), or open the clip in your player.`
        : "The clip couldn't be played here.";
  }

  function onkey(e: KeyboardEvent) {
    if (exporting || inField(e)) return;
    const k = e.key;
    if (k === " " || k === "k") toggle();
    else if (k === "ArrowLeft") seek((video?.currentTime ?? 0) - (e.shiftKey ? 1 : 5));
    else if (k === "ArrowRight") seek((video?.currentTime ?? 0) + (e.shiftKey ? 1 : 5));
    else if (k === ",") step(-1);
    else if (k === ".") step(1);
    else if (k === "i") inPoint = Math.min(video?.currentTime ?? 0, (outPoint ?? duration) - 0.5);
    else if (k === "o") outPoint = Math.max(video?.currentTime ?? 0, (inPoint ?? 0) + 0.5);
    else if (k === "x") inPoint = outPoint = null;
    else if (k === "[") jump(-1);
    else if (k === "]") jump(1);
    else if (k === "Escape") onclose();
    else if (k === "Delete") remove();
    else return;
    e.preventDefault();
  }

  onMount(() => {
    load();
    // Scene cuts and loud moments appear when background analysis finishes.
    const unlisten = listen<string>("clip-updated", (e) => {
      if (e.payload.toLowerCase() === key.toLowerCase()) load();
    });
    return () => {
      unlisten.then((f) => f());
    };
  });
</script>

<svelte:window onkeydown={onkey} />

<header>
  <button class="ghost" onclick={onclose} title="Back (Esc)">← Clips</button>
  {#if detail}
    <div class="title">
      <strong>{detail.game || detail.file}</strong>
      <span class="faint">{when(detail.start_utc ?? detail.modified)} · {fmt(detail.duration)} · {bytes(detail.bytes)}{detail.codec ? ` · ${detail.codec.toUpperCase()}` : ""}{detail.height ? ` ${detail.height}p${detail.fps ?? ""}` : ""}{detail.hdr ? " HDR" : ""}</span>
    </div>
    <button class="star" class:on={detail.starred} onclick={star} title="Starred clips are never deleted by the storage limit">★ {detail.starred ? "Starred" : "Star"}</button>
    <button onclick={copy} title="Copy the file, to paste into Discord, a chat or Explorer">Copy</button>
    <button draggable="true" ondragstart={(e) => dragOut(e, [key], detail?.thumb ?? null)} title="Drag the file into another app">Drag out</button>
    <button onclick={() => (exporting = true)}>Export for Discord…</button>
    <button class="ghost" onclick={() => api.reveal(key)} title="Show in Explorer">Folder</button>
    <button class="ghost" onclick={() => api.open(key)} title="Open in the default player">Open</button>
    <button class="ghost danger" onclick={remove} title="Move to the Recycle Bin (Delete)">Delete</button>
  {/if}
</header>

{#if error}
  <p class="error pad">{error}</p>
{:else if detail}
  <div class="body">
    <div class="stage">
      <!-- svelte-ignore a11y_media_has_caption -->
      <video
        bind:this={video}
        src={fileUrl(detail.video)}
        bind:currentTime={time}
        bind:duration={length}
        bind:paused
        bind:volume
        onclick={toggle}
        {onerror}
        preload="auto"
      ></video>
      {#if playError}
        <div class="playerror">
          <p>{playError}</p>
          <button class="primary" onclick={() => api.open(key)}>Open in player</button>
        </div>
      {/if}
    </div>

    <div class="controls">
      <div class="row">
        <button class="play" onclick={toggle} title="Play / pause (Space)">{paused ? "▶" : "❚❚"}</button>
        <span class="time">{precise(time)} <span class="faint">/ {precise(duration)}</span></span>
        <button class="ghost" onclick={() => jump(-1)} disabled={!jumps.length} title="Previous marker ([)">⟨ ◆</button>
        <button class="ghost" onclick={() => jump(1)} disabled={!jumps.length} title="Next marker (])">◆ ⟩</button>
        <input class="vol" type="range" min="0" max="1" step="0.05" bind:value={volume} title="Volume" />
        <div class="grow"></div>
        <button onclick={() => (inPoint = Math.min(time, (outPoint ?? duration) - 0.5))} title="Start the trim here (I)">Set in</button>
        <button onclick={() => (outPoint = Math.max(time, (inPoint ?? 0) + 0.5))} title="End the trim here (O)">Set out</button>
        {#if hasRange}
          <span class="range dim">
            {precise(snapped?.start ?? inPoint ?? 0)} – {precise(snapped?.end ?? outPoint ?? duration)}
          </span>
          <button class="ghost" onclick={() => (inPoint = outPoint = null)} title="Clear (X)">✕</button>
          <button class="primary" disabled={trimming} onclick={saveTrim} title="Saves a new clip next to this one without re-encoding">
            {trimming ? "Saving…" : "Save trimmed copy"}
          </button>
        {/if}
      </div>
      <Timeline
        {duration}
        {time}
        loudness={track?.loudness ?? []}
        hz={detail.hz || 10}
        events={detail.events}
        {inPoint}
        {outPoint}
        snappedStart={snapped?.start ?? null}
        onseek={seek}
        onrange={(a, b) => {
          inPoint = a;
          outPoint = b;
        }}
      />
      <div class="legend faint">
        <span><i class="m"></i>Marker</span>
        {#if detail.analysis}<span><i class="c"></i>Scene cut</span><span><i class="l"></i>Loud moment</span>{:else}<span>Scene cuts and loud moments appear once the clip is analysed.</span>{/if}
        {#if track}<span>Waveform: {track.name || track.role}</span>{/if}
        <span class="grow"></span>
        <span>Space play · ←/→ 5 s · , . frame · I/O trim · [ ] markers</span>
      </div>
    </div>
  </div>
{/if}

{#if exporting && detail}
  <ExportDialog
    clip={detail}
    {recording}
    start={hasRange ? (inPoint ?? 0) : 0}
    end={hasRange ? (outPoint ?? duration) : null}
    onclose={() => (exporting = false)}
    {onsay}
  />
{/if}

<style>
  header {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 10px 16px;
    border-bottom: 1px solid var(--line);
  }
  header button {
    flex: none;
  }
  .title {
    display: flex;
    flex-direction: column;
    flex: 1 1 0;
    min-width: 0;
    margin-left: 6px;
  }
  .title > * {
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .title span {
    font-size: 12px;
  }
  .star.on {
    color: var(--star);
  }
  .pad {
    padding: 20px;
  }
  .body {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
  }
  .stage {
    flex: 1;
    min-height: 0;
    background: #000;
    position: relative;
    display: flex;
  }
  video {
    width: 100%;
    height: 100%;
    object-fit: contain;
  }
  .playerror {
    position: absolute;
    inset: 0;
    display: grid;
    place-content: center;
    text-align: center;
    padding: 40px;
    gap: 12px;
    background: #000c;
  }
  .playerror p {
    max-width: 520px;
  }
  .controls {
    padding: 10px 16px 12px;
    border-top: 1px solid var(--line);
    background: var(--panel);
  }
  .play {
    width: 40px;
  }
  .time {
    font-variant-numeric: tabular-nums;
    min-width: 130px;
  }
  .vol {
    width: 90px;
    padding: 0;
  }
  .range {
    font-variant-numeric: tabular-nums;
  }
  .legend {
    display: flex;
    gap: 16px;
    font-size: 12px;
    margin-top: 6px;
    align-items: center;
  }
  .legend i {
    display: inline-block;
    width: 9px;
    height: 9px;
    margin-right: 5px;
    vertical-align: -1px;
  }
  .legend .m {
    background: var(--marker);
    transform: rotate(45deg);
  }
  .legend .c {
    width: 2px;
    background: #8b93a7;
  }
  .legend .l {
    background: #f9731666;
  }
</style>
