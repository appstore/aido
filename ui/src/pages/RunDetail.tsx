import { useEffect, useState } from 'react';
import { Link, useParams } from 'react-router-dom';
import { archiveUrl, getRun } from '../api';
import ArtifactViewer from '../components/ArtifactViewer';
import Deliveries from '../components/Deliveries';
import StatusBadge from '../components/StatusBadge';
import type { RunReport } from '../types';

/** One run's whole record: what was asked (summary), what came back
 * (artifacts, by provenance), what went wrong (warnings, failed parts)
 * — the same pages `history show` prints, pointable. */
export default function RunDetail() {
  const { id = '' } = useParams();
  const [report, setReport] = useState<RunReport | null>(null);
  const [error, setError] = useState('');

  useEffect(() => {
    let alive = true;
    getRun(id)
      .then((r) => alive && setReport(r))
      .catch((e) => alive && setError(e instanceof Error ? e.message : String(e)));
    return () => {
      alive = false;
    };
  }, [id]);

  if (error) {
    return (
      <div>
        <div className="banner bad">找不到这次运行：{error}</div>
        <Link className="link" to="/history">
          ← 历史
        </Link>
      </div>
    );
  }
  if (!report) return <div className="empty">加载记录……</div>;

  const summary = report.summary;
  return (
    <div className="detail-page">
      <div className="detail-head">
        <Link className="link" to="/history">
          ← 历史
        </Link>
        <h2>
          {report.task ?? '—'} <StatusBadge status={report.status.status} />
        </h2>
        <div className="hint">
          {report.run_id} · {report.created_at}
          {summary?.model ? ` · ${summary.model}` : ''}
          {summary?.profile ? ` · profile ${summary.profile}` : ''}
        </div>
        {report.status.reason && <div className="banner warn">{report.status.reason}</div>}
        {report.error && <div className="banner bad">{report.error.message}</div>}
      </div>

      {report.warnings.length > 0 && (
        <section className="card">
          <div className="card-title">警告</div>
          <ul className="warnings">
            {report.warnings.map((w, i) => (
              <li key={i}>{w}</li>
            ))}
          </ul>
        </section>
      )}

      {summary && summary.inputs.length > 0 && (
        <section className="card">
          <div className="card-title">材料</div>
          <ul className="material-list static">
            {summary.inputs.map((input, i) => (
              <li key={i}>
                <span className="kind" data-kind={input.kind}>
                  {input.kind}
                </span>
                <span className="name">{input.name}</span>
                <span className="size">{input.source}</span>
              </li>
            ))}
          </ul>
        </section>
      )}

      <section className="card">
        <div className="card-title">
          产物
          {report.parts_total > 0 && (
            <span className="hint">
              {' '}
              {report.parts_total - report.failed_parts.length}/{report.parts_total} 个文件成功
            </span>
          )}
          <a className="link" href={archiveUrl(report.run_id)} download>
            打包下载 .zip
          </a>
        </div>
        {report.artifacts.length === 0 && report.failed_parts.length === 0 ? (
          <div className="empty">这次运行没有留下产物。</div>
        ) : (
          <ArtifactViewer
            runId={report.run_id}
            artifacts={report.artifacts}
            failedParts={report.failed_parts}
          />
        )}
      </section>

      {(report.deliveries?.length ?? 0) > 0 && (
        <section className="card">
          <Deliveries report={report} />
        </section>
      )}

      <section className="card">
        <div className="card-title">来源时间线</div>
        {report.artifacts.length === 0 ? (
          <div className="empty">没有产物，也就没有来源。</div>
        ) : (
          <table className="run-table">
            <thead>
              <tr>
                <th>材料（按请求顺序）</th>
                <th />
                <th>产物</th>
                <th>来源</th>
              </tr>
            </thead>
            <tbody>
              {(summary?.inputs ?? []).map((input, i) => {
                const consumers = report.artifacts.filter((a) =>
                  a.provenance?.type === 'request'
                    ? a.provenance.index === i
                    : a.provenance?.type === 'merged' &&
                      a.provenance.requests?.includes(i),
                );
                return (
                  <tr key={i}>
                    <td>
                      <span className="chip">{`#${i}`}</span>{' '}
                      <span className="muted-cell">{input.kind}</span> {input.name}
                    </td>
                    <td className="muted-cell">──▶</td>
                    <td>
                      {consumers.length === 0 ? (
                        <span className="muted-cell">（未进入任何产物）</span>
                      ) : (
                        consumers.map((a) => (
                          <div key={a.id}>
                            {a.id} <span className="muted-cell">({a.kind})</span>
                          </div>
                        ))
                      )}
                    </td>
                    <td className="muted-cell">
                      {i === 0 ? provenanceNote(report.artifacts) : null}
                    </td>
                  </tr>
                );
              })}
              {report.artifacts
                .filter(
                  (a) =>
                    a.provenance?.type === 'restored' ||
                    (a.provenance === undefined && (summary?.inputs ?? []).length === 0),
                )
                .map((a) => (
                  <tr key={a.id}>
                    <td className="muted-cell">—</td>
                    <td className="muted-cell">──▶</td>
                    <td>
                      {a.id} <span className="muted-cell">({a.kind})</span>
                    </td>
                    <td className="muted-cell">恢复的历史产物</td>
                  </tr>
                ))}
            </tbody>
          </table>
        )}
      </section>
    </div>
  );
}

/** The one-line legend for provenance shapes the timeline can't draw as
 * arrows (merged/reduced results spanning several inputs). */
function provenanceNote(artifacts: RunReport['artifacts']): string | null {
  const merged = artifacts.filter((a) => a.provenance?.type === 'merged');
  const reduced = artifacts.length > 0 && artifacts.every((a) => a.provenance === undefined);
  if (merged.length > 0) {
    const parts = merged.map((a) => `${a.id} ← 合并 #${(a.provenance?.requests ?? []).join('+#')}`);
    return `另有合并产物：${parts.join('；')}`;
  }
  if (reduced) return '产物由全部材料归纳而来（reduce）。';
  return null;
}
