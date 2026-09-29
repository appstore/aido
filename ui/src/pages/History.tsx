import { useCallback, useEffect, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { listRuns } from '../api';
import StatusBadge from '../components/StatusBadge';
import type { RunRow } from '../types';

/** The run list, newest first; `seq` is the CLI's own numbering, so a
 * row here and `history show N` in a terminal address the same run. */
export default function History() {
  const navigate = useNavigate();
  const [rows, setRows] = useState<RunRow[] | null>(null);
  const [error, setError] = useState('');
  const [task, setTask] = useState('');
  const [status, setStatus] = useState('');

  const refresh = useCallback(async () => {
    try {
      setRows(await listRuns({ task: task.trim() || undefined, status: status || undefined }));
      setError('');
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, [task, status]);

  useEffect(() => {
    refresh();
    // Poll while the tab is visible: runs land here from the run page,
    // the terminal, and watch daemons alike.
    const timer = window.setInterval(() => {
      if (document.visibilityState === 'visible') refresh();
    }, 5000);
    return () => window.clearInterval(timer);
  }, [refresh]);

  return (
    <div className="history-page">
      <div className="filters">
        <input
          placeholder="按任务过滤（如 ocr）"
          value={task}
          onChange={(e) => setTask(e.target.value)}
        />
        <select value={status} onChange={(e) => setStatus(e.target.value)}>
          <option value="">全部状态</option>
          <option value="complete">完整</option>
          <option value="partial">部分失败</option>
          <option value="incomplete">不完整</option>
          <option value="failed">失败</option>
          <option value="cancelled">已取消</option>
        </select>
      </div>
      {error && <div className="banner bad">{error}</div>}
      {!rows ? (
        <div className="empty">加载历史……</div>
      ) : rows.length === 0 ? (
        <div className="empty">还没有运行记录。</div>
      ) : (
        <table className="run-table">
          <thead>
            <tr>
              <th>#</th>
              <th>任务</th>
              <th>状态</th>
              <th>产物</th>
              <th>时间</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((row) => (
              <tr key={row.run_id} onClick={() => navigate(`/runs/${row.run_id}`)}>
                <td>{row.seq}</td>
                <td>{row.task ?? '—'}</td>
                <td>
                  <StatusBadge status={row.status} />
                  {row.failed_parts > 0 && (
                    <span className="hint">
                      {' '}
                      {row.failed_parts}/{Math.max(row.parts_total, row.failed_parts)} 失败
                    </span>
                  )}
                </td>
                <td>
                  {row.artifacts}
                  {row.warnings > 0 && <span className="hint">（{row.warnings} 警告）</span>}
                </td>
                <td className="hint">{row.created_at || row.run_id}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}
