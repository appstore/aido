import { useEffect, useMemo, useState } from 'react';
import { Link } from 'react-router-dom';
import { ApiError, listProfiles, listTasks, previewChain } from '../api';
import ArtifactViewer from '../components/ArtifactViewer';
import Dropzone from '../components/Dropzone';
import ParamForm, { type ParamValues } from '../components/ParamForm';
import StreamView from '../components/StreamView';
import { startChain } from '../api';
import { useRunStream } from '../useRunStream';
import type { ChainStagePayload, Task } from '../types';

/** One pipeline card's editable state — the task it runs plus its own
 * parameters, exactly what a `--then` segment would carry. */
interface StageState {
  task: string;
  params: ParamValues;
  prompt: string;
  profile: string;
  model: string;
  noSplit: boolean;
}

function stageState(task: string): StageState {
  return { task, params: {}, prompt: '', profile: '', model: '', noSplit: false };
}

/** The chains the README teaches, one click away. */
const TEMPLATES: { label: string; stages: () => StageState[]; note: string }[] = [
  {
    label: '截图 → 翻译 → 配音',
    note: 'ocr | translate --to zh-CN | tts',
    stages: () => [
      stageState('ocr'),
      { ...stageState('translate'), params: { to: 'zh-CN' } },
      stageState('tts'),
    ],
  },
  {
    label: '语音同传',
    note: 'transcribe | translate --to en | tts',
    stages: () => [stageState('transcribe'), { ...stageState('translate'), params: { to: 'en' } }, stageState('tts')],
  },
  {
    label: '每日简报',
    note: 'summarize | tts',
    stages: () => [stageState('summarize'), stageState('tts')],
  },
  {
    label: '长图提问',
    note: "ocr | ask -p '这页在讲什么？'",
    stages: () => [stageState('ocr'), { ...stageState('ask'), prompt: '这页在讲什么？' }],
  },
];

/** A junction error from plan-time type checking names its stage pair
 * ("stage 1 (ocr) → stage 2 (tts)") — parse it to mark the connection. */
function junctionIndex(error: string): number | null {
  const match = /stage (\d+) \([^)]*\) → stage (\d+)/.exec(error);
  return match ? Number(match[1]) : null;
}

