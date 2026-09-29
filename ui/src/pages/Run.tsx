import { useCallback, useEffect, useMemo, useState } from 'react';
import { Link } from 'react-router-dom';
import { ApiError, cancelRun, listProfiles, listTasks, previewRun, startRun } from '../api';
import ArtifactViewer from '../components/ArtifactViewer';
import Dropzone from '../components/Dropzone';
import ParamForm, { type ParamValues } from '../components/ParamForm';
import PlanPreview from '../components/PlanPreview';
import StreamView from '../components/StreamView';
import TaskPicker from '../components/TaskPicker';
import { subscribe } from '../sse';
import type { DoneFrame, Frame, Preview, RunReport, RunRequestPayload, Task } from '../types';

type Phase = 'idle' | 'running' | 'done' | 'failed';

export default function Run() {
  const [tasks, setTasks] = useState<Task[] | null>(null);
  const [loadError, setLoadError] = useState('');
  const [taskName, setTaskName] = useState('');
  const [files, setFiles] = useState<File[]>([]);
  const [texts, setTexts] = useState<string[]>([]);
  const [params, setParams] = useState<ParamValues>({});
  const [prompt, setPrompt] = useState('');
  const [profile, setProfile] = useState('');
  const [model, setModel] = useState('');
  const [advanced, setAdvanced] = useState(false);
  const [noSplit, setNoSplit] = useState(false);
  // What --profile may name: the config's own profiles, or the built-in
  // "default" when the user defined none (the config's own rule).
  const [profileNames, setProfileNames] = useState<string[]>(['default']);

  const [preview, setPreview] = useState<Preview | null>(null);
  const [previewError, setPreviewError] = useState('');
  const [phase, setPhase] = useState<Phase>('idle');
  const [runId, setRunId] = useState('');
  const [streamText, setStreamText] = useState('');
  const [step, setStep] = useState<{ done: number; total: number; label: string } | null>(null);
  const [warnings, setWarnings] = useState<string[]>([]);
  const [report, setReport] = useState<RunReport | null>(null);
  const [failure, setFailure] = useState('');

  useEffect(() => {
    listTasks()
      .then((all) => {
        setTasks(all);
        if (all.length > 0) setTaskName(all[0].name);
      })
      .catch((e: ApiError) => setLoadError(e.message));
    listProfiles()
      .then((view) => {
        const names = view.profiles.map((p) => p.name);
        setProfileNames(names.length > 0 ? names : ['default']);
      })
      .catch(() => {
        // An unloadable config keeps the free-text box; the plan's own
        // error names the available profiles anyway.
      });
  }, []);

  const task = useMemo(() => tasks?.find((t) => t.name === taskName) ?? null, [tasks, taskName]);

  const payload = useCallback((): RunRequestPayload => {
    const body: Record<string, unknown> = { task: taskName };
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
    if (noSplit) body.no_split = true;
    if (texts.length > 0) body.texts = texts;
    return body as unknown as RunRequestPayload;
  }, [task, taskName, params, prompt, profile, model, noSplit, texts]);

  const equivalent = useMemo(() => {
    const parts = ['aido', taskName, ...files.map((f) => f.name)];
    if (prompt.trim()) parts.push(`-p ${JSON.stringify(prompt.trim())}`);
    if (profile.trim()) parts.push(`--profile ${profile.trim()}`);
    if (model.trim()) parts.push(`--model ${model.trim()}`);
    if (task) {
      for (const spec of task.params) {
        const value = (params[spec.name] ?? '').trim();
        if (value) parts.push(`--${spec.name} ${value}`);
      }
    }
    for (const text of texts) parts.push(`--text ${JSON.stringify(text)}`);
    return parts.join(' ');
  }, [task, taskName, params, prompt, profile, model, files, texts]);

  async function doPreview() {
    setPreviewError('');
    setPreview(null);
    try {
      setPreview(await previewRun(payload(), files));
    } catch (e) {
      setPreviewError(e instanceof ApiError ? e.message : String(e));
    }
  }

  async function doRun() {
    setPreviewError('');
    setFailure('');
    setReport(null);
    setWarnings([]);
    setStreamText('');
    setStep(null);
    try {
      const id = await startRun(payload(), files);
      setRunId(id);
      setPhase('running');
      // The late subscriber's contract: events broadcast before
      // subscribing are gone; the done frame and the detail endpoint
      // carry the whole truth.
      subscribe(id, (frame: Frame | DoneFrame) => {
        if (frame.type === 'delta') {
          setStreamText((current) => current + frame.text);
        } else if (frame.type === 'step') {
          setStep({ done: frame.done, total: frame.total, label: frame.label });
        } else if (frame.type === 'warning') {
          setWarnings((current) => [...current, frame.text]);
        } else if (frame.type === 'done') {
          const done = frame as DoneFrame;
          setReport(done);
          setPhase('done');
        } else if (frame.type === 'error') {
          setFailure(`${frame.kind}: ${frame.message}`);
          setPhase('failed');
        } else if (frame.type === 'cancelled') {
          setFailure('已取消。');
          setPhase('failed');
        }
      });
    } catch (e) {
      setFailure(e instanceof ApiError ? e.message : String(e));
      setPhase('failed');
    }
  }

  async function doCancel() {
    if (!runId) return;
    try {
      await cancelRun(runId);
    } catch {
      // The run already ended; the stream says which way.
    }
  }

  function reset() {
    setPhase('idle');
    setReport(null);
    setFailure('');
    setStreamText('');
    setStep(null);
    setWarnings([]);
    setRunId('');
  }

  if (loadError) {
    return <div className="banner bad">无法加载任务列表：{loadError}</div>;
  }
  if (!tasks) return <div className="empty">加载任务……</div>;

  return (
    <div className="run-page">
      {phase === 'running' ? (
        <StreamView
          text={streamText}
          step={step}
          warnings={warnings}
          onCancel={doCancel}
        />
      ) : phase === 'done' && report ? (
        <div className="card result">
          <div className="card-title">
            完成 · {report.task}
            <span className="chips">
              <Link className="link" to={`/runs/${report.run_id}`}>
                查看完整记录 →
              </Link>
            </span>
            <button onClick={reset}>再来一次</button>
          </div>
          {report.warnings.length > 0 && (
            <ul className="warnings">
              {report.warnings.map((w, i) => (
                <li key={i}>{w}</li>
              ))}
            </ul>
          )}
          <ArtifactViewer
            runId={report.run_id}
            artifacts={report.artifacts}
            failedParts={report.failed_parts}
          />
          {report.error && <div className="banner warn">{report.error.message}</div>}
        </div>
      ) : phase === 'failed' ? (
        <div className="card result">
          <div className="card-title">
            {runId ? '运行失败' : '无法开始'}
            {runId && (
              <span className="chips">
                <Link className="link" to={`/runs/${runId}`}>
                  查看记录 →
                </Link>
              </span>
            )}
            <button onClick={reset}>返回</button>
          </div>
          <div className="banner bad">{failure}</div>
          {streamText && (
            <pre className="artifact-text">{streamText}</pre>
          )}
        </div>
      ) : (
        <>
          <section className="card">
            <div className="card-title">1 · 选任务</div>
            <TaskPicker tasks={tasks} selected={taskName} onSelect={setTaskName} />
          </section>

          <section className="card">
            <div className="card-title">2 · 给材料</div>
            <Dropzone files={files} texts={texts} onFiles={setFiles} onTexts={setTexts} />
          </section>

          <section className="card">
            <div className="card-title">3 · 参数</div>
            {task && (
              <>
                {(taskName === 'ask' || prompt.trim() !== '') && (
                  <div className="param param-wide">
                    <label htmlFor="param-prompt">-p 指令</label>
                    <textarea
                      id="param-prompt"
                      rows={3}
                      placeholder={taskName === 'ask' ? '必填：这次要做什么' : '可选：附加要求'}
                      value={prompt}
                      onChange={(e) => setPrompt(e.target.value)}
                    />
                  </div>
                )}
                <ParamForm task={task} values={params} onChange={setParams} />
                {(task.processor === 'ocr-tiles' ||
                  task.processor === 'chunk-join' ||
                  task.processor === 'chunk-reduce') && (
                  <div className="param param-inline">
                    <label htmlFor="param-no-split">--no-split</label>
                    <input
                      id="param-no-split"
                      type="checkbox"
                      checked={noSplit}
                      onChange={(e) => setNoSplit(e.target.checked)}
                    />
                    <span className="hint">关闭切片 / 分块，整份发送</span>
                  </div>
                )}
              </>
            )}
            <button className="link" onClick={() => setAdvanced(!advanced)}>
              {advanced ? '收起高级' : '高级（profile / model）'}
            </button>
            {advanced && (
              <div className="param-row">
                <div className="param">
                  <label htmlFor="adv-profile">--profile</label>
                  <input
                    id="adv-profile"
                    list="profile-names"
                    value={profile}
                    placeholder="默认"
                    onChange={(e) => setProfile(e.target.value)}
                  />
                  <datalist id="profile-names">
                    {profileNames.map((name) => (
                      <option key={name} value={name} />
                    ))}
                  </datalist>
                </div>
                <div className="param">
                  <label htmlFor="adv-model">--model</label>
                  <input
                    id="adv-model"
                    value={model}
                    placeholder="Profile 的模型"
                    onChange={(e) => setModel(e.target.value)}
                  />
                </div>
              </div>
            )}
          </section>

          <section className="card actions-card">
            <div className="equivalent" title="等效命令（以预览为准）">
              <code>{equivalent}</code>
            </div>
            <div className="actions">
              <button onClick={doPreview} disabled={!taskName}>
                预览（dry-run）
              </button>
              <button className="primary" onClick={doRun} disabled={!taskName}>
                运行
              </button>
            </div>
          </section>

          <PlanPreview preview={preview} error={previewError} />
        </>
      )}
    </div>
  );
}
