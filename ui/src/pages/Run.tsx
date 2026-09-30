import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Link } from 'react-router-dom';
import { ApiError, cancelRun, getRun, listProfiles, listTasks, previewRun, startRun } from '../api';
import ArtifactViewer from '../components/ArtifactViewer';
import Deliveries from '../components/Deliveries';
import Dropzone from '../components/Dropzone';
import ParamForm, { type ParamValues } from '../components/ParamForm';
import PlanPreview from '../components/PlanPreview';
import StreamView from '../components/StreamView';
import TaskPicker from '../components/TaskPicker';
import { subscribe } from '../sse';
import type { DoneFrame, Frame, Preview, RunReport, RunRequestPayload, Task } from '../types';

type Phase = 'idle' | 'running' | 'done' | 'failed';

// The OutputFormat enum the CLI accepts (src/cli.rs): a bad pick would
// be a clap error anyway — the list keeps the form honest.
const FORMATS = ['mp3', 'opus', 'aac', 'flac', 'wav', 'pcm', 'png', 'jpeg', 'webp'];

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
  // Server-side delivery: a name under aido's deliveries directory (the
  // whitelist rule); -o and --out-dir conflict on the CLI, so setting
  // one clears the other here.
  const [outDir, setOutDir] = useState('');
  const [outFile, setOutFile] = useState('');
  // --produce / --format: what the generation should emit and how a
  // single media artifact should be encoded.
  const [produce, setProduce] = useState<string[]>([]);
  const [format, setFormat] = useState('');
  // What --profile may name: the config's own profiles, or the built-in
  // "default" when the user defined none (the config's own rule).
  const [profileNames, setProfileNames] = useState<string[]>(['default']);

  const [preview, setPreview] = useState<Preview | null>(null);
  const [previewError, setPreviewError] = useState('');
  const [phase, setPhase] = useState<Phase>('idle');
  const [starting, setStarting] = useState(false);
  const [runId, setRunId] = useState('');
  const [streamText, setStreamText] = useState('');
  const [step, setStep] = useState<{ done: number; total: number; label: string } | null>(null);
  const [warnings, setWarnings] = useState<string[]>([]);
  const [report, setReport] = useState<RunReport | null>(null);
  const [failure, setFailure] = useState('');
  // Whether the failed phase has a history record to link to: cancelled
  // runs are recorded, error-frame runs (a failed execute) are not.
  const [failureHasRecord, setFailureHasRecord] = useState(false);
  const closeStream = useRef<(() => void) | null>(null);

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
    // A mid-run navigation must not leave the stream pushing into a
    // dead page.
    return () => closeStream.current?.();
  }, []);

  // --no-split only exists for the slicing processors; switching tasks
  // must not carry it invisibly.
  useEffect(() => {
    setNoSplit(false);
  }, [taskName]);

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
    if (produce.length > 0) body.produce = produce;
    if (format) body.format = format;
    if (outDir.trim()) body.out_dir = outDir.trim();
    if (outFile.trim()) body.out_file = outFile.trim();
    if (texts.length > 0) body.texts = texts;
    return body as unknown as RunRequestPayload;
  }, [task, taskName, params, prompt, profile, model, noSplit, produce, format, outDir, outFile, texts]);

  const equivalent = useMemo(() => {
    const parts = ['aido', taskName, ...files.map((f) => f.name)];
    if (prompt.trim()) parts.push(`-p ${JSON.stringify(prompt.trim())}`);
    if (profile.trim()) parts.push(`--profile ${profile.trim()}`);
    if (model.trim()) parts.push(`--model ${model.trim()}`);
    if (noSplit) parts.push('--no-split');
    if (produce.length > 0) parts.push(`--produce ${produce.join(',')}`);
    if (format) parts.push(`--format ${format}`);
    if (outDir.trim()) parts.push(`--out-dir <deliveries>/${outDir.trim()}`);
    if (outFile.trim()) parts.push(`-o <deliveries>/${outFile.trim()}`);
    if (task) {
      for (const spec of task.params) {
        const value = (params[spec.name] ?? '').trim();
        if (!value) continue;
        if (
          (spec.kind === 'number' || spec.kind === 'integer') &&
          Number.isNaN(Number(value))
        ) {
          continue;
        }
        parts.push(
          spec.kind === 'language' || spec.kind === 'string'
            ? `--${spec.name} ${JSON.stringify(value)}`
            : `--${spec.name} ${value}`,
        );
      }
    }
    for (const text of texts) parts.push(`--text ${JSON.stringify(text)}`);
    return parts.join(' ');
  }, [task, taskName, params, prompt, profile, model, noSplit, produce, format, outDir, outFile, files, texts]);

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
    if (starting || phase === 'running') return;
    setStarting(true);
    setPreviewError('');
    setFailure('');
    setReport(null);
    setFailureHasRecord(false);
    setWarnings([]);
    setStreamText('');
    setStep(null);
    // Settled by a closing frame, or by the onEnd fallback below.
    let settled = false;
    try {
      const id = await startRun(payload(), files);
      setRunId(id);
      setPhase('running');
      closeStream.current = subscribe(
        id,
        (frame: Frame | DoneFrame) => {
          if (frame.type === 'delta') {
            setStreamText((current) => current + frame.text);
          } else if (frame.type === 'step') {
            setStep({ done: frame.done, total: frame.total, label: frame.label });
          } else if (frame.type === 'warning') {
            setWarnings((current) => [...current, frame.text]);
          } else if (frame.type === 'done') {
            settled = true;
            setReport(frame as DoneFrame);
            setPhase('done');
          } else if (frame.type === 'error') {
            // An execute error leaves no record — do not offer one.
            settled = true;
            setFailure(`${frame.kind}: ${frame.message}`);
            setPhase('failed');
          } else if (frame.type === 'cancelled') {
            settled = true;
            setFailureHasRecord(true);
            setFailure('已取消。');
            setPhase('failed');
          }
        },
        () => {
          // The stream ended with no closing frame: the run finished
          // between the 202 and this subscription (a fast failure wins
          // that race). History is the truth; without a record, say so.
          if (settled) return;
          settled = true;
          getRun(id)
            .then((record) => {
              setFailureHasRecord(true);
              setReport(record);
              if (record.error || record.status.status !== 'complete') {
                setFailure(record.error?.message ?? record.status.reason ?? '运行未完成。');
                setPhase('failed');
              } else {
                setPhase('done');
              }
            })
            .catch(() => {
              setFailure('运行中断，未留下记录（连接失败或服务未及应答）。');
              setPhase('failed');
            });
        },
      );
    } catch (e) {
      setFailure(e instanceof ApiError ? e.message : String(e));
      setPhase('failed');
    } finally {
      setStarting(false);
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
    closeStream.current?.();
    closeStream.current = null;
    setPhase('idle');
    setReport(null);
    setFailure('');
    setFailureHasRecord(false);
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
          <Deliveries report={report} />
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
            {runId && failureHasRecord && (
              <span className="chips">
                <Link className="link" to={`/runs/${runId}`}>
                  查看记录 →
                </Link>
              </span>
            )}
            <button onClick={reset}>返回</button>
          </div>
          <div className="banner bad">{failure}</div>
          {streamText && <pre className="artifact-text">{streamText}</pre>}
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
                <div className="param">
                  <label htmlFor="adv-format">--format</label>
                  <select
                    id="adv-format"
                    value={format}
                    onChange={(e) => setFormat(e.target.value)}
                  >
                    <option value="">默认编码</option>
                    {FORMATS.map((f) => (
                      <option key={f} value={f}>
                        {f}
                      </option>
                    ))}
                  </select>
                </div>
                <div className="param">
                  <label>--produce</label>
                  <div className="chips">
                    {['text', 'image', 'audio'].map((kind) => (
                      <button
                        key={kind}
                        className={produce.includes(kind) ? 'chip part' : 'chip'}
                        onClick={() =>
                          setProduce((current) =>
                            current.includes(kind)
                              ? current.filter((k) => k !== kind)
                              : [...current, kind],
                          )
                        }
                      >
                        {kind}
                      </button>
                    ))}
                  </div>
                </div>
              </div>
            )}
          </section>

          <section className="card">
            <div className="card-title">4 · 交付（可选）</div>
            <div className="hint-line" style={{ marginBottom: 10 }}>
              默认交付给浏览器；这里改为写入服务器磁盘 —— 名字只落在 aido
              的交付目录（deliveries）之内，不能是路径。
            </div>
            <div className="param-row">
              <div className="param">
                <label htmlFor="out-dir">--out-dir</label>
                <input
                  id="out-dir"
                  value={outDir}
                  placeholder="子目录名，如 job-1"
                  onChange={(e) => {
                    setOutDir(e.target.value);
                    if (e.target.value.trim()) setOutFile('');
                  }}
                />
              </div>
              <div className="param">
                <label htmlFor="out-file">-o 文件名</label>
                <input
                  id="out-file"
                  value={outFile}
                  placeholder="文件名，如 report.txt"
                  onChange={(e) => {
                    setOutFile(e.target.value);
                    if (e.target.value.trim()) setOutDir('');
                  }}
                />
              </div>
            </div>
          </section>

          <section className="card actions-card">
            <div className="equivalent" title="等效命令（以预览为准）">
              <code>{equivalent}</code>
            </div>
            <div className="actions">
              <button onClick={doPreview} disabled={!taskName || starting}>
                预览（dry-run）
              </button>
              <button
                className="primary"
                onClick={doRun}
                disabled={!taskName || starting}
              >
                {starting ? '提交中……' : '运行'}
              </button>
            </div>
          </section>

          <PlanPreview preview={preview} error={previewError} />
        </>
      )}
    </div>
  );
}
