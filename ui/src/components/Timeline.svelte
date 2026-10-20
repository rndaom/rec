<script lang="ts">
  // The marker timeline: the game sound's loudness, the clip's events
  // (markers, scene cuts, loud moments) and the trim range, over a seek bar.
  import type { ClipEvent } from "../lib/api";
  import { precise } from "../lib/format";

  let {
    duration,
    time,
    loudness,
    hz,
    events,
    inPoint,
    outPoint,
    snappedStart,
    onseek,
    onrange,
  }: {
    duration: number;
    time: number;
    loudness: number[];
    hz: number;
    events: ClipEvent[];
    inPoint: number | null;
    outPoint: number | null;
    /** Where a lossless cut really starts: the keyframe at or before `inPoint`. */
    snappedStart: number | null;
    onseek: (t: number) => void;
    onrange: (inPoint: number | null, outPoint: number | null) => void;
  } = $props();

  let bar: HTMLDivElement;
  let dragging: "seek" | "in" | "out" | null = null;
  let hover = $state<{ x: number; text: string } | null>(null);

  const W = 1000;
  const H = 56;
  const pct = (t: number) => (duration > 0 ? (Math.min(Math.max(t, 0), duration) / duration) * 100 : 0);
  const at = (clientX: number) => {
    const r = bar.getBoundingClientRect();
    return Math.min(Math.max((clientX - r.left) / r.width, 0), 1) * duration;
  };

  const wave = $derived.by(() => {
    if (!loudness.length || duration <= 0) return "";
    const step = Math.max(1, Math.floor(loudness.length / W));
    let d = `M0 ${H}`;
    for (let i = 0; i < loudness.length; i += step) {
      let v = 0;
      for (let j = i; j < Math.min(i + step, loudness.length); j++) v = Math.max(v, loudness[j]);
      const x = ((i / hz) / duration) * W;
      d += ` L${x.toFixed(1)} ${(H - (v / 255) * (H - 4)).toFixed(1)}`;
    }
    return d + ` L${W} ${H} Z`;
  });

  const markers = $derived(events.filter((e) => e.kind === "marker"));
  const cuts = $derived(events.filter((e) => e.kind === "cut"));
  const louds = $derived(events.filter((e) => e.kind === "loud"));
  const others = $derived(events.filter((e) => !["marker", "cut", "loud"].includes(e.kind)));

  function down(e: PointerEvent, what: "seek" | "in" | "out") {
    e.stopPropagation();
    dragging = what;
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    move(e);
  }

  function move(e: PointerEvent) {
    const t = at(e.clientX);
    const r = bar.getBoundingClientRect();
    hover = { x: e.clientX - r.left, text: precise(t) };
    if (dragging === "seek") onseek(t);
    else if (dragging === "in") onrange(Math.min(t, (outPoint ?? duration) - 0.5), outPoint);
    else if (dragging === "out") onrange(inPoint, Math.max(t, (inPoint ?? 0) + 0.5));
  }

  function label(e: ClipEvent) {
    const names: Record<string, string> = { marker: "Marker", cut: "Scene cut", loud: "Loud moment" };
    const src = e.src ? ` (${e.src})` : "";
    return `${names[e.kind] ?? e.kind}${src} at ${precise(e.t)}`;
  }
</script>

<div
  class="timeline"
  bind:this={bar}
  role="slider"
  tabindex="-1"
  aria-label="Seek"
  aria-valuemin={0}
  aria-valuemax={duration}
  aria-valuenow={time}
  onpointerdown={(e) => down(e, "seek")}
  onpointermove={move}
  onpointerup={() => (dragging = null)}
  onpointerleave={() => (hover = null)}
