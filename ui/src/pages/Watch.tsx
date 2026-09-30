import { useEffect, useMemo, useRef, useState } from 'react';
import {
  ApiError,
  listProfiles,
  listTasks,
  listWatches,
  previewWatch,
  startWatch,
  stopWatch,
} from '../api';
import ParamForm, { type ParamValues } from '../components/ParamForm';
import { subscribeWatch } from '../sse';
import type { Task, WatchFrame, WatchPreview, WatchRow } from '../types';

const STATUS_LABEL: Record<WatchRow['status'], string> = {
  running: '守护中',
  stopping: '停止中（当前文件跑完）',
  stopped: '已停止',
};

type LogLine = { at: string; text: string; tone: 'ok' | 'bad' | 'muted' };

/** One watch daemon's live line, in the dashboard's voice. */
function frameLine(frame: WatchFrame): LogLine {
  const at = frame.at.replace('T', ' ').slice(5, 19);
  switch (frame.type) {
    case 'started':
      return {
        at,
        text: `开始守护 ${frame.dir} → ${frame.task}（结果写入 ${frame.out_dir}）`,
        tone: 'muted',
      };
    case 'file_done':
      return { at, text: `${frame.file} → ${frame.task}：完成`, tone: 'ok' };
    case 'file_failed':
      return { at, text: `${frame.file} → ${frame.task}：失败（不重试）— ${frame.reason}`, tone: 'bad' };
    case 'dir_unreadable':
      return { at, text: `目录暂时不可读（继续等待）：${frame.error}`, tone: 'bad' };
    case 'dir_readable':
      return { at, text: '目录恢复可读', tone: 'muted' };
    case 'stopped':
      return { at, text: `守护已停止${frame.reason ? `：${frame.reason}` : ''}`, tone: 'muted' };
    case 'lagged':
      return { at, text: '事件过快，部分被跳过；以列表计数为准', tone: 'muted' };
  }
}

