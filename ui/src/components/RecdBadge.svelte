<script lang="ts">
  import type { RecdState } from "../lib/api";

  let { state, onstart }: { state: RecdState | null; onstart: () => void } = $props();

  const label = $derived.by(() => {
    if (!state) return "…";
    if (!state.running) return "Not running";
    switch (state.status?.state) {
      case "recording": return "Recording";
      case "paused": return "Paused";
      case "error": return "Problem";
      default: return "Waiting for a game";
    }
  });
  const detail = $derived(
    state?.status?.state === "recording"
      ? `${state.status.target ?? ""} · ${Math.round(state.status.buffered_seconds)} s buffered`
      : (state?.status?.reason ?? state?.error ?? (state && !state.running ? "Clips aren't being recorded." : "")),
  );
</script>

<div class="badge {state?.running ? state.status?.state : 'off'}" title={state?.status?.warning ?? detail}>
  <div class="row"><span class="light"></span><strong>{label}</strong></div>
  {#if detail}<div class="detail">{detail}</div>{/if}
  {#if state?.status?.warning}<div class="detail warn">{state.status.warning}</div>{/if}
  {#if state && !state.running}
    <button class="primary" onclick={onstart}>Start rec</button>
  {/if}
</div>

<style>
  .badge { background: var(--bg); border: 1px solid var(--line); border-radius: var(--radius); padding: 10px; font-size: 12.5px; display: flex; flex-direction: column; gap: 4px; }
  .light { width: 8px; height: 8px; border-radius: 50%; background: var(--faint); }
  .recording .light { background: var(--accent); animation: pulse 1.6s infinite; }
  .idle .light { background: var(--ok); }
  .paused .light, .error .light { background: var(--warn); }
  .detail { color: var(--dim); overflow: hidden; text-overflow: ellipsis; display: -webkit-box; -webkit-line-clamp: 3; line-clamp: 3; -webkit-box-orient: vertical; }
  .warn { color: var(--warn); }
  button { margin-top: 6px; }
  @keyframes pulse { 50% { opacity: 0.35; } }
</style>
