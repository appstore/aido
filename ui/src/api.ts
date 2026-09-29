import type {
  ChainPreview,
  ChainRequestPayload,
  ConfigView,
  Preview,
  ProfilesView,
  RunReport,
  RunRequestPayload,
  RunRow,
  SaveResult,
  Task,
  TaskSaveResult,
  TaskSource,
} from './types';

// The session token: `aido ui` prints one URL (`?t=…`); landing there
// keeps the token for the session. Development pins it through
// VITE_AIDO_TOKEN (see .env.development and vite.config.ts).
function initialToken(): string {
  const fromUrl = new URLSearchParams(window.location.search).get('t');
  if (fromUrl) sessionStorage.setItem('aido-token', fromUrl);
  return sessionStorage.getItem('aido-token') ?? import.meta.env.VITE_AIDO_TOKEN ?? '';
}

export const token = initialToken();
export const hasToken = token !== '';

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}

async function request(path: string, init?: RequestInit): Promise<Response> {
  const response = await fetch(path, {
    ...init,
    headers: { 'X-Aido-Token': token, ...(init?.headers ?? {}) },
  });
  if (!response.ok) {
    let message =
      response.status === 401
        ? '会话令牌缺失或无效：请从 aido ui 打印的链接进入（URL 里的 ?t=…）'
        : `HTTP ${response.status}`;
    try {
      const body = (await response.json()) as { error?: { message?: string } };
      message = body.error?.message ?? message;
    } catch {
      // not JSON — keep the status line
    }
    throw new ApiError(response.status, message);
  }
  return response;
}

async function json<T>(path: string): Promise<T> {
  return (await request(path).then((r) => r.json())) as T;
}

export function listTasks(): Promise<Task[]> {
  return json<{ tasks: Task[] }>('/api/tasks').then((b) => b.tasks);
}

export function getTask(name: string): Promise<Task> {
  return json<Task>(`/api/tasks/${encodeURIComponent(name)}`);
}

export function getTaskSource(name: string): Promise<TaskSource> {
  return json<TaskSource>(`/api/tasks/${encodeURIComponent(name)}/source`);
}

/** Save a custom task: the server parses with the loader's own rules
 * first, so the file on disk never becomes invalid. A 409 means a file
 * already exists — resend with overwrite after confirming. */
export function saveTask(
  name: string,
  toml: string,
  overwrite = false,
): Promise<TaskSaveResult> {
  return request('/api/tasks', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ name, toml, overwrite }),
  }).then((r) => r.json());
}

export async function deleteTask(name: string): Promise<void> {
  await request(`/api/tasks/${encodeURIComponent(name)}`, { method: 'DELETE' });
}

export function listRuns(query: { task?: string; status?: string } = {}): Promise<RunRow[]> {
  const params = new URLSearchParams();
  if (query.task) params.set('task', query.task);
  if (query.status) params.set('status', query.status);
  const suffix = params.size > 0 ? `?${params}` : '';
  return json<{ runs: RunRow[] }>(`/api/runs${suffix}`).then((b) => b.runs);
}

export function getRun(id: string): Promise<RunReport> {
  return json<RunReport>(`/api/runs/${encodeURIComponent(id)}`);
}

/** A URL the browser itself can fetch (img/audio/a tags carry no
 * headers), so the token rides the query string here. */
export function artifactUrl(runId: string, artifactId: string): string {
  return (
    `/api/runs/${encodeURIComponent(runId)}/artifacts/${encodeURIComponent(artifactId)}` +
    `?t=${encodeURIComponent(token)}`
  );
}

async function postForm(path: string, payload: RunRequestPayload, files: File[]): Promise<Response> {
  const form = new FormData();
  form.append('request', JSON.stringify(payload));
  for (const file of files) form.append('file', file, file.name);
  return request(path, { method: 'POST', body: form });
}

export async function previewRun(payload: RunRequestPayload, files: File[]): Promise<Preview> {
  const response = await postForm('/api/runs/preview', payload, files);
  return (await response.json()) as Preview;
}

export async function startRun(payload: RunRequestPayload, files: File[]): Promise<string> {
  const response = await postForm('/api/runs', payload, files);
  const body = (await response.json()) as { run_id: string };
  return body.run_id;
}

export async function cancelRun(runId: string): Promise<void> {
  await request(`/api/runs/${encodeURIComponent(runId)}/cancel`, { method: 'POST' });
}

export function getConfig(): Promise<ConfigView> {
  return json<ConfigView>('/api/config');
}

export function saveConfig(toml: string): Promise<SaveResult> {
  return request('/api/config', {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ toml }),
  }).then((r) => r.json());
}

export function listProfiles(): Promise<ProfilesView> {
  return json<ProfilesView>('/api/profiles');
}

export async function previewChain(
  payload: ChainRequestPayload,
  files: File[],
): Promise<ChainPreview> {
  const response = await postForm('/api/chain/preview', payload as unknown as RunRequestPayload, files);
  return (await response.json()) as ChainPreview;
}

export async function startChain(payload: ChainRequestPayload, files: File[]): Promise<string> {
  const response = await postForm(
    '/api/chain',
    payload as unknown as RunRequestPayload,
    files,
  );
  return ((await response.json()) as { run_id: string }).run_id;
}
