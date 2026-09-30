import { useEffect, useState } from 'react';
import { ApiError, getConfig, saveConfig } from '../api';
import type { ConfigView, SaveResult } from '../types';

type Tab = 'overview' | 'providers' | 'profiles' | 'toml';

/** The config page: parsed views over what runs resolve against, and a
 * TOML editor whose textarea IS the file — parse-first saves keep the
 * file on disk valid, check's issues sit next to the save button. */
export default function Config() {
  const [view, setView] = useState<ConfigView | null>(null);
  const [loadError, setLoadError] = useState('');
  const [toml, setToml] = useState('');
  const [dirty, setDirty] = useState(false);
  const [result, setResult] = useState<SaveResult | null>(null);
  const [saveError, setSaveError] = useState('');
  const [tab, setTab] = useState<Tab>('overview');

  useEffect(() => {
    getConfig()
      .then((v) => {
        setView(v);
        setToml(v.raw);
        setDirty(false);
      })
      .catch((e) => setLoadError(e instanceof ApiError ? e.message : String(e)));
  }, []);

  async function save() {
    setSaveError('');
    setResult(null);
    try {
      const saved = await saveConfig(toml);
      setResult(saved);
      setDirty(false);
      // Reload the parsed view: what runs resolve against just changed.
      setView(await getConfig());
    } catch (e) {
      setSaveError(e instanceof ApiError ? e.message : String(e));
    }
  }

  function reload() {
    if (dirty && !window.confirm('放弃未保存的修改，重新读取文件？')) return;
    getConfig()
      .then((v) => {
        setView(v);
        setToml(v.raw);
        setDirty(false);
        setResult(null);
        setSaveError('');
      })
      .catch((e) => setLoadError(e instanceof ApiError ? e.message : String(e)));
  }

  if (loadError) return <div className="banner bad">无法读取配置：{loadError}</div>;
  if (!view) return <div className="empty">加载配置……</div>;

  return (
    <div className="config-page">
      <div className="tabs">
        {(
          [
            ['overview', '概览'],
            ['providers', 'Providers'],
            ['profiles', 'Profiles'],
            ['toml', 'TOML 编辑'],
          ] as [Tab, string][]
        ).map(([key, label]) => (
          <button
            key={key}
            className={tab === key ? 'tab active' : 'tab'}
            onClick={() => setTab(key)}
          >
            {label}
          </button>
        ))}
        <button className="link" style={{ marginLeft: 'auto' }} onClick={reload}>
          重新读取
        </button>
      </div>

      {view.load_error && (
        <div className="banner bad">
          配置无法加载：{view.load_error}
          <br />
          在 TOML 编辑里修正并保存即可。
        </div>
      )}
      {view.issues.length > 0 && tab !== 'toml' && (
        <div className="banner warn">
          config check 发现 {view.issues.length} 个问题：
          <ul className="warnings">
            {view.issues.map((issue, i) => (
              <li key={i}>{issue}</li>
            ))}
          </ul>
        </div>
      )}

      {tab === 'overview' && (
        <>
          <section className="card">
            <div className="card-title">文件</div>
            <div className="kv">
              <span className="k">路径</span>
              <code>{view.path ?? '（此平台无法确定）'}</code>
            </div>
            <div className="kv">
              <span className="k">状态</span>
              {view.exists ? '存在' : '不存在（保存一次即创建）'}
            </div>
            <div className="kv">
              <span className="k">default_profile</span>
              {view.effective?.default_profile ?? '（未设置，解析为 "default"）'}
            </div>
          </section>
          {view.effective && (
            <section className="card">
              <div className="card-title">settings（生效值）</div>
              <table className="run-table">
                <thead>
                  <tr>
                    <th>键</th>
                    <th>值</th>
                  </tr>
                </thead>
                <tbody>
                  {Object.entries(view.effective.settings).map(([key, value]) => (
                    <tr key={key} onClick={undefined}>
                      <td>{key}</td>
                      <td>{value === null ? '默认' : String(value)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </section>
          )}
        </>
      )}

      {tab === 'providers' && (
        <section className="card">
          <div className="card-title">Providers（连接）</div>
          {view.effective && view.effective.providers.length > 0 ? (
            <table className="run-table">
              <thead>
                <tr>
                  <th>名称</th>
                  <th>base_url</th>
                  <th>api_key_env</th>
                  <th>路由</th>
                </tr>
              </thead>
              <tbody>
                {view.effective.providers.map((p) => (
                  <tr key={p.name}>
                    <td>{p.name}</td>
                    <td className="hint">{p.base_url ?? '—'}</td>
                    <td className="hint">{p.api_key_env ?? '—'}</td>
                    <td className="hint">
                      {Object.entries(p.routes)
                        .map(([op, adapter]) => `${op}→${adapter}`)
                        .join('，') || '全部默认'}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          ) : (
            <div className="empty">
              没有用户定义的 provider——运行时走内置 openai 回退。
            </div>
          )}
        </section>
      )}

      {tab === 'profiles' && (
        <section className="card">
          <div className="card-title">Profiles（模型选择）</div>
          {view.effective && view.effective.profiles.length > 0 ? (
            <table className="run-table">
              <thead>
                <tr>
                  <th>名称</th>
                  <th>provider</th>
                  <th>model</th>
                  <th>能力</th>
                </tr>
              </thead>
              <tbody>
                {view.effective.profiles.map((p) => (
                  <tr key={p.name}>
                    <td>
                      {p.name}
                      {p.is_default && <span className="chip"> 默认 </span>}
                    </td>
                    <td>{p.provider ?? '—'}</td>
                    <td>{p.model ?? '—'}</td>
                    <td className="hint">
                      {[
                        p.operations?.join('/') ?? null,
                        p.input_types ? `入 ${p.input_types.join('/')}` : null,
                        p.output_types ? `出 ${p.output_types.join('/')}` : null,
                      ]
                        .filter(Boolean)
                        .join(' · ') || '不限'}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          ) : (
            <div className="empty">
              没有用户定义的 profile——零配置时运行解析到内置 default
              profile。运行页的 --profile 下拉同样如此。
            </div>
          )}
        </section>
      )}

      {tab === 'toml' && (
        <section className="card">
          <div className="card-title">
            TOML 编辑
            {dirty && <span className="chip danger">未保存</span>}
            <button className="primary" onClick={save} disabled={!dirty}>
              保存
            </button>
          </div>
          <textarea
            className="toml-editor"
            spellCheck={false}
            value={toml}
            placeholder={'# 保存前先解析：写错只报 400，不动磁盘上的文件'}
            onChange={(e) => {
              setToml(e.target.value);
              setDirty(true);
            }}
          />
          {saveError && <div className="banner bad">{saveError}</div>}
          {result && (
            <div className={result.issues.length > 0 ? 'banner warn' : 'banner ok'}>
              已保存到 <code>{result.path}</code>
              {result.issues.length > 0 ? (
                <>
                  ，config check 提示：
                  <ul className="warnings">
                    {result.issues.map((issue, i) => (
                      <li key={i}>{issue}</li>
                    ))}
                  </ul>
                </>
              ): (
                '，检查全部通过。'
              )}
            </div>
          )}
        </section>
      )}
    </div>
  );
}
