// Typed wrappers over rec-ui's Tauri commands (crates/rec-ui/src/main.rs).
import { invoke, convertFileSrc } from "@tauri-apps/api/core";

export interface Status {
  state: "idle" | "recording" | "paused" | "error";
  reason: string | null;
  target: string | null;
  encoder: string | null;
  buffered_seconds: number;
  buffer_bytes: number;
  warning?: string | null;
  version: string;
}

export interface RecdState {
  running: boolean;
  status: Status | null;
  error: string | null;
}

export interface Info {
  version: string;
  config_path: string;
  clips_dir: string;
  drag_icon: string | null;
}

export type FieldKind =
  | { kind: "bool" }
  | { kind: "number"; min: number; max: number; step: number; unit: string; optional: boolean }
  | { kind: "choice"; options: [string, string][] }
  | { kind: "text"; optional: boolean; placeholder: string }
  | { kind: "folder" }
  | { kind: "hotkey"; optional: boolean }
  | { kind: "combo" }
  | { kind: "list" }
  | { kind: "extra_clips" };

export type Field = FieldKind & {
  key: string;
  section: string;
  label: string;
  help: string;
  per_game: boolean;
};

export interface ProfileInfo {
  id: string;
  name: string;
  exe: string[];
  path: string;
  count: number;
}

export interface ProfileView {
  id: string;
  name: string;
  path: string;
  overrides: Record<string, unknown>;
  effective: Record<string, Record<string, unknown>>;
  ignored: string[];
}

export interface Snapshot {
  config_path: string;
  profiles_dir: string;
  values: Record<string, Record<string, unknown>>;
  profiles: ProfileInfo[];
  profile: ProfileView | null;
  problems: string[];
  warnings: string[];
  schema: Field[];
}

export interface ClipItem {
  key: string;
  id: string;
  game: string;
  file: string;
  folder: string;
  thumb: string | null;
  duration: number;
  start_utc: number | null;
  modified: number | null;
  bytes: number;
  starred: boolean;
  codec: string | null;
  width: number | null;
  height: number | null;
  fps: number | null;
  hdr: boolean;
  markers: number;
  analyzed: boolean;
  has_sidecar: boolean;
  trimmed_from: string | null;
}

export interface Listing {
  folder: string;
  clips: ClipItem[];
  problems: string[];
  total_bytes: number;
}

export interface ClipEvent {
  t: number;
  kind: string;
  src: string;
  conf?: number;
  dur?: number;
  [k: string]: unknown;
}

export interface Track {
  index: number;
  name: string;
  role: string;
  loudness: number[];
}

export interface Detail extends ClipItem {
  video: string;
  sidecar: string | null;
  trigger: string | null;
  saved_at: number | null;
  profile: string | null;
  events: ClipEvent[];
  tracks: Track[];
  hz: number;
  analysis: Record<string, unknown> | null;
}

export interface TrimPlan {
  start: number;
  end: number;
}

export interface Trimmed {
  key: string;
  start: number;
  end: number;
}

export interface ExportRequest {
  key: string;
  limit_mb: number;
  start: number;
  end: number | null;
}

export interface ExportPlan {
  width: number;
  height: number;
  fps: number;
  video_kbps: number;
  audio_kbps: number;
  estimated_bytes: number;
  fits: boolean;
  hdr: boolean;
}

export interface ExportDone {
  path: string;
  bytes: number;
  width: number;
  height: number;
  fps: number;
  from_hdr: boolean;
}

export const api = {
  info: () => invoke<Info>("app_info"),
  recdState: () => invoke<RecdState>("recd_state"),
  recdStart: () => invoke<void>("recd_start"),
  settings: (profile: string | null) => invoke<Snapshot>("settings_get", { profile }),
  applySettings: (profile: string | null, changes: { key: string; value: unknown }[]) =>
    invoke<{ reload: string | null }>("settings_apply", { profile, changes }),
  list: () => invoke<Listing>("library_list"),
  detail: (key: string) => invoke<Detail>("clip_detail", { key }),
  star: (key: string, starred: boolean) => invoke<void>("clip_star", { key, starred }),
  remove: (keys: string[]) => invoke<void>("clip_delete", { keys }),
  copy: (keys: string[]) => invoke<void>("clip_copy", { keys }),
  reveal: (path: string) => invoke<void>("reveal", { path }),
  open: (path: string) => invoke<void>("open_path", { path }),
  trimPlan: (key: string, start: number, end: number) =>
    invoke<TrimPlan>("trim_plan", { key, start, end }),
  trim: (key: string, start: number, end: number) => invoke<Trimmed>("trim", { key, start, end }),
  exportPlan: (request: ExportRequest) => invoke<ExportPlan>("export_plan", { request }),
  exportStart: (request: ExportRequest) => invoke<void>("export_start", { request }),
  exportCancel: () => invoke<void>("export_cancel"),
};

/** A local file as a URL the webview may load (asset protocol). */
export const fileUrl = (path: string) => convertFileSrc(path);
