const CLASS: Record<string, string> = {
  complete: 'ok',
  partial: 'warn',
  incomplete: 'warn',
  failed: 'bad',
  cancelled: 'muted',
  running: 'muted',
  unreadable: 'bad',
};

const LABEL: Record<string, string> = {
  complete: '完整',
  partial: '部分失败',
  incomplete: '不完整',
  failed: '失败',
  cancelled: '已取消',
  running: '进行中',
  unreadable: '不可读',
};

export default function StatusBadge({ status }: { status: string }) {
  return <span className={`status ${CLASS[status] ?? 'muted'}`}>{LABEL[status] ?? status}</span>;
}
