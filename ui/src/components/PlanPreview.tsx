import type { Preview } from '../types';

/** The dry-run the CLI prints, verbatim — the plan is the truth; this
 * card only frames it. */
export default function PlanPreview({
  preview,
  error,
}: {
  preview: Preview | null;
  error: string;
}) {
  if (error) {
    return (
      <div className="card preview error">
        <div className="card-title">执行计划</div>
        <div className="plan-error">{error}</div>
      </div>
    );
  }
  if (!preview) {
    return (
      <div className="card preview">
        <div className="card-title">执行计划</div>
        <div className="empty">点「预览」看 dry-run：切片/分块方案、Profile、凭据，一个请求都不发。</div>
      </div>
    );
  }
  return (
    <div className="card preview">
      <div className="card-title">
        执行计划
        <span className="chips">
          <span className="chip">{preview.task}</span>
          <span className="chip">{preview.model}</span>
          <span className="chip">{preview.steps.length} 个请求</span>
          {preview.credentials_available === false && <span className="chip danger">凭据未设置</span>}
        </span>
      </div>
      <pre>{preview.text}</pre>
    </div>
  );
}
