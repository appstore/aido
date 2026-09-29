import { useEffect, useRef } from 'react';

/** The run's terminal: merged deltas exactly as a live stdout would
 * print them, the step line the spinner would show, and the warnings
 * stderr would carry. */
export default function StreamView({
  text,
  step,
  warnings,
  onCancel,
}: {
  text: string;
  step: { done: number; total: number; label: string; part?: string | null } | null;
  warnings: string[];
  onCancel: () => void;
}) {
  const pre = useRef<HTMLPreElement>(null);
  const bottom = useRef<HTMLSpanElement>(null);
  useEffect(() => {
    const el = pre.current;
    if (!el) return;
    // Follow the stream only when the reader is already at the bottom —
    // a scroll up means they are reading, not watching.
    const near = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
    if (near) bottom.current?.scrollIntoView({ block: 'nearest' });
  }, [text]);

  return (
    <div className="card stream">
      <div className="card-title">
        运行中
        {step && (
          <span className="chips">
            <span className="chip">
              {step.done}/{step.total}
            </span>
            {step.part && <span className="chip part">{step.part}</span>}
            <span className="chip">{step.label}</span>
          </span>
        )}
        <button className="danger" onClick={onCancel}>
          取消
        </button>
      </div>
      <pre className="stream-text" ref={pre}>
        {text || '（等待第一个字符……）'}
        <span ref={bottom} />
      </pre>
      {warnings.length > 0 && (
        <ul className="warnings">
          {warnings.map((warning, index) => (
            <li key={index}>{warning}</li>
          ))}
        </ul>
      )}
    </div>
  );
}