export default function Chain() {
  const [tasks, setTasks] = useState<Task[] | null>(null);
  const [loadError, setLoadError] = useState('');
  const [stages, setStages] = useState<StageState[]>([stageState('summarize'), stageState('translate')]);
  const [files, setFiles] = useState<File[]>([]);
  const [texts, setTexts] = useState<string[]>([]);
  const [profileNames, setProfileNames] = useState<string[]>(['default']);
  const [previewText, setPreviewText] = useState('');
  const [previewError, setPreviewError] = useState('');
  const run = useRunStream();

  useEffect(() => {
    listTasks()
      .then(setTasks)
      .catch((e) => setLoadError(e instanceof ApiError ? e.message : String(e)));
    listProfiles()
      .then((view) => {
        const names = view.profiles.map((p) => p.name);
        setProfileNames(names.length > 0 ? names : ['default']);
      })
      .catch(() => {});
  }, []);

  const taskOf = (name: string): Task | null => tasks?.find((t) => t.name === name) ?? null;

  const payload = useMemo(() => {
    const built = stages.map((stage): ChainStagePayload => {
      const body: Record<string, unknown> = { task: stage.task };
      if (stage.prompt.trim()) body.prompt = stage.prompt.trim();
      if (stage.profile.trim()) body.profile = stage.profile.trim();
      if (stage.model.trim()) body.model = stage.model.trim();
      const task = taskOf(stage.task);
      if (task) {
        for (const spec of task.params) {
          const value = (stage.params[spec.name] ?? '').trim();
          if (!value) continue;
          if (spec.kind === 'number' || spec.kind === 'integer') {
            const parsed = Number(value);
            if (!Number.isNaN(parsed)) body[spec.name] = parsed;
          } else {
            body[spec.name] = value;
          }
        }
      }
      if (stage.noSplit) body.no_split = true;
      return body as unknown as ChainStagePayload;
    });
    const request: Record<string, unknown> = { stages: built };
    if (texts.length > 0) request.texts = texts;
    return request as unknown as { stages: ChainStagePayload[]; texts?: string[] };
  }, [stages, texts, tasks]);

  // The equivalent --then command, built stage by stage.
  const command = useMemo(() => {
    const segments = stages.map((stage) => {
      const words = [stage.task];
      const push = (name: string, value?: string) => {
        if (value && value.trim()) words.push(`--${name} ${value.trim()}`);
      };
      push('profile', stage.profile);
      push('model', stage.model);
      if (stage.prompt.trim()) words.push(`-p ${JSON.stringify(stage.prompt.trim())}`);
      const task = taskOf(stage.task);
      if (task) {
        for (const spec of task.params) {
          const value = (stage.params[spec.name] ?? '').trim();
          if (value) words.push(`--${spec.name} ${value}`);
        }
      }
      if (stage.noSplit) words.push('--no-split');
      return words.join(' ');
    });
    return `aido ${[...files.map((f) => f.name), ...segments].join(' --then ')}`;
  }, [stages, files, tasks]);

  function update(index: number, patch: Partial<StageState>) {
    setStages((current) => current.map((s, i) => (i === index ? { ...s, ...patch } : s)));
  }
  function move(index: number, delta: -1 | 1) {
    setStages((current) => {
      const next = [...current];
      const target = index + delta;
      if (target < 0 || target >= next.length) return current;
      [next[index], next[target]] = [next[target], next[index]];
      return next;
    });
  }

  async function doPreview() {
    setPreviewError('');
    setPreviewText('');
    try {
      const preview = await previewChain(payload, files);
      setPreviewText(preview.text);
    } catch (e) {
      setPreviewError(e instanceof ApiError ? e.message : String(e));
    }
  }

  if (loadError) return <div className="banner bad">无法加载任务列表：{loadError}</div>;
  if (!tasks) return <div className="empty">加载任务……</div>;

  const badJunction = previewError ? junctionIndex(previewError) : null;
  const report = run.report;
  const deliverables = report
    ? report.artifacts.slice(Math.max(0, report.artifacts.length - (report.last_stage_len ?? 0)))
    : [];
  const intermediates = report
    ? report.artifacts.slice(0, Math.max(0, report.artifacts.length - (report.last_stage_len ?? 0)))
    : [];

  return (
    <div className="chain-page">
      {run.phase === 'running' ? (
        <StreamView text={run.streamText} step={run.step} warnings={run.warnings} onCancel={run.cancel} />
      ) : report && (run.phase === 'done' || run.phase === 'failed') ? (
        <div className="card result">
          <div className="card-title">
            {run.phase === 'done' ? '完成' : '运行失败'} · {report.task}
            <span className="chips">
              <Link className="link" to={`/runs/${report.run_id}`}>
                查看完整记录 →
              </Link>
            </span>
            <button onClick={run.reset}>再来一次</button>
          </div>
          {run.failure && <div className="banner bad">{run.failure}</div>}
          {report.warnings.length > 0 && (
            <ul className="warnings">
              {report.warnings.map((w, i) => (
                <li key={i}>{w}</li>
              ))}
            </ul>
          )}
          {deliverables.length > 0 && (
            <>
              <div className="group-label">最终交付</div>
              <ArtifactViewer runId={report.run_id} artifacts={deliverables} failedParts={report.failed_parts} />
            </>
          )}
          {intermediates.length > 0 && (
            <>
              <div className="group-label">中间产物（已入历史，可随时找回）</div>
              <ArtifactViewer runId={report.run_id} artifacts={intermediates} />
            </>
          )}
        </div>
      ) : run.phase === 'failed' ? (
        <div className="card result">
          <div className="card-title">
            {run.runId ? '运行失败' : '无法开始'}
            {run.runId && run.failureHasRecord && (
              <span className="chips">
                <Link className="link" to={`/runs/${run.runId}`}>
                  查看记录 →
                </Link>
              </span>
            )}
            <button onClick={run.reset}>返回</button>
          </div>
          <div className="banner bad">{run.failure}</div>
          {run.streamText && <pre className="artifact-text">{run.streamText}</pre>}
        </div>
      ) : (
        <>
          <section className="card">
            <div className="card-title">
              模板
              <span className="hint">一键填充经典链（来自 README）</span>
            </div>
            <div className="templates">
              {TEMPLATES.map((template) => (
                <button
                  key={template.label}
                  className="template"
                  title={template.note}
                  onClick={() => setStages(template.stages())}
                >
                  {template.label}
                </button>
              ))}
            </div>
          </section>

          <section className="card">
            <div className="card-title">管线（材料喂给第 1 环，交付看末环）</div>
            <div className="pipeline">
              {stages.map((stage, index) => {
                const task = taskOf(stage.task);
                const junctionBad = badJunction === index + 1;
                return (
                  <div key={index} className="pipeline-slots">
                    {index > 0 && (
                      <div className={junctionBad ? 'junction bad' : 'junction'}>
                        {junctionBad ? '✕ 类型不符' : '→ text'}
                      </div>
                    )}
                    <div className="stage-card">
                      <div className="stage-head">
                        <span className="stage-no">{index + 1}</span>
                        <select value={stage.task} onChange={(e) => update(index, stageState(e.target.value))}>
                          {tasks.map((t) => (
                            <option key={t.name} value={t.name}>
                              {t.name}
                            </option>
                          ))}
                        </select>
                        <span className="chips">
                          {task && (
                            <>
                              <span className="chip">{(task.input_types ?? ['任意']).join('/')}</span>
                              <span className="chip">→ {task.output_types.join('/')}</span>
                            </>
                          )}
                        </span>
                        <span className="actions">
                          <button className="link" disabled={index === 0} onClick={() => move(index, -1)}>
                            ↑
                          </button>
                          <button
                            className="link"
                            disabled={index === stages.length - 1}
                            onClick={() => move(index, 1)}
                          >
                            ↓
                          </button>
                          <button
                            className="link"
                            disabled={stages.length <= 2}
                            onClick={() => setStages((s) => s.filter((_, i) => i !== index))}
                          >
                            删除
                          </button>
                        </span>
                      </div>
                      {stage.task === 'ask' || stage.prompt.trim() !== '' ? (
                        <div className="param param-wide">
                          <label>-p 指令</label>
                          <textarea
                            rows={2}
                            placeholder={stage.task === 'ask' ? '必填：这一环要做什么' : '可选'}
                            value={stage.prompt}
                            onChange={(e) => update(index, { prompt: e.target.value })}
                          />
                        </div>
                      ) : null}
                      {task && <ParamForm task={task} values={stage.params} onChange={(values) => update(index, { params: values })} />}
                      <div className="param-row">
                        <div className="param">
                          <label>--profile</label>
                          <input
                            list="chain-profiles"
                            placeholder="默认"
                            value={stage.profile}
                            onChange={(e) => update(index, { profile: e.target.value })}
                          />
                        </div>
                        {(task?.processor === 'ocr-tiles' ||
                          task?.processor === 'chunk-join' ||
                          task?.processor === 'chunk-reduce') && (
                          <label className="param-inline">
                            <input
                              type="checkbox"
                              checked={stage.noSplit}
                              onChange={(e) => update(index, { noSplit: e.target.checked })}
                            />
                            --no-split
                          </label>
                        )}
                      </div>
                    </div>
                  </div>
                );
              })}
              <div className="pipeline-slots">
                <div className="junction">＋</div>
                <button
                  className="stage-card add-stage"
                  onClick={() => setStages((s) => [...s, stageState('ask')])}
                >
                  加一环
                </button>
              </div>
            </div>
            <datalist id="chain-profiles">
              {profileNames.map((name) => (
                <option key={name} value={name} />
              ))}
            </datalist>
          </section>

          <section className="card">
            <div className="card-title">材料（第 1 环）</div>
            <Dropzone files={files} texts={texts} onFiles={setFiles} onTexts={setTexts} />
          </section>

          <section className="card actions-card">
            <div className="equivalent" title="等效命令（以预览为准）">
              <code>{command}</code>
            </div>
            <div className="actions">
              <button onClick={doPreview} disabled={run.starting}>
                预览（dry-run）
              </button>
              <button
                className="primary"
                disabled={run.starting}
                onClick={() => run.start(() => startChain(payload, files), '无法开始链')}
              >
                {run.starting ? '提交中……' : '运行链'}
              </button>
            </div>
          </section>

          {(previewText || previewError) && (
            <section className="card preview">
              <div className="card-title">执行计划</div>
              {previewError ? (
                <div className="plan-error">{previewError}</div>
              ) : (
                <pre>{previewText}</pre>
              )}
            </section>
          )}
        </>
      )}
    </div>
  );
}
