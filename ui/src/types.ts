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
  deliveries?: { destination: string; status: string }[];
  stages?: RunSummary[];
  last_stage_len?: number;
  error?: { kind: string; message: string };
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
  | { type: 'step'; done: number; total: number; label: string }
  | { type: 'warning'; text: string }
  | { type: 'done' }
  | { type: 'error'; kind: string; message: string }
  | { type: 'cancelled'; run_id: string };

// RunReport doubles as the done frame's payload (the frame flattens it,
// so its own "type" key is overwritten by the frame tag).
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
  timeout_secs?: number;
  total_timeout_secs?: number;
  texts?: string[];
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
