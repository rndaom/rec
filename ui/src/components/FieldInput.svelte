<script lang="ts">
  import { open as pickFolder } from "@tauri-apps/plugin-dialog";
  import type { Field } from "../lib/api";
  import HotkeyInput from "./HotkeyInput.svelte";

  let {
    field,
    value,
    inherited,
    overridden,
    perGame,
    onchange,
  }: {
    field: Field;
    value: unknown;
    /** In a profile: the "All games" value it would follow. */
    inherited: unknown;
    overridden: boolean;
    perGame: boolean;
    /** `null` resets: to the default, or to following "All games". */
    onchange: (value: unknown) => void;
  } = $props();

  const locked = $derived(perGame && !overridden);
  // What is typed but not saved yet; saved on blur or Enter.
  let draft = $state<string | null>(null);
  const text = $derived(draft ?? asText(value));

  function asText(v: unknown): string {
    if (v == null) return "";
    if (Array.isArray(v)) return v.join("\n");
    return String(v);
  }

  function commitText() {
    if (draft == null) return;
    const raw = draft;
    draft = null;
    if (raw === asText(value)) return;
    if (field.kind === "number") {
      const t = raw.trim();
      if (!t) return onchange(field.optional ? 0 : null);
      const n = Number(t);
      if (Number.isFinite(n)) onchange(Math.round(n));
    } else if (field.kind === "list") {
      onchange(raw.split(/\r?\n/).map((s) => s.trim()).filter(Boolean));
    } else if (field.kind === "text" || field.kind === "folder" || field.kind === "combo") {
      const t = raw.trim();
      onchange(t ? t : field.kind === "text" && !field.optional ? null : "");
    }
  }

  async function browse() {
    const dir = await pickFolder({ directory: true, defaultPath: (value as string) || undefined });
    if (typeof dir === "string") onchange(dir);
  }

  function shown(v: unknown) {
    if (v == null || v === "" || v === 0) return "not set";
    if (typeof v === "boolean") return v ? "on" : "off";
    if (Array.isArray(v)) return v.length ? v.join(", ") : "none";
    if (field.kind === "choice") return field.options.find(([k]) => k === v)?.[1] ?? String(v);
    return String(v) + (field.kind === "number" && field.unit ? ` ${field.unit}` : "");
  }
</script>

