import { useEffect, useMemo, useState } from 'react';
import { ApiError, deleteTask, getTaskSource, listTasks, saveTask } from '../api';
import type { Task, TaskSource } from '../types';

// The TaskFile grammar the server's loader enforces (src/tasks.rs):
// the form offers every field except [defaults]/[options] — those belong
// to the TOML tab, where the file is the truth.
const OPERATIONS = [
  ['generate', 'generate · 文本生成'],
  ['speech', 'speech · 文本转语音'],
  ['transcribe', 'transcribe · 音频转写'],
  ['image', 'image · 图像生成'],
] as const;

const PROCESSORS = [
  ['single', 'single · 整体一次请求'],
  ['ocr-tiles', 'ocr-tiles · 长图切片'],
  ['chunk-join', 'chunk-join · 分块后拼接'],
  ['chunk-reduce', 'chunk-reduce · 分块后归纳'],
] as const;

const KINDS = ['text', 'image', 'audio'] as const;
const PARAMS = ['to', 'voice', 'speed', 'count', 'size'] as const;

interface TaskForm {
  name: string;
  operation: string;
  instruction: string;
  profile: string;
  inputTypes: string[];
  requiredTypes: string[];
  outputTypes: string[];
  processor: string;
  perPart: boolean;
  maxInputs: string;
  requiresMaterial: boolean;
  params: string[];
}

const EMPTY_FORM: TaskForm = {
  name: '',
  operation: 'generate',
  instruction: '',
  profile: '',
  inputTypes: [],
  requiredTypes: [],
  outputTypes: ['text'],
  processor: 'single',
  perPart: false,
  maxInputs: '',
  requiresMaterial: true,
  params: [],
};

/** TOML emission for the wizard's constrained fields: basic strings
 * with the escapes TOML defines, arrays of the same. The server parses
 * before writing, so a mistake here can never corrupt a file. */
