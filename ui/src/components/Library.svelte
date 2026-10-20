<script lang="ts">
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { api, type ClipItem, type Listing } from "../lib/api";
  import { bytes } from "../lib/format";
  import ClipCard from "./ClipCard.svelte";
  import ClipView from "./ClipView.svelte";

  let { recording }: { recording: boolean } = $props();

  let listing = $state<Listing | null>(null);
  let error = $state<string | null>(null);
  let query = $state("");
  let game = $state("");
  let starredOnly = $state(false);
  let selected = $state<Set<string>>(new Set());
  let anchor: string | null = null;
  let openKey = $state<string | null>(null);
  let busy = $state(false);
  let toast = $state<string | null>(null);

  async function refresh() {
    try {
      listing = await api.list();
      error = null;
      const keys = new Set(listing.clips.map((c) => c.key));
      selected = new Set([...selected].filter((k) => keys.has(k)));
    } catch (e) {
      error = String(e);
    }
  }

  function say(text: string) {
    toast = text;
    setTimeout(() => {
      if (toast === text) toast = null;
    }, 2800);
  }

  const games = $derived(
    [...new Set((listing?.clips ?? []).map((c) => c.game).filter(Boolean))].sort(),
  );
  const shown = $derived(
    (listing?.clips ?? []).filter((c) => {
      if (starredOnly && !c.starred) return false;
      if (game && c.game !== game) return false;
      const q = query.trim().toLowerCase();
      return !q || c.game.toLowerCase().includes(q) || c.file.toLowerCase().includes(q);
    }),
  );

  function select(clip: ClipItem, e: MouseEvent) {
    const next = new Set(e.ctrlKey ? selected : []);
    if (e.shiftKey && anchor) {
      const keys = shown.map((c) => c.key);
      const [a, b] = [keys.indexOf(anchor), keys.indexOf(clip.key)].sort((x, y) => x - y);
      if (a >= 0) keys.slice(a, b + 1).forEach((k) => next.add(k));
    } else if (e.ctrlKey && selected.has(clip.key)) {
      next.delete(clip.key);
    } else {
      next.add(clip.key);
      anchor = clip.key;
    }
    selected = next;
  }

  async function act(what: () => Promise<unknown>, done?: string) {
    busy = true;
    try {
      await what();
      if (done) say(done);
    } catch (e) {
      say(String(e));
    } finally {
      busy = false;
      await refresh();
    }
  }

  const selectedClips = $derived(shown.filter((c) => selected.has(c.key)));
  const allStarred = $derived(selectedClips.length > 0 && selectedClips.every((c) => c.starred));

  function starSelected() {
    const to = !allStarred;
    act(
      () => Promise.all(selectedClips.map((c) => api.star(c.key, to))),
      to ? "Starred: the storage limit never deletes these" : "Unstarred",
    );
  }

  function deleteSelected() {
    const keys = [...selected];
    const n = keys.length;
    selected = new Set();
    act(() => api.remove(keys), `Moved ${n} clip${n === 1 ? "" : "s"} to the Recycle Bin`);
  }

  function onkey(e: KeyboardEvent) {
    if (openKey || (e.target as HTMLElement).closest("input, textarea, select")) return;
    if (e.key === "Delete" && selected.size) deleteSelected();
    else if (e.key === "a" && e.ctrlKey) {
      selected = new Set(shown.map((c) => c.key));
      e.preventDefault();
    } else if (e.key === "Escape") selected = new Set();
    else if (e.key === "Enter" && selected.size === 1) openKey = [...selected][0];
  }

  onMount(() => {
    refresh();
    const unlisten = listen<string>("clip-updated", () => refresh());
    const unfocus = getCurrentWindow().onFocusChanged(({ payload }) => {
      if (payload) refresh();
    });
    // New clips appear while the window is open.
    const timer = setInterval(() => {
      if (!document.hidden && !openKey) refresh();
    }, 10000);
    return () => {
      unlisten.then((f) => f());
      unfocus.then((f) => f());
      clearInterval(timer);
    };
  });
</script>

<svelte:window onkeydown={onkey} />