export default function Watch() {
  // --- the list, refreshed on a cadence and after every action ---------
  const [rows, setRows] = useState<WatchRow[]>([]);
  const [error, setError] = useState('');

  // --- the form ---------------------------------------------------------
  const [tasks, setTasks] = useState<Task[] | null>(null);
  const [profileNames, setProfileNames] = useState<string[]>(['default']);
  const [dir, setDir] = useState('');
  const [taskName, setTaskName] = useState('');
  const [prompt, setPrompt] = useState('');
  const [params, setParams] = useState<ParamValues>({});
  const [profile, setProfile] = useState('');
  const [model, setModel] = useState('');
  const [outSubdir, setOutSubdir] = useState('out');
  const [intervalSecs, setIntervalSecs] = useState('');
  const [stableMs, setStableMs] = useState('');
  const [includeExisting, setIncludeExisting] = useState(false);
  const [advanced, setAdvanced] = useState(false);
  const [preview, setPreview] = useState<WatchPreview | null>(null);
  const [previewError, setPreviewError] = useState('');
  const [starting, setStarting] = useState(false);
  const [startError, setStartError] = useState('');

  // --- the live log ------------------------------------------------------
  const [logOf, setLogOf] = useState<string | null>(null);
  const [lines, setLines] = useState<LogLine[]>([]);
  const closeStream = useRef<(() => void) | null>(null);

  const refresh = () => {
    listWatches()
      .then(setRows)
      .catch((e) => setError(e instanceof Error ? e.message : String(e)));
  };

  useEffect(() => {
    listTasks()
      .then((all) => {
        setTasks(all);
        if (all.length > 0) setTaskName(all[0].name);
      })
      .catch((e) => setError(e instanceof Error ? e.message : String(e)));
    listProfiles()
      .then((view) => {
        const names = view.profiles.map((p) => p.name);
        setProfileNames(names.length > 0 ? names : ['default']);
      })
      .catch(() => {
        // The plan's own error names the available profiles anyway.
      });
  }, []);

  // The dashboard's own poll: while a daemon runs, its counters move.
  useEffect(() => {
    refresh();
    const timer = window.setInterval(() => {
      if (document.visibilityState === 'visible') refresh();
    }, 3000);
    return () => window.clearInterval(timer);
  }, []);

  // The live tail: one stream at a time, capped, closed on unmount.
  useEffect(() => {
    closeStream.current?.();
    closeStream.current = null;
    setLines([]);
    if (!logOf) return;
    closeStream.current = subscribeWatch(
      logOf,
      (frame) => {
        setLines((current) => [...current.slice(-199), frameLine(frame)]);
      },
      () => {
        setLines((current) => [
          ...current.slice(-199),
          { at: '', text: '（实时流已结束）', tone: 'muted' },
        ]);
        refresh();
      },
    );
    return () => {
      closeStream.current?.();
      closeStream.current = null;
    };
  }, [logOf]);

  const task = useMemo(
    () => tasks?.find((t) => t.name === taskName) ?? null,
    [tasks, taskName],
  );

  const payload = () => {
    const body: Record<string, unknown> = { dir: dir.trim(), task: taskName };
    if (prompt.trim()) body.prompt = prompt.trim();
    if (profile.trim()) body.profile = profile.trim();
    if (model.trim()) body.model = model.trim();
    if (task) {
      for (const spec of task.params) {
        const value = (params[spec.name] ?? '').trim();
        if (!value) continue;
        if (spec.kind === 'number' || spec.kind === 'integer') {
          const parsed = Number(value);
          if (!Number.isNaN(parsed)) body[spec.name] = parsed;
        } else {
          body[spec.name] = value;
        }
      }
    }
    if (outSubdir.trim()) body.out_subdir = outSubdir.trim();
    if (intervalSecs.trim() && Number.isFinite(Number(intervalSecs)))
      body.interval_secs = Number(intervalSecs);
    if (stableMs.trim() && Number.isFinite(Number(stableMs)))
      body.stable_ms = Number(stableMs);
    if (includeExisting) body.include_existing = true;
    return body;
  };

  const equivalent = useMemo(() => {
    if (!dir.trim()) return 'aido watch <目录> -- …';
    const parts = ['aido watch', dir.trim(), '--', taskName];
    if (prompt.trim()) parts.push(`-p ${JSON.stringify(prompt.trim())}`);
    if (profile.trim()) parts.push(`--profile ${profile.trim()}`);
    if (model.trim()) parts.push(`--model ${model.trim()}`);
    if (task) {
      for (const spec of task.params) {
        const value = (params[spec.name] ?? '').trim();
        if (!value) continue;
        if ((spec.kind === 'number' || spec.kind === 'integer') && Number.isNaN(Number(value)))
          continue;
        parts.push(
          spec.kind === 'language' || spec.kind === 'string'
            ? `--${spec.name} ${JSON.stringify(value)}`
            : `--${spec.name} ${value}`,
        );
      }
    }
    parts.push(`--out-dir ${dir.trim()}/${outSubdir.trim() || 'out'}`);
    return parts.join(' ');
  }, [task, taskName, params, prompt, profile, model, dir, outSubdir]);

  async function doPreview() {
    setPreviewError('');
    setPreview(null);
    try {
      setPreview(await previewWatch(payload() as never));
    } catch (e) {
      setPreviewError(e instanceof ApiError ? e.message : String(e));
    }
  }

  async function doStart() {
    if (starting) return;
    setStarting(true);
    setStartError('');
    try {
      const id = await startWatch(payload() as never);
      refresh();
      setLogOf(id);
    } catch (e) {
      setStartError(e instanceof ApiError ? e.message : String(e));
    } finally {
      setStarting(false);
    }
  }

  async function doStop(id: string) {
    try {
      await stopWatch(id);
      refresh();
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e));
    }
  }

  return (
    <>
      <div className="card">
        <div className="card-title">守护进程</div>
        {error && <div className="banner bad">{error}</div>}
        {rows.length === 0 ? (
          <div className="empty">
            还没有由这个 UI 启动的守护进程（CLI 里另起的 watch 是另一个进程，这里看不到）。
          </div>
        ) : (
          rows.map((row) => (
            <div key={row.id} className={logOf === row.id ? 'stage-card selected' : 'stage-card'}>
              <div className="stage-head">
                <span className="stage-no">{row.id}</span>
                <strong>{row.task}</strong>
                <span className="muted-cell">{row.dir}</span>
                <span className={'status ' + (row.status === 'running' ? 'ok' : row.status === 'stopping' ? 'warn' : 'muted')}>
                  {STATUS_LABEL[row.status]}
                </span>
                <div className="actions">
                  <button className="link" onClick={() => setLogOf(logOf === row.id ? null : row.id)}>
                    {logOf === row.id ? '收起日志' : '实时日志'}
                  </button>
                  {row.status !== 'stopped' && (
                    <button className="link danger-link" onClick={() => doStop(row.id)}>
                      停止
                    </button>
                  )}
                </div>
              </div>
              <div className="kv">
                <span className="k">交付目录</span>
                <span className="v">{row.out_dir}</span>
                <span className="k">节奏</span>
                <span className="v">
                  每 {row.interval_ms} ms 扫描 · 稳定 {row.stable_ms} ms
                </span>
                <span className="k">处理</span>
                <span className="v">
                  {row.processed} 完成 · {row.failed} 失败
                  {row.last_file ? ` · 最近 ${row.last_file}` : ''}
                </span>
                <span className="k">启动于</span>
                <span className="v">{row.started_at.replace('T', ' ').slice(0, 19)}</span>
              </div>
              {row.last_error && <div className="banner warn">最近一次失败：{row.last_error}</div>}
              {row.stop_reason && (
                <div className="hint-line">停止原因：{row.stop_reason}</div>
              )}
            </div>
          ))
        )}
        {logOf && lines.length > 0 && (
          <pre className="stream-text">{lines.map((l) => `${l.at}  ${l.text}`).join('\n')}</pre>
        )}
      </div>

      <div className="card">
        <div className="card-title">新建守护</div>
        {startError && <div className="banner bad">{startError}</div>}
        <div className="param-form">
          <div className="param param-wide">
            <label>要守住的目录（服务器上的路径）</label>
            <input
              value={dir}
              onChange={(e) => setDir(e.target.value)}
              placeholder="例如 /home/me/screenshots"
            />
          </div>
          <div className="param">
            <label>任务</label>
            <select value={taskName} onChange={(e) => setTaskName(e.target.value)}>
              {(tasks ?? []).map((t) => (
                <option key={t.name} value={t.name}>
                  {t.name}
                </option>
              ))}
            </select>
          </div>
          <div className="param">
            <label>结果子目录（在守护目录内）</label>
            <input value={outSubdir} onChange={(e) => setOutSubdir(e.target.value)} />
          </div>
          <div className="param param-inline">
            <label>
              <input
                type="checkbox"
                checked={includeExisting}
                onChange={(e) => setIncludeExisting(e.target.checked)}
              />{' '}
              启动时处理已有文件
            </label>
          </div>
          {task && (
            <div className="param param-wide">
              <label>-p 指令（可选）</label>
              <textarea
                rows={2}
                value={prompt}
                onChange={(e) => setPrompt(e.target.value)}
                placeholder="每个文件都带上这条要求"
              />
            </div>
          )}
          {task && <ParamForm task={task} values={params} onChange={setParams} />}
          <button className="link" onClick={() => setAdvanced(!advanced)}>
            {advanced ? '收起高级' : '高级（profile / model / 节奏）'}
          </button>
          {advanced && (
            <div className="param-row">
              <div className="param">
                <label>--profile</label>
                <input
                  list="watch-profile-names"
                  value={profile}
                  placeholder="默认"
                  onChange={(e) => setProfile(e.target.value)}
                />
                <datalist id="watch-profile-names">
                  {profileNames.map((name) => (
                    <option key={name} value={name} />
                  ))}
                </datalist>
              </div>
              <div className="param">
                <label>--model</label>
                <input value={model} placeholder="Profile 的模型" onChange={(e) => setModel(e.target.value)} />
              </div>
              <div className="param">
                <label>--interval（秒）</label>
                <input
                  type="number"
                  min={0.1}
                  step={0.1}
                  value={intervalSecs}
                  placeholder="默认 1"
                  onChange={(e) => setIntervalSecs(e.target.value)}
                />
              </div>
              <div className="param">
                <label>--stable-ms</label>
                <input
                  type="number"
                  min={100}
                  step={100}
                  value={stableMs}
                  placeholder="默认 500"
                  onChange={(e) => setStableMs(e.target.value)}
                />
              </div>
            </div>
          )}
        </div>
        <div className="actions-card" style={{ marginTop: 12 }}>
          <div className="equivalent" title="等效命令（以预览为准）">
            <code>{equivalent}</code>
          </div>
          <div className="actions">
            <button onClick={doPreview} disabled={!dir.trim() || !taskName}>
              预览（probe）
            </button>
            <button
              className="primary"
              onClick={doStart}
              disabled={!dir.trim() || !taskName || starting}
            >
              {starting ? '启动中……' : '启动守护'}
            </button>
          </div>
        </div>
        {previewError && <div className="banner bad">{previewError}</div>}
        {preview && (
          <div className="preview">
            <div className="chips">
              <span className="chip">{preview.task}</span>
              <span className="chip">每 {preview.interval_ms} ms</span>
              <span className="chip">稳定 {preview.stable_ms} ms</span>
              <span className="chip part">→ {preview.out_dir}</span>
            </div>
            <pre>{preview.text}</pre>
          </div>
        )}
      </div>
    </>
  );
}
