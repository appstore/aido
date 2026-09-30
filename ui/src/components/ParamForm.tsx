import type { ParamSpec, Task } from '../types';

/** Values as strings while editing; the run payload converts and drops
 * empties, so the server's own validation stays the only judge. */
export type ParamValues = Record<string, string>;

/** The task's declared parameters, each rendered by its descriptor —
 * the same bounds the plan enforces, so the form cannot offer what a
 * run would reject. */
export default function ParamForm({
  task,
  values,
  onChange,
}: {
  task: Task;
  values: ParamValues;
  onChange: (values: ParamValues) => void;
}) {
  return (
    <div className="param-form">
      {task.params.map((spec) => control(spec))}
      {task.params.length === 0 && <div className="empty">这个任务没有专属参数。</div>}
    </div>
  );

  function set(name: string, value: string) {
    onChange({ ...values, [name]: value });
  }

  function control(spec: ParamSpec) {
    const value = values[spec.name] ?? String(spec.default ?? '');
    const label = <label htmlFor={`param-${spec.name}`}>--{spec.name}</label>;
    if (spec.kind === 'enum') {
      return (
        <div className="param" key={spec.name}>
          {label}
          <select
            id={`param-${spec.name}`}
            value={value}
            onChange={(e) => set(spec.name, e.target.value)}
          >
            {(spec.choices ?? []).map((choice) => (
              <option key={choice} value={choice}>
                {choice}
              </option>
            ))}
          </select>
        </div>
      );
    }
    if (spec.kind === 'number' || spec.kind === 'integer') {
      return (
        <div className="param" key={spec.name}>
          {label}
          <input
            id={`param-${spec.name}`}
            type="number"
            step={spec.kind === 'integer' ? 1 : 0.05}
            min={spec.min}
            max={spec.max}
            value={value}
            placeholder="默认"
            onChange={(e) => set(spec.name, e.target.value)}
          />
        </div>
      );
    }
    if (spec.kind === 'language') {
      return (
        <div className="param" key={spec.name}>
          {label}
          <input
            id={`param-${spec.name}`}
            list="languages"
            value={value}
            placeholder={String(spec.default ?? 'auto')}
            onChange={(e) => set(spec.name, e.target.value)}
          />
          <datalist id="languages">
            {['auto', 'zh-CN', 'en', 'ja', 'ko', 'fr', 'de', 'es', 'ru'].map((lang) => (
              <option key={lang} value={lang} />
            ))}
          </datalist>
        </div>
      );
    }
    return (
      <div className="param" key={spec.name}>
        {label}
        <input
          id={`param-${spec.name}`}
          value={value}
          placeholder="默认"
          onChange={(e) => set(spec.name, e.target.value)}
        />
      </div>
    );
  }
}
