// Dragging clips out of the window as files (into Explorer, Discord, a
// browser upload box). WebView2 can't drag local files out by itself, so
// tauri-plugin-drag starts a native drag.
import { startDrag } from "@crabnebula/tauri-plugin-drag";
import { api } from "./api";

let fallbackIcon: string | null = null;
api.info().then((i) => (fallbackIcon = i.drag_icon)).catch(() => {});

export function dragOut(e: DragEvent, paths: string[], icon: string | null) {
  e.preventDefault();
  const preview = icon ?? fallbackIcon;
  if (!preview) return;
  startDrag({ item: paths, icon: preview, mode: "copy" }).catch(() => {});
}
