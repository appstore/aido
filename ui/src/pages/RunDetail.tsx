import { useEffect, useState } from 'react';
import { Link, useParams } from 'react-router-dom';
import { getRun } from '../api';
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
    </div>
  );
}
