<script lang="ts">
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import { api, type Detail, type ExportDone, type ExportPlan } from "../lib/api";
  import { dragOut } from "../lib/drag";
  import { precise } from "../lib/format";

  let {
    clip,
    recording,
    start,
    end,
    onclose,
    onsay,
  }: {
    clip: Detail;
    recording: boolean;
    start: number;
    end: number | null;
    onclose: () => void;
    onsay: (text: string) => void;
  } = $props();

  // Discord's upload limits: 10 MB free, 50 MB with Nitro Basic, 500 MB Nitro.
  const presets = [
    { mb: 10, label: "10 MB", note: "Discord" },
    { mb: 50, label: "50 MB", note: "Nitro Basic" },
    { mb: 500, label: "500 MB", note: "Nitro" },
  ];
  let limit = $state(10);
  let custom = $state(25);
  let useCustom = $state(false);
  let running = $state(false);
  let fraction = $state(0);
  let done = $state<ExportDone | null>(null);
  let error = $state<string | null>(null);

  const span = $derived((end ?? clip.duration) - start);
  const target = $derived(useCustom ? custom : limit);
  let plan = $state<ExportPlan | null>(null);
  // Upload limits are decimal megabytes.
  const mb = (n: number) => `${(n / 1e6).toFixed(n < 1e7 ? 2 : 1)} MB`;

  // What the encoder will make at this size.
  $effect(() => {
    const request = { key: clip.key, limit_mb: target, start, end };
    api.exportPlan(request).then((p) => (plan = p)).catch(() => (plan = null));
  });

  async function go() {
    error = null;
    done = null;
    fraction = 0;
    running = true;
    try {
      await api.exportStart({ key: clip.key, limit_mb: target, start, end });
    } catch (e) {
      running = false;
      error = String(e);
    }
  }

  onMount(() => {
    const subs = [
      listen<{ fraction: number }>("export-progress", (e) => (fraction = e.payload.fraction)),
      listen<ExportDone>("export-done", (e) => {
        running = false;
        done = e.payload;
      }),
      listen<string>("export-failed", (e) => {
        running = false;
        error = e.payload;
      }),
    ];
    return () => subs.forEach((s) => s.then((f) => f()));
  });

  async function cancel() {
    if (running) await api.exportCancel().catch(() => {});
    onclose();
  }
</script>

<div class="backdrop" role="presentation" onclick={(e) => e.target === e.currentTarget && !running && onclose()}>
  <div class="dialog" role="dialog" aria-label="Export for Discord">
    <h2>Export for Discord</h2>
    <p class="dim">
      Makes an H.264 MP4 that plays everywhere and fits under a size limit.
      {#if end != null}It covers the trim range, {precise(start)} to {precise(end)}.{:else}It covers the whole clip.{/if}
    </p>

    {#if !done}
      <div class="presets">
        {#each presets as p (p.mb)}
          <button class:active={!useCustom && limit === p.mb} disabled={running} onclick={() => { useCustom = false; limit = p.mb; }}>
            <strong>{p.label}</strong><span class="faint">{p.note}</span>
          </button>
        {/each}
        <button class:active={useCustom} disabled={running} onclick={() => (useCustom = true)}>
          <strong>
            {#if useCustom}<input type="number" min="1" max="4000" bind:value={custom} onclick={(e) => e.stopPropagation()} /> MB{:else}Custom{/if}
          </strong><span class="faint">size</span>
        </button>
      </div>
      {#if plan}
        <p class="faint small">
          {plan.height}p{plan.fps} at {plan.video_kbps >= 1000 ? `${(plan.video_kbps / 1000).toFixed(1)} Mbit/s` : `${plan.video_kbps} kbit/s`}
          for {precise(span)}, about {mb(plan.estimated_bytes)}.
          {#if !plan.fits}<span class="warn">Too long for this size to look good: trim it first or pick a bigger size.</span>
          {:else if plan.height < 720}A shorter clip would look sharper at this size.{/if}
        </p>
        {#if plan.hdr}
          <p class="warn small">This clip is HDR. The export is SDR and Windows' conversion makes it look darker and flatter than the clip.</p>
        {/if}
      {/if}
      {#if recording}
        <p class="warn small">A game is being recorded. Exporting uses the GPU's video engine and can cost the game frames; it's lighter to export after playing.</p>
      {/if}
      {#if running}
        <div class="progress"><div style="width:{Math.round(fraction * 100)}%"></div></div>
      {/if}
      {#if error}<p class="error small">{error}</p>{/if}
      <div class="row actions">
        <div class="grow"></div>
        <button onclick={cancel}>{running ? "Cancel" : "Close"}</button>
        <button class="primary" disabled={running || target <= 0} onclick={go}>{running ? `Exporting… ${Math.round(fraction * 100)}%` : "Export"}</button>
      </div>
    {:else}
      <div class="done">
        <p><strong>{mb(done.bytes)}</strong> · {done.height}p{done.fps} H.264 · saved next to the clip</p>
        <code>{done.path}</code>
      </div>
      <div class="row actions">
        <button onclick={() => api.copy([done!.path]).then(() => onsay("Copied: paste into Discord")).catch((e) => onsay(String(e)))}>Copy file</button>
        <button draggable="true" ondragstart={(e) => dragOut(e, [done!.path], clip.thumb)}>Drag out</button>
        <button class="ghost" onclick={() => api.reveal(done!.path)}>Show in folder</button>
        <div class="grow"></div>
        <button class="primary" onclick={onclose}>Done</button>
      </div>
    {/if}
  </div>
</div>

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    background: #0009;
    display: grid;
    place-items: center;
    z-index: 40;
  }
  .dialog {
    width: min(560px, 92vw);
    background: var(--panel);
    border: 1px solid var(--line);
    border-radius: 12px;
    padding: 20px 22px;
    box-shadow: 0 20px 60px #000a;
  }
  h2 {
    margin: 0 0 6px;
    font-size: 17px;
    font-weight: 600;
  }
  .presets {
    display: grid;
    grid-template-columns: repeat(4, 1fr);
    gap: 8px;
    margin: 14px 0 8px;
  }
  .presets button {
    display: flex;
    flex-direction: column;
    align-items: center;
    padding: 10px 6px;
  }
  .presets button.active {
    border-color: var(--accent);
    background: #2a1a1a;
  }
  .presets input {
    width: 56px;
    padding: 1px 4px;
  }
  .small {
    font-size: 12.5px;
  }
  .warn {
    color: var(--warn);
  }
  .progress {
    height: 6px;
    background: var(--bg);
    border-radius: 3px;
    overflow: hidden;
    margin: 12px 0;
  }
  .progress div {
    height: 100%;
    background: var(--accent);
    transition: width 0.2s;
  }
  .actions {
    margin-top: 16px;
  }
  .done code {
    font-size: 12px;
    color: var(--dim);
    user-select: text;
    word-break: break-all;
  }
</style>