>
  <svg viewBox="0 0 {W} {H}" preserveAspectRatio="none" class="wave">
    {#each louds as e, i (i)}
      <rect class="loud" x={(pct(e.t) / 100) * W} width={Math.max(2, (pct(e.dur ?? 0.5) / 100) * W)} y="0" height={H} />
    {/each}
    {#if wave}<path d={wave} class="level" />{/if}
    {#each cuts as e, i (i)}
      <line class="cut" x1={(pct(e.t) / 100) * W} x2={(pct(e.t) / 100) * W} y1="0" y2={H} />
    {/each}
  </svg>

  {#if inPoint != null || outPoint != null}
    <div class="shade" style="left:0;width:{pct(snappedStart ?? inPoint ?? 0)}%"></div>
    {#if snappedStart != null && inPoint != null && snappedStart < inPoint}
      <div class="snap" style="left:{pct(snappedStart)}%;width:{pct(inPoint) - pct(snappedStart)}%" title="A lossless cut starts at the keyframe before the in point"></div>
    {/if}
    <div class="shade" style="left:{pct(outPoint ?? duration)}%;right:0"></div>
    <div class="handle in" style="left:{pct(inPoint ?? 0)}%" role="slider" tabindex="-1" aria-label="In point" aria-valuenow={inPoint ?? 0} onpointerdown={(e) => down(e, "in")}></div>
    <div class="handle out" style="left:{pct(outPoint ?? duration)}%" role="slider" tabindex="-1" aria-label="Out point" aria-valuenow={outPoint ?? duration} onpointerdown={(e) => down(e, "out")}></div>
  {/if}

  {#each [...markers, ...others] as e, i (i)}
    <button
      class="pin"
      class:other={e.kind !== "marker"}
      style="left:{pct(e.t)}%"
      title={label(e)}
      aria-label={label(e)}
      onpointerdown={(ev) => ev.stopPropagation()}
      onclick={() => onseek(e.t)}
    ></button>
  {/each}

  <div class="head" style="left:{pct(time)}%"></div>
  {#if hover}<div class="tip" style="left:{hover.x}px">{hover.text}</div>{/if}
</div>

<style>
  .timeline {
    position: relative;
    height: 72px;
    background: #0d0f13;
    border: 1px solid var(--line);
    border-radius: 6px;
    cursor: pointer;
    touch-action: none;
    margin-top: 14px;
  }
  .wave {
    position: absolute;
    left: 0;
    right: 0;
    bottom: 0;
    width: 100%;
    height: 56px;
  }
  .level {
    fill: #3b4252;
  }
  .loud {
    fill: #f97316;
    opacity: 0.22;
  }
  .cut {
    stroke: #8b93a7;
    stroke-width: 1;
    vector-effect: non-scaling-stroke;
    opacity: 0.55;
  }
  .shade {
    position: absolute;
    top: 0;
    bottom: 0;
    background: #000a;
    pointer-events: none;
  }
  .snap {
    position: absolute;
    top: 0;
    bottom: 0;
    background: repeating-linear-gradient(45deg, #4b6bfb33 0 4px, transparent 4px 8px);
    pointer-events: none;
  }
  .handle {
    position: absolute;
    top: -4px;
    bottom: -4px;
    width: 10px;
    margin-left: -5px;
    background: #4b6bfb;
    border-radius: 3px;
    cursor: ew-resize;
    z-index: 3;
  }
  .pin {
    position: absolute;
    top: -7px;
    width: 12px;
    height: 12px;
    margin-left: -6px;
    padding: 0;
    border: none;
    border-radius: 2px;
    background: var(--marker);
    transform: rotate(45deg);
    z-index: 4;
    box-shadow: 0 0 0 2px #0d0f13;
  }
  .pin.other {
    background: #a78bfa;
  }
  .head {
    position: absolute;
    top: 0;
    bottom: 0;
    width: 2px;
    margin-left: -1px;
    background: var(--accent);
    pointer-events: none;
    z-index: 2;
  }
  .tip {
    position: absolute;
    bottom: calc(100% + 8px);
    transform: translateX(-50%);
    background: #2a2f39;
    padding: 2px 6px;
    border-radius: 4px;
    font-size: 12px;
    pointer-events: none;
    white-space: nowrap;
  }
</style>