function tomlString(value: string): string {
  const escaped = value
    .replace(/\\/g, '\\\\')
    .replace(/"/g, '\\"')
    .replace(/\n/g, '\\n')
    .replace(/\r/g, '\\r')
    .replace(/\t/g, '\\t');
  return `"${escaped}"`;
}

function tomlArray(values: string[]): string {
  return `[${values.map((v) => tomlString(v)).join(', ')}]`;
}

function buildToml(form: TaskForm): string {
  const lines: string[] = [`operation = ${tomlString(form.operation)}`];
  if (form.instruction.trim()) lines.push(`instruction = ${tomlString(form.instruction)}`);
  if (form.profile.trim()) lines.push(`profile = ${tomlString(form.profile.trim())}`);
  if (form.inputTypes.length > 0) lines.push(`input_types = ${tomlArray(form.inputTypes)}`);
  if (form.requiredTypes.length > 0)
    lines.push(`required_types = ${tomlArray(form.requiredTypes)}`);
  lines.push(`output_types = ${tomlArray(form.outputTypes)}`);
  if (form.maxInputs.trim() !== '' && Number.isFinite(Number(form.maxInputs)))
    lines.push(`max_inputs = ${Number(form.maxInputs)}`);
  if (!form.requiresMaterial) lines.push('requires_material = false');
  if (form.processor !== 'single') lines.push(`processor = ${tomlString(form.processor)}`);
  if (form.perPart) lines.push('per_part = true');
  if (form.params.length > 0) lines.push(`params = ${tomlArray(form.params)}`);
  return `${lines.join('\n')}\n`;
}

export default function Tasks() {
  const [tasks, setTasks] = useState<Task[]>([]);
  const [error, setError] = useState('');
  const [saved, setSaved] = useState<{ name: string; path: string } | null>(null);

  // The wizard: null = closed; mode is the tab. Editing a task opens
  // the TOML tab with the file's own bytes (the form cannot round-trip
  // [defaults]/[options], the file can).
  const [open, setOpen] = useState(false);
  const [mode, setMode] = useState<'form' | 'toml'>('form');
  const [editing, setEditing] = useState<string | null>(null);
  const [form, setForm] = useState<TaskForm>(EMPTY_FORM);
  const [toml, setToml] = useState('');
  const [saveError, setSaveError] = useState('');
  const [saving, setSaving] = useState(false);
  const [viewing, setViewing] = useState<TaskSource | null>(null);

  const load = () => {
    listTasks()
      .then(setTasks)
      .catch((e) => setError(e.message));
  };
  useEffect(load, []);

  const preview = useMemo(
    () => (mode === 'form' ? buildToml(form) : toml),
    [mode, form, toml],
  );
  const wizardName = mode === 'form' ? form.name.trim() : editing ?? form.name.trim();
  const shadowsBuiltin =
    wizardName !== '' && tasks.some((t) => t.builtin && t.name === wizardName);

  const openCreate = () => {
    setOpen(true);
    setMode('form');
    setEditing(null);
    setForm(EMPTY_FORM);
    setToml('');
    setSaveError('');
    setSaved(null);
  };

  const openEdit = async (name: string) => {
    try {
      const source = await getTaskSource(name);
      setOpen(true);
      setMode('toml');
      setEditing(name);
      setForm({ ...EMPTY_FORM, name });
      setToml(source.toml);
      setSaveError('');
      setSaved(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const submit = async (name: string, body: string, overwrite: boolean) => {
    setSaving(true);
    setSaveError('');
    try {
      const result = await saveTask(name, body, overwrite);
      setSaved({ name: result.task.name, path: result.path });
      setOpen(false);
      setViewing(null);
      load();
    } catch (e) {
      if (e instanceof ApiError && e.status === 409 && !overwrite) {
        if (window.confirm(`任务 “${name}” 已有定义文件，覆盖它？`)) {
          return submit(name, body, true);
        }
      }
      setSaveError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };

  const remove = async (name: string) => {
    if (!window.confirm(`删除任务 “${name}” 的定义文件？`)) return;
    try {
      await deleteTask(name);
      setSaved({ name, path: '' });
      load();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const toggle = (key: 'inputTypes' | 'requiredTypes' | 'outputTypes' | 'params', value: string) => {
    setForm((f) => ({
      ...f,
      [key]: f[key].includes(value) ? f[key].filter((v) => v !== value) : [...f[key], value],
    }));
  };

  return (
    <>
      <div className="card">
        <div className="card-title">任务库</div>
        {error && <div className="banner bad">{error}</div>}
        {saved && (
          <div className="banner ok">
            已保存{saved.path ? `到 ${saved.path}` : ''}；任务 “{saved.name}” 立即可用。
          </div>
        )}
        <div className="filters">
          <button className="primary" onClick={openCreate}>
            ＋ 新建任务
          </button>
          <span className="hint-line">
            任务是 tasks 目录里的 TOML 文件；自定义任务会覆盖同名内置任务。
          </span>
        </div>
        <table className="run-table">
          <thead>
            <tr>
              <th>任务</th>
              <th>操作</th>
              <th>契约</th>
              <th>处理</th>
              <th>来源</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {tasks.map((task) => (
              <tr key={task.name}>
                <td>
                  <code>{task.name}</code>
                </td>
                <td>{task.operation}</td>
                <td className="muted-cell">
                  {(task.input_types ?? task.required_types).join(' / ') || '任意'} →{' '}
                  {task.output_types.join(' / ')}
                </td>
                <td>
                  {task.processor}
                  {task.per_part && ' · 逐文件'}
                </td>
                <td>
                  <span className={task.builtin ? 'chip' : 'chip part'}>
                    {task.builtin ? '内置' : '自定义'}
                  </span>
                </td>
                <td>
                  <button
                    className="link"
                    onClick={async () => {
                      try {
                        setViewing(await getTaskSource(task.name));
                      } catch (e) {
                        setError(e instanceof Error ? e.message : String(e));
                      }
                    }}
                  >
                    源码
                  </button>
                  {!task.builtin && (
                    <>
                      {' '}
                      <button className="link" onClick={() => openEdit(task.name)}>
                        编辑
                      </button>{' '}
                      <button className="link danger-link" onClick={() => remove(task.name)}>
                        删除
                      </button>
                    </>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {viewing && (
        <div className="card">
          <div className="card-title">
            {viewing.name} 的定义{viewing.builtin ? '（内置，只读）' : ''}
          </div>
          {viewing.path && <div className="hint-line" style={{ marginBottom: 8 }}>{viewing.path}</div>}
          <pre className="toml-editor">{viewing.toml}</pre>
          <div className="actions" style={{ marginTop: 10 }}>
            <button onClick={() => setViewing(null)}>
              关闭
            </button>
            {!viewing.builtin && (
              <button
                onClick={() => {
                  const name = viewing.name;
                  setViewing(null);
                  void openEdit(name);
                }}
              >
                去 TOML 编辑
              </button>
            )}
          </div>
        </div>
      )}

      {open && (
        <div className="card">
          <div className="card-title">{editing ? `编辑 ${editing}` : '新建任务'}</div>
          {saveError && <div className="banner bad">{saveError}</div>}
          <div className="tabs">
            <button className={mode === 'form' ? 'tab active' : 'tab'} onClick={() => setMode('form')}>
              表单
            </button>
            <button
              className={mode === 'toml' ? 'tab active' : 'tab'}
              onClick={() => {
                if (mode === 'form') setToml(buildToml(form));
                setMode('toml');
              }}
            >
              TOML
            </button>
          </div>

          {mode === 'form' ? (
            <div className="param-form">
              <div className="param">
                <label>name</label>
                <input
                  value={form.name}
                  onChange={(e) => setForm({ ...form, name: e.target.value })}
                  placeholder="my-task"
                />
                {shadowsBuiltin && (
                  <span className="hint-line">将覆盖同名内置任务</span>
                )}
              </div>
              <div className="param">
                <label>operation</label>
                <select
                  value={form.operation}
                  onChange={(e) => setForm({ ...form, operation: e.target.value })}
                >
                  {OPERATIONS.map(([value, label]) => (
                    <option key={value} value={value}>
                      {label}
                    </option>
                  ))}
                </select>
              </div>
              <div className="param">
                <label>profile（可选）</label>
                <input
                  value={form.profile}
                  onChange={(e) => setForm({ ...form, profile: e.target.value })}
                  placeholder="default"
                />
              </div>
              <div className="param param-wide">
                <label>instruction</label>
                <textarea
                  rows={4}
                  value={form.instruction}
                  onChange={(e) => setForm({ ...form, instruction: e.target.value })}
                  placeholder="这个任务要模型做什么（多行指令）"
                />
              </div>
              {(
                [
                  ['inputTypes', 'input_types（勾选 = 只接受这些；不勾 = 适配器支持的全部）'],
                  ['requiredTypes', 'required_types（必须出现的输入种类）'],
                  ['outputTypes', 'output_types（至少一项）'],
                ] as const
              ).map(([key, label]) => (
                <div className="param" key={key}>
                  <label>{label}</label>
                  <div className="chips">
                    {KINDS.map((kind) => (
                      <button
                        key={kind}
                        className={form[key].includes(kind) ? 'chip part' : 'chip'}
                        onClick={() => toggle(key, kind)}
                      >
                        {kind}
                      </button>
                    ))}
                  </div>
                </div>
              ))}
              <div className="param">
                <label>processor</label>
                <select
                  value={form.processor}
                  onChange={(e) => setForm({ ...form, processor: e.target.value })}
                >
                  {PROCESSORS.map(([value, label]) => (
                    <option key={value} value={value}>
                      {label}
                    </option>
                  ))}
                </select>
              </div>
              <div className="param">
                <label>max_inputs（可选）</label>
                <input
                  type="number"
                  min={1}
                  value={form.maxInputs}
                  onChange={(e) => setForm({ ...form, maxInputs: e.target.value })}
                  placeholder="不限制"
                />
              </div>
              <div className="param param-inline">
                <label>
                  <input
                    type="checkbox"
                    checked={form.requiresMaterial}
                    onChange={(e) => setForm({ ...form, requiresMaterial: e.target.checked })}
                  />{' '}
                  requires_material（需要材料）
                </label>
                <label>
                  <input
                    type="checkbox"
                    checked={form.perPart}
                    onChange={(e) => setForm({ ...form, perPart: e.target.checked })}
                  />{' '}
                  per_part（逐文件批处理）
                </label>
              </div>
              <div className="param param-wide">
                <label>params（运行表单会出现的参数）</label>
                <div className="chips">
                  {PARAMS.map((param) => (
                    <button
                      key={param}
                      className={form.params.includes(param) ? 'chip part' : 'chip'}
                      onClick={() => toggle('params', param)}
                    >
                      --{param}
                    </button>
                  ))}
                </div>
              </div>
            </div>
          ) : (
            <div className="param-form">
              {!editing && (
                <div className="param">
                  <label>name</label>
                  <input
                    value={form.name}
                    onChange={(e) => setForm({ ...form, name: e.target.value })}
                    placeholder="my-task"
                  />
                  {shadowsBuiltin && <span className="hint-line">将覆盖同名内置任务</span>}
                </div>
              )}
              <div className="param param-wide">
                <label>toml</label>
                <textarea
                  className="toml-editor"
                  rows={14}
                  value={toml}
                  onChange={(e) => setToml(e.target.value)}
                  spellCheck={false}
                />
              </div>
            </div>
          )}

          <div className="actions-card" style={{ marginTop: 12 }}>
            <div className="equivalent">
              <code>
                {mode === 'form' ? '将写入：' : ''}
                {preview.split('\n').slice(0, 3).join(' ⏎ ')}
                {preview.split('\n').length > 4 ? ' ⏎ …' : ''}
              </code>
            </div>
            <div className="actions">
              <button disabled={saving} onClick={() => setOpen(false)}>
                取消
              </button>
              <button
                className="primary"
                disabled={saving || wizardName === ''}
                onClick={() => submit(wizardName, mode === 'form' ? buildToml(form) : toml, false)}
              >
                {saving ? '保存中…' : '保存任务'}
              </button>
            </div>
          </div>
          {mode === 'form' && (
            <div className="preview">
              <pre>{preview}</pre>
            </div>
          )}
        </div>
      )}
    </>
  );
}
