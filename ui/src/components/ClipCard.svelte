<script lang="ts">
  import { dragOut } from "../lib/drag";
  import { fileUrl, type ClipItem } from "../lib/api";
  import { bytes, duration, resolution, when } from "../lib/format";

  let {
    clip,
    selected,
    onselect,
    onopen,
    onstar,
  }: {
    clip: ClipItem;
    selected: boolean;
    onselect: (e: MouseEvent) => void;
    onopen: () => void;
    onstar: () => void;
  } = $props();
</script>

<div
  class="card"
  class:selected
  role="button"
  tabindex="0"
  draggable="true"
  ondragstart={(e) => dragOut(e, [clip.key], clip.thumb)}
  onclick={onselect}
  ondblclick={onopen}
  onkeydown={(e) => e.key === " " && onopen()}
>
  <div class="thumb">
    {#if clip.thumb}
      <img src={fileUrl(clip.thumb)} alt="" loading="lazy" draggable="false" />
    {:else}
      <div class="placeholder"><span></span></div>
    {/if}
    <span class="dur">{duration(clip.duration)}</span>
    {#if clip.hdr}<span class="tag">HDR</span>{/if}
    <button
      class="star"
      class:on={clip.starred}
      title={clip.starred ? "Starred: never deleted by the storage limit" : "Star"}
      onclick={(e) => {
        e.stopPropagation();
        onstar();
      }}>★</button
    >
    <button
      class="play"
      title="Open"
      aria-label="Open"
      onclick={(e) => {
        e.stopPropagation();
        onopen();
      }}>▶</button
    >
  </div>
  <div class="meta">
    <div class="title">{clip.game || clip.file}</div>
    <div class="sub faint">
      {when(clip.start_utc ?? clip.modified)} · {bytes(clip.bytes)}
      {#if clip.markers}
        · <span class="mk">◆ {clip.markers}</span>{/if}
      {#if resolution(clip.width, clip.height, clip.fps)}
        · {resolution(clip.width, clip.height, clip.fps)}{/if}
    </div>
  </div>
</div>

<style>
  .card {
    border-radius: var(--radius);
    background: var(--panel);
    border: 1px solid var(--line);
    overflow: hidden;
    cursor: default;
    transition: border-color 0.1s;
  }
  .card:hover {
    border-color: #3a404c;
  }
  .card.selected {
    border-color: #4b6bfb;
    box-shadow: 0 0 0 1px #4b6bfb;
  }
  .thumb {
    position: relative;
    aspect-ratio: 16 / 9;
    background: #0c0d10;
  }
  img {
    width: 100%;
    height: 100%;
    object-fit: cover;
    display: block;
  }
  .placeholder {
    width: 100%;
    height: 100%;
    display: grid;
    place-items: center;
  }
  .placeholder span {
    width: 22px;
    height: 22px;
    border-radius: 50%;
    background: #2b303a;
    animation: pulse 1.6s infinite;
  }
  .dur,
  .tag {
    position: absolute;
    bottom: 6px;
    font-size: 12px;
    background: #000b;
    padding: 1px 6px;
    border-radius: 4px;
  }
  .dur {
    right: 6px;
  }
  .tag {
    left: 6px;
    color: var(--warn);
  }
  .star {
    position: absolute;
    top: 4px;
    right: 4px;
    background: #0009;
    border: none;
    color: #fff8;
    font-size: 16px;
    padding: 2px 7px;
    opacity: 0;
  }
  .star.on {
    color: var(--star);
    opacity: 1;
  }
  .play {
    position: absolute;
    inset: 0;
    margin: auto;
    width: 46px;
    height: 46px;
    border-radius: 50%;
    background: #000a;
    border: none;
    opacity: 0;
    font-size: 16px;
  }
  .card:hover .star,
  .card:hover .play {
    opacity: 1;
  }
  .meta {
    padding: 8px 10px 10px;
  }
  .title {
    font-weight: 600;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .sub {
    font-size: 12px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .mk {
    color: var(--marker);
  }
  @keyframes pulse {
    50% {
      opacity: 0.4;
    }
  }
</style>
