<script lang="ts">
  import { onMount } from "svelte";
  import { api, type Field, type Snapshot } from "../lib/api";
  import FieldInput from "./FieldInput.svelte";

  let snap = $state<Snapshot | null>(null);
  let profile = $state<string | null>(null);
  let error = $state<string | null>(null);
  let status = $state<{ text: string; bad?: boolean } | null>(null);
  let section = $state("Replay");

  async function load() {
    try {
      snap = await api.settings(profile);
      error = null;
    } catch (e) {
      error = String(e);
    }
  }

  const sections = $derived.by(() => {
    const fields = (snap?.schema ?? []).filter((f) => !profile || f.per_game);
    const out: [string, Field[]][] = [];
    for (const f of fields) {
      const last = out[out.length - 1];
      if (last && last[0] === f.section) last[1].push(f);
      else out.push([f.section, [f]]);
    }
    return out;
  });

  $effect(() => {
    if (sections.length && !sections.some(([s]) => s === section)) section = sections[0][0];
  });

  function get(values: Record<string, Record<string, unknown>> | undefined, key: string) {
    const [s, k] = key.split(".");
    return values?.[s]?.[k] ?? null;
  }

  async function save(key: string, value: unknown) {
    status = { text: "Saving…" };
    try {
      const saved = await api.applySettings(profile, [{ key, value }]);
      status = saved.reload
        ? { text: `Saved, but rec couldn't apply it: ${saved.reload}`, bad: true }
        : { text: "Saved" };
    } catch (e) {
      status = { text: String(e), bad: true };
    }
    await load();
    setTimeout(() => {
      if (status?.text === "Saved") status = null;
    }, 1800);
  }

  function pickProfile(id: string) {
    profile = id || null;
    load();
  }

  onMount(load);
</script>

<header>
  <h1>Settings</h1>
  {#if snap}
    <label class="row for">
      <span class="dim">For</span>
      <select value={profile ?? ""} onchange={(e) => pickProfile((e.target as HTMLSelectElement).value)}>
        <option value="">All games</option>
        {#each snap.profiles as p (p.id)}
          <option value={p.id}>{p.name}{p.count ? ` (${p.count} set)` : ""}</option>
        {/each}
      </select>
    </label>
  {/if}
  <div class="grow"></div>
  {#if status}<span class:error={status.bad} class:dim={!status.bad}>{status.text}</span>{/if}
</header>

{#if error}
  <p class="error pad">{error}</p>
{:else if snap}
  <div class="body">
    <aside>
      {#each sections as [name] (name)}
        <button class="sec" class:active={section === name} onclick={() => (section = name)}>{name}</button>
      {/each}
      <div class="grow"></div>
      <div class="files faint">
        <button
          class="link"
          onclick={() =>
            api.open(snap!.profile?.path ?? snap!.config_path).catch((e) => (status = { text: String(e), bad: true }))}
          >Open the file</button
        >
        <span>Comments and layout in the file are kept.</span>
      </div>
    </aside>
    <section class="fields">
      {#if snap.problems.length}
        <div class="problems">
          <strong>Can't read these files; fix them in a text editor first.</strong>
          {#each snap.problems as p (p)}<div>{p}</div>{/each}
        </div>
      {/if}
      {#if snap.warnings.length && !profile}
        <div class="warnings">{#each snap.warnings as w (w)}<div>⚠ {w}</div>{/each}</div>
      {/if}
      {#if snap.profile}
        <p class="dim intro">
          Settings for <strong>{snap.profile.name}</strong> only. Anything not overridden follows "All games".
          Profiles are made when rec first records a game; hotkeys, folders and what counts as a game are the same for every game.
        </p>
      {/if}
      {#each sections.filter(([s]) => s === section) as [, fields] (section)}
        {#each fields as field (field.key)}
          {@const overridden = !!snap.profile && field.key in snap.profile.overrides}
          <FieldInput
            {field}
            value={snap.profile ? get(snap.profile.effective, field.key) : get(snap.values, field.key)}
            inherited={snap.profile ? get(snap.values, field.key) : undefined}
            {overridden}
            perGame={!!snap.profile}
            onchange={(v) => save(field.key, v)}
          />
        {/each}
      {/each}
    </section>
  </div>
{/if}

<style>
  header {
    display: flex;
    align-items: center;
    gap: 14px;
    padding: 14px 20px;
    border-bottom: 1px solid var(--line);
  }
  h1 {
    font-size: 18px;
    margin: 0;
    font-weight: 600;
  }
  .for {
    gap: 8px;
  }
  .pad {
    padding: 20px;
  }
  .body {
    flex: 1;
    display: flex;
    min-height: 0;
  }
  aside {
    width: 170px;
    padding: 14px 10px;
    border-right: 1px solid var(--line);
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .sec {
    text-align: left;
    background: transparent;
    border: none;
    color: var(--dim);
  }
  .sec.active {
    background: var(--panel-2);
    color: var(--text);
  }
  .files {
    font-size: 12px;
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 0 6px;
  }
  .link {
    background: none;
    border: none;
    padding: 0;
    color: #8ea2ff;
    text-align: left;
  }
  .fields {
    flex: 1;
    overflow-y: auto;
    padding: 8px 28px 40px;
    max-width: 860px;
  }
  .problems,
  .warnings {
    margin: 14px 0;
    padding: 10px 12px;
    border-radius: 8px;
    font-size: 13px;
  }
  .problems {
    background: #3a1717;
    border: 1px solid #6b2626;
  }
  .warnings {
    background: #3a2f12;
    border: 1px solid #6b5520;
  }
  .intro {
    font-size: 13px;
    margin: 14px 0 4px;
  }
</style>
