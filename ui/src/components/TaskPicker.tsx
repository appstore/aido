import { useState } from 'react';
import type { Task } from '../types';

const KIND_LABEL: Record<string, string> = {
  ocr: '图片 → 文本',
  translate: '文本 → 译文',
  summarize: '长文 → 摘要',
  'code-review': 'diff → 审查',
  tts: '文本 → 语音',
  transcribe: '音频 → 文本',
  image: '文本 → 图片',
  ask: '任意材料 + 指令',
};

/** The task cards: one line of what it does, badges for the contract
 * bits the run form will respect (per-part batches, the processor). */
export default function TaskPicker({
  tasks,
  selected,
  onSelect,
}: {
  tasks: Task[];
  selected: string;
  onSelect: (name: string) => void;
}) {
  const [expanded, setExpanded] = useState<string | null>(null);
  const builtins = tasks.filter((t) => t.builtin);
  const customs = tasks.filter((t) => !t.builtin);
  return (
    <div className="task-picker">
      <div className="task-group">
        {builtins.map((task) => card(task))}
      </div>
      {customs.length > 0 && (
        <>
          <div className="group-label">自定义任务</div>
          <div className="task-group">{customs.map((task) => card(task))}</div>
        </>
      )}
    </div>
  );

  function card(task: Task) {
    const isOpen = expanded === task.name;
    return (
      <div
        key={task.name}
        className={task.name === selected ? 'task-card selected' : 'task-card'}
        onClick={() => onSelect(task.name)}
      >
        <div className="task-head">
          <span className="task-name">{task.name}</span>
          {task.per_part && <span className="badge">逐文件</span>}
          {task.processor !== 'single' && task.processor !== 'ocr-tiles' && (
            <span className="badge">{task.processor}</span>
          )}
        </div>
        <div className="task-summary">{KIND_LABEL[task.name] ?? task.summary}</div>
        <button
          className="link"
          onClick={(e) => {
            e.stopPropagation();
            setExpanded(isOpen ? null : task.name);
          }}
        >
          {isOpen ? '收起契约' : '契约'}
        </button>
        {isOpen && (
          <dl className="task-contract">
            <dt>operation</dt>
            <dd>{task.operation}</dd>
            <dt>输入</dt>
            <dd>{(task.input_types ?? task.required_types).join(' / ') || '任意'}</dd>
            <dt>输出</dt>
            <dd>{task.output_types.join(' / ')}</dd>
            <dt>指令</dt>
            <dd>{task.instruction.trim().split('\n')[0]}</dd>
          </dl>
        )}
      </div>
    );
  }
}
