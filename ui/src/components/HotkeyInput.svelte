<script lang="ts">
  // A hotkey field: type it ("Alt+F8") or press "Record" and press the keys.
  // A hotkey recd already owns is taken by Windows before this window sees
  // it, so recording only works for keys rec isn't using.
  let {
    value,
    optional,
    onchange,
  }: { value: string | null; optional: boolean; onchange: (v: string | null) => void } = $props();

  let recording = $state(false);
  let draft = $state<string | null>(null);

  const named: Record<string, string> = {
    Space: "Space", Tab: "Tab", Enter: "Enter", Escape: "Esc", Backspace: "Backspace",
    Insert: "Insert", Delete: "Delete", Home: "Home", End: "End", PageUp: "PageUp",
    PageDown: "PageDown", ArrowUp: "Up", ArrowDown: "Down", ArrowLeft: "Left",
    ArrowRight: "Right", PrintScreen: "PrintScreen", ScrollLock: "ScrollLock", Pause: "Pause",
  };

  function keyName(e: KeyboardEvent): string | null {
    const c = e.code;
    if (/^Key[A-Z]$/.test(c)) return c.slice(3);
    if (/^Digit[0-9]$/.test(c)) return c.slice(5);
    if (/^F([1-9]|1[0-9]|2[0-4])$/.test(c)) return c;
    if (/^Numpad[0-9]$/.test(c)) return "Num" + c.slice(6);
    return named[c] ?? null;
  }

  function onkey(e: KeyboardEvent) {
    if (!recording) return;
    e.preventDefault();
    e.stopPropagation();
    if (e.key === "Escape" && !e.ctrlKey && !e.altKey && !e.shiftKey) {
      recording = false;
      return;
    }
    const key = keyName(e);
    if (!key) return; // a modifier on its own: wait for the key
    const mods = [e.ctrlKey && "Ctrl", e.altKey && "Alt", e.shiftKey && "Shift", e.metaKey && "Win"].filter(Boolean);
    recording = false;
    onchange([...mods, key].join("+"));
  }

  function commit() {
    if (draft == null) return;
    const t = draft.trim();
    draft = null;
    if (t === (value ?? "")) return;
    onchange(t ? t : optional ? null : value);
  }
</script>

<div class="row">
  {#if recording}
    <!-- svelte-ignore a11y_autofocus -->
    <input class="rec" readonly value="Press the keys…" autofocus onkeydown={onkey} onblur={() => (recording = false)} />
  {:else}
    <input placeholder={optional ? "Not set" : ""} value={draft ?? value ?? ""} oninput={(e) => (draft = (e.target as HTMLInputElement).value)} onblur={commit} onkeydown={(e) => e.key === "Enter" && commit()} />
  {/if}
  <button onclick={() => (recording = !recording)}>{recording ? "Cancel" : "Record"}</button>
  {#if optional && value}
    <button class="ghost" title="Clear" onclick={() => onchange(null)}>✕</button>
  {/if}
</div>

<style>
  input {
    width: 150px;
  }
  .rec {
    border-color: var(--accent);
    color: var(--accent-2);
  }
</style>