{#if openKey}
  <ClipView
    key={openKey}
    {recording}
    onclose={() => {
      openKey = null;
      refresh();
    }}
    onopen={(k) => {
      openKey = k;
      refresh();
    }}
    onsay={say}
  />
{:else}
  <header>
    <h1>Clips</h1>
    <input class="search" placeholder="Search" bind:value={query} />
    <select bind:value={game}>
      <option value="">All games</option>
      {#each games as g (g)}<option value={g}>{g}</option>{/each}
    </select>
    <label class="row toggle"><input type="checkbox" bind:checked={starredOnly} />Starred</label>
    <div class="grow"></div>
    <button class="ghost" title="Refresh" onclick={refresh}>↻</button>
    {#if listing}<button onclick={() => api.open(listing!.folder)}>Open folder</button>{/if}
  </header>

  {#if selected.size}
    <div class="selbar">
      <span>{selected.size} selected</span>
      <button disabled={busy} onclick={starSelected}>{allStarred ? "Unstar" : "Star"}</button>
      <button
        disabled={busy}
        onclick={() =>
          act(() => api.copy([...selected]), "Copied: paste into Explorer, Discord or a chat")}
        >Copy file{selected.size === 1 ? "" : "s"}</button
      >
      <button class="danger" disabled={busy} onclick={deleteSelected}>Delete</button>
      <div class="grow"></div>
      <button class="ghost" onclick={() => (selected = new Set())}>Clear</button>
    </div>
  {/if}

  <section class="grid-wrap">
    {#if error}
      <p class="error">{error}</p>
    {:else if !listing}
      <p class="dim">Loading…</p>
    {:else if listing.clips.length === 0}
      <div class="empty">
        <h2>No clips yet</h2>
        <p class="dim">
          Press the save hotkey (<kbd>Alt</kbd>+<kbd>F8</kbd> by default) in a game to keep the last
          minute.<br />Clips are saved to <code>{listing.folder}</code>
        </p>
      </div>
    {:else}
      <div class="grid">
        {#each shown as clip (clip.key)}
          <ClipCard
            {clip}
            selected={selected.has(clip.key)}
            onselect={(e) => select(clip, e)}
            onopen={() => (openKey = clip.key)}
            onstar={() => act(() => api.star(clip.key, !clip.starred))}
          />
        {/each}
      </div>
      {#if shown.length === 0}<p class="dim">No clips match.</p>{/if}
      {#if listing.problems.length}
        <details class="problems">
          <summary>{listing.problems.length} clip file(s) couldn't be read</summary>
          {#each listing.problems as p (p)}<div class="faint">{p}</div>{/each}
        </details>
      {/if}
    {/if}
  </section>
  {#if listing}
    <footer class="faint">
      {shown.length} of {listing.clips.length} clips · {bytes(listing.total_bytes)} · {listing.folder}
    </footer>
  {/if}
{/if}

{#if toast}<div class="toast">{toast}</div>{/if}

<style>
  header {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 14px 20px;
    border-bottom: 1px solid var(--line);
  }
  h1 {
    font-size: 18px;
    margin: 0 8px 0 0;
    font-weight: 600;
  }
  .search {
    width: 220px;
  }
  .toggle {
    gap: 6px;
    color: var(--dim);
  }
  .selbar {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 20px;
    background: #1e2330;
    border-bottom: 1px solid var(--line);
  }
  .grid-wrap {
    flex: 1;
    overflow-y: auto;
    padding: 18px 20px;
  }
  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(240px, 1fr));
    gap: 16px;
  }
  .empty {
    text-align: center;
    margin-top: 15vh;
  }
  .empty h2 {
    font-weight: 600;
  }
  kbd {
    background: var(--panel-2);
    border: 1px solid var(--line);
    border-radius: 4px;
    padding: 1px 6px;
    font-size: 12px;
  }
  code {
    color: var(--dim);
    user-select: text;
  }
  footer {
    padding: 6px 20px;
    border-top: 1px solid var(--line);
    font-size: 12px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .problems {
    margin-top: 20px;
    font-size: 12px;
  }
  .toast {
    position: fixed;
    bottom: 36px;
    left: 50%;
    transform: translateX(-50%);
    background: #2a2f39;
    border: 1px solid var(--line);
    padding: 8px 16px;
    border-radius: 8px;
    box-shadow: 0 8px 24px #0008;
    max-width: 70vw;
    z-index: 50;
  }
</style>
