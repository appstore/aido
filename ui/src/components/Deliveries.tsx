import type { RunReport } from '../types';

const KIND_LABEL: Record<string, string> = {
  directory: '目录',
  file: '文件',
  stdout: 'stdout',
  clipboard: '剪贴板',
};

/** The server-side delivery section: every destination the whitelist
 * named, with its real outcome — and the done frame's artifact→path
 * mapping, so "where did it land" is one glance. Absent when the run
 * delivered nothing (the browser is the destination). */
export default function Deliveries({ report }: { report: RunReport }) {
  const deliveries = report.deliveries ?? [];
  const saved = Object.entries(report.saved ?? {});
  if (deliveries.length === 0 && saved.length === 0) return null;
  return (
    <div className="delivery-box">
      <div className="group-label">交付</div>
      <table className="run-table">
        <tbody>
          {deliveries.map((d, i) => {
            const failed =
              typeof d.status === 'object' ? d.status.failed.error : null;
            return (
              <tr key={i}>
                <td>
                  <span className="chip">{KIND_LABEL[d.destination.type] ?? d.destination.type}</span>
                </td>
                <td className="delivery-path">
                  {d.destination.path ?? '—'}
                </td>
                <td>
                  {failed ? (
                    <span className="status bad">失败</span>
                  ) : d.status === 'succeeded' ? (
                    <span className="status ok">已交付</span>
                  ) : (
                    <span className="status muted">待定</span>
                  )}
                </td>
                {failed && (
                  <td className="muted-cell">
                    {failed}
                  </td>
                )}
              </tr>
            );
          })}
        </tbody>
      </table>
      {saved.length > 0 && (
        <div className="chips">
          {saved.map(([id, path]) => (
            <span key={id} className="chip part" title={path}>
              {id} → {path}
            </span>
          ))}
        </div>
      )}
    </div>
  );
}
