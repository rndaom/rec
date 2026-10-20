<script lang="ts">
  import { onMount } from "svelte";
  import { api, type RecdState } from "./lib/api";
  import Library from "./components/Library.svelte";
  import Settings from "./components/Settings.svelte";
  import RecdBadge from "./components/RecdBadge.svelte";

  let view = $state<"library" | "settings">("library");
  let recd = $state<RecdState | null>(null);

  async function poll() {
    try {
      recd = await api.recdState();
    } catch {
      recd = null;
    }
  }

  onMount(() => {
    poll();
    // Cheap: one line over a local pipe. Slower while hidden.
    let timer = setInterval(poll, 3000);
    const vis = () => {
      clearInterval(timer);
      timer = setInterval(poll, document.hidden ? 15000 : 3000);
      if (!document.hidden) poll();
    };
    document.addEventListener("visibilitychange", vis);
    return () => {
      clearInterval(timer);
      document.removeEventListener("visibilitychange", vis);
    };
  });
</script>

<div class="shell">
  <nav>
    <div class="brand"><span class="dot"></span>rec</div>
    <button class="nav" class:active={view === "library"} onclick={() => (view = "library")}>
      <svg viewBox="0 0 24 24"><path d="M4 5h16v14H4zM10 9l5 3-5 3z" /></svg>
      Clips
    </button>
    <button class="nav" class:active={view === "settings"} onclick={() => (view = "settings")}>
      <svg viewBox="0 0 24 24"><path d="M4 7h10M18 7h2M4 17h4M12 17h8M14 4v6M8 14v6" /></svg>
      Settings
    </button>
    <div class="grow"></div>
    <RecdBadge state={recd} onstart={async () => { await api.recdStart(); setTimeout(poll, 800); }} />
  </nav>
  <main>
    {#if view === "library"}
      <Library recording={recd?.status?.state === "recording"} />
    {:else}
      <Settings />
    {/if}
  </main>
</div>

<style>
  .shell { display: flex; height: 100%; }
  nav {
    width: 188px;
    flex: none;
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: 14px 10px;
    background: var(--panel);
    border-right: 1px solid var(--line);
  }
  .brand { display: flex; align-items: center; gap: 9px; font-weight: 700; font-size: 18px; padding: 2px 10px 16px; letter-spacing: 0.5px; }
  .dot { width: 14px; height: 14px; border-radius: 50%; background: var(--accent); box-shadow: 0 0 10px #eb403466; }
  .nav { display: flex; align-items: center; gap: 10px; text-align: left; background: transparent; border: none; padding: 8px 10px; color: var(--dim); }
  .nav svg { width: 18px; height: 18px; fill: none; stroke: currentColor; stroke-width: 1.8; stroke-linecap: round; stroke-linejoin: round; }
  .nav.active { background: var(--panel-2); color: var(--text); }
  main { flex: 1; min-width: 0; display: flex; flex-direction: column; }
</style>
