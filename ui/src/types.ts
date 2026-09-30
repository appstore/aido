// The shapes the aido server answers with — field names follow the
// --json envelope, so scripts and this app read one schema.

export interface ParamSpec {
  name: string;
  kind: 'language' | 'string' | 'number' | 'integer' | 'enum';
  default?: string | number | null;
  min?: number;
  max?: number;
  choices?: string[];
}

export interface Task {
  name: string;
  operation: string;
  summary: string;
  instruction: string;
  profile: string | null;
  input_types: string[] | null;
  required_types: string[];
  output_types: string[];
  requires_material: boolean;
  processor: string;
  per_part: boolean;
  max_inputs: number | null;
  builtin: boolean;
  params: ParamSpec[];
}

export interface RunRow {
  seq: number;
  run_id: string;
  task: string | null;
  status: string;
  failed_parts: number;
  parts_total: number;
  artifacts: number;
  warnings: number;
  created_at: string;
}

export interface Artifact {
  id: string;
  kind: 'text' | 'image' | 'audio';
  mime: string;
  format: string;
  size: number;
  provenance?: { type?: string; index?: number; requests?: number[] };
}

export interface InputSummary {
  name: string;
  kind: string;
  source: string;
  bytes: number;
}

export interface RunSummary {
  task?: string | null;
  profile?: string | null;
  provider?: string | null;
  model?: string | null;
  adapter?: string | null;
  inputs: InputSummary[];
  processor?: string | null;
}

export interface RunReport {
  version: number;
  run_id: string;
  task: string | null;
  created_at: string;
  status: { status: string; reason?: string };
  summary?: RunSummary;
  artifacts: Artifact[];
  warnings: string[];
  failed_parts: { part: string; error: string }[];
  parts_total: number;
  // DeliveryState as serde writes it: a tagged destination and an
  // externally tagged status (UI runs deliver nothing, CLI runs do).
  deliveries?: {
    destination: { type: 'stdout' | 'file' | 'directory' | 'clipboard'; path?: string };
    status: 'pending' | 'succeeded' | { failed: { error: string } };
  }[];
  stages?: RunSummary[];
  last_stage_len?: number;
  error?: { kind: string; message: string };
  // Server-side delivery only: artifact id → absolute saved path (the
  // done frame carries it; history detail does not).
  saved?: Record<string, string>;
}

export interface Preview {
  text: string;
  task: string;
  profile: string;
  model: string;
  adapter: string;
  steps: { label: string; part: number | null; role: string }[];
  destinations: string[];
  credentials_available: boolean | null;
}

export type Frame =
  | { type: 'delta'; text: string }
  | { type: 'step'; done: number; total: number; label: string; part?: string | null }
  | { type: 'warning'; text: string }
  | { type: 'done' }
  | { type: 'error'; kind: string; message: string }
  | { type: 'cancelled'; run_id: string };

// RunReport doubles as the done frame's payload: the frame's serde tag
// is written first, then the report's keys flatten into the same object
// (no report key is named "type", so the tag survives).
export type DoneFrame = Frame & { type: 'done' } & RunReport;

export interface RunRequestPayload {
  task: string;
  prompt?: string;
  profile?: string;
  model?: string;
  to?: string;
  voice?: string;
  speed?: number;
  count?: number;
  size?: string;
  no_split?: boolean;
  produce?: string[];
  format?: string;
  timeout_secs?: number;
  total_timeout_secs?: number;
  // Server-side delivery, the whitelist form: a NAME under aido's
  // deliveries directory, never a free-form path.
  out_dir?: string;
  out_file?: string;
  texts?: string[];
}

// --- tasks (the wizard) ------------------------------------------------------

export interface TaskSource {
  name: string;
  builtin: boolean;
  path: string | null;
  toml: string;
}

export interface TaskSaveResult {
  task: Task;
  path: string;
}

// --- configuration ---------------------------------------------------------

export interface ProfileEntry {
  name: string;
  provider: string | null;
  model: string | null;
  operations: string[] | null;
  input_types: string[] | null;
  output_types: string[] | null;
  is_default: boolean;
}

export interface ProviderEntry {
  name: string;
  base_url: string | null;
  api_key_env: string | null;
  routes: Record<string, string>;
}

export interface ConfigEffective {
  default_profile: string | null;
  profiles: ProfileEntry[];
  providers: ProviderEntry[];
  settings: Record<string, number | boolean | null>;
}

export interface ConfigView {
  path: string | null;
  exists: boolean;
  raw: string;
  effective: ConfigEffective | null;
  load_error: string | null;
  issues: string[];
}

export interface SaveResult {
  ok: boolean;
  path: string;
  issues: string[];
}

export interface ProfilesView {
  default_profile: string | null;
  profiles: { name: string; provider: string | null; model: string | null; is_default: boolean }[];
}

// --- chains -----------------------------------------------------------------

export interface ChainStagePayload {
  task: string;
  prompt?: string;
  profile?: string;
  model?: string;
  to?: string;
  voice?: string;
  speed?: number;
  count?: number;
  size?: string;
  no_split?: boolean;
  produce?: string[];
  format?: string;
  timeout_secs?: number;
  total_timeout_secs?: number;
}

export interface ChainRequestPayload {
  stages: ChainStagePayload[];
  texts?: string[];
}

export interface ChainPreview {
  text: string;
  label: string;
  stages: { name: string; profile: string; model: string; produce: string[] }[];
}

// --- watch daemons -----------------------------------------------------------

export interface WatchRequestPayload {
  dir: string;
  task: string;
  prompt?: string;
  profile?: string;
  model?: string;
  to?: string;
  voice?: string;
  speed?: number;
  count?: number;
  size?: string;
  no_split?: boolean;
  timeout_secs?: number;
  total_timeout_secs?: number;
  out_subdir?: string;
  interval_secs?: number;
  stable_ms?: number;
  include_existing?: boolean;
}

export interface WatchRow {
  id: string;
  dir: string;
  task: string;
  out_dir: string;
  interval_ms: number;
  stable_ms: number;
  include_existing: boolean;
  started_at: string;
  status: 'running' | 'stopping' | 'stopped';
  processed: number;
  failed: number;
  last_file: string | null;
  last_error: string | null;
  stop_reason: string | null;
}

export interface WatchPreview {
  text: string;
  task: string;
  dir: string;
  out_dir: string;
  interval_ms: number;
  stable_ms: number;
}

export type WatchFrame =
  | { type: 'started'; at: string; dir: string; task: string; out_dir: string; interval_ms: number; stable_ms: number }
  | { type: 'file_done'; at: string; file: string; task: string }
  | { type: 'file_failed'; at: string; file: string; task: string; reason: string }
  | { type: 'dir_unreadable'; at: string; dir: string; error: string }
  | { type: 'dir_readable'; at: string; dir: string }
  | { type: 'stopped'; at: string; reason: string | null }
  | { type: 'lagged'; at: string };