<div class="field" class:locked>
  <div class="label">
    <div class="name">{field.label}</div>
    {#if field.help}<div class="help faint">{field.help}</div>{/if}
    {#if perGame}
      <div class="inherit faint">
        {#if overridden}
          All games: {shown(inherited)} ·
          <button class="link" onclick={() => onchange(null)}>Use the All games setting</button>
        {:else}
          Follows All games ·
          <button class="link" onclick={() => onchange(value ?? inherited ?? null)}>Set for this game</button>
        {/if}
      </div>
    {/if}
  </div>
  <div class="control">
    {#if field.kind === "bool"}
      <label class="switch">
        <input type="checkbox" checked={!!value} disabled={locked} onchange={(e) => onchange((e.target as HTMLInputElement).checked)} />
        <span></span>
      </label>
    {:else if field.kind === "choice"}
      <select value={value as string} disabled={locked} onchange={(e) => onchange((e.target as HTMLSelectElement).value)}>
        {#each field.options as [k, label] (k)}<option value={k}>{label}</option>{/each}
      </select>
    {:else if field.kind === "number"}
      <div class="row">
        <input
          type="number"
          min={field.min}
          max={field.max}
          step={field.step}
          placeholder={field.optional ? "Not set" : ""}
          disabled={locked}
          value={text}
          oninput={(e) => (draft = (e.target as HTMLInputElement).value)}
          onblur={commitText}
          onkeydown={(e) => e.key === "Enter" && commitText()}
        />
        <span class="faint">{field.unit}</span>
      </div>
    {:else if field.kind === "list"}
      <textarea rows="3" disabled={locked} value={text}
          oninput={(e) => (draft = (e.target as HTMLInputElement).value)} onblur={commitText}></textarea>
    {:else if field.kind === "folder"}
      <div class="row">
        <input class="wide" placeholder="Videos\rec" disabled={locked} value={text}
          oninput={(e) => (draft = (e.target as HTMLInputElement).value)} onblur={commitText} onkeydown={(e) => e.key === "Enter" && commitText()} />
        <button disabled={locked} onclick={browse}>Browse…</button>
      </div>
    {:else if field.kind === "hotkey"}
      <HotkeyInput value={(value as string) ?? null} optional={field.optional} {onchange} />
    {:else if field.kind === "combo"}
      <input placeholder="e.g. LS+RS" disabled={locked} value={text}
          oninput={(e) => (draft = (e.target as HTMLInputElement).value)} onblur={commitText} onkeydown={(e) => e.key === "Enter" && commitText()} />
    {:else if field.kind === "extra_clips"}
      {@const clips = (value as { hotkey: string; seconds: number }[]) ?? []}
      <div class="extras">
        {#each clips as c, i (i)}
          <div class="row">
            <HotkeyInput value={c.hotkey} optional={false} onchange={(h) => onchange(clips.map((x, j) => (j === i ? { ...x, hotkey: h } : x)))} />
            <input type="number" min="5" max="1200" value={c.seconds} onchange={(e) => onchange(clips.map((x, j) => (j === i ? { ...x, seconds: Number((e.target as HTMLInputElement).value) } : x)))} />
            <span class="faint">s</span>
            <button class="ghost" title="Remove" onclick={() => onchange(clips.filter((_, j) => j !== i))}>✕</button>
          </div>
        {/each}
        <button onclick={() => onchange([...clips, { hotkey: "Alt+F9", seconds: 15 }])}>Add a length</button>
      </div>
    {:else}
      <input
        class="wide"
        placeholder={field.kind === "text" ? field.placeholder : ""}
        disabled={locked}
        value={text}
          oninput={(e) => (draft = (e.target as HTMLInputElement).value)}
        onblur={commitText}
        onkeydown={(e) => e.key === "Enter" && commitText()}
      />
    {/if}
  </div>
</div>

<style>
  .field {
    display: grid;
    grid-template-columns: 1fr 300px;
    gap: 20px;
    padding: 14px 0;
    border-bottom: 1px solid #22262e;
    align-items: start;
  }
  .name {
    font-weight: 500;
  }
  .help,
  .inherit {
    font-size: 12.5px;
    margin-top: 2px;
  }
  .locked .control {
    opacity: 0.5;
  }
  .control input[type="number"] {
    width: 120px;
  }
  .control select,
  .control textarea,
  .wide {
    width: 100%;
  }
  .row .wide {
    flex: 1;
    min-width: 0;
  }
  .link {
    background: none;
    border: none;
    padding: 0;
    color: #8ea2ff;
    font-size: inherit;
  }
  .switch {
    position: relative;
    display: inline-block;
    width: 40px;
    height: 22px;
  }
  .switch input {
    opacity: 0;
    width: 0;
    height: 0;
  }
  .switch span {
    position: absolute;
    inset: 0;
    background: #333a46;
    border-radius: 11px;
    transition: background 0.15s;
    cursor: pointer;
  }
  .switch span::after {
    content: "";
    position: absolute;
    left: 3px;
    top: 3px;
    width: 16px;
    height: 16px;
    border-radius: 50%;
    background: #fff;
    transition: transform 0.15s;
  }
  .switch input:checked + span {
    background: var(--accent);
  }
  .switch input:checked + span::after {
    transform: translateX(18px);
  }
  .switch input:focus-visible + span {
    outline: 2px solid #4b6bfb;
  }
  .extras {
    display: flex;
    flex-direction: column;
    gap: 6px;
    align-items: flex-start;
  }
  .extras input[type="number"] {
    width: 70px;
  }
</style>
