import { useEffect, useState } from 'react';
import { artifactUrl } from '../api';
import type { Artifact } from '../types';

/** Render one run's artifacts by kind: text as a copyable block, images
 * inline, audio with a player. A batch renders as a grid — a failed
 * part shows as its own red cell from failed_parts. */
export default function ArtifactViewer({
  runId,
  artifacts,
  failedParts = [],
}: {
  runId: string;
  artifacts: Artifact[];
  failedParts?: { part: string; error: string }[];
}) {
  const grid = artifacts.length > 1 || failedParts.length > 0;
  return (
    <div className={grid ? 'artifact-grid' : 'artifact-list'}>
      {artifacts.map((artifact) => (
        <ArtifactCell key={artifact.id} runId={runId} artifact={artifact} />
      ))}
      {failedParts.map((part) => (
        <div key={part.part} className="artifact-cell failed">
          <div className="artifact-name">{part.part}</div>
          <div className="artifact-error">{part.error}</div>
        </div>
      ))}
    </div>
  );
}

function ArtifactCell({ runId, artifact }: { runId: string; artifact: Artifact }) {
  const [text, setText] = useState<string | null>(null);
  const [showRaw, setShowRaw] = useState(false);

  useEffect(() => {
    if (artifact.kind !== 'text') return;
    let alive = true;
    fetch(artifactUrl(runId, artifact.id))
      .then((r) => r.text())
      .then((body) => alive && setText(body))
      .catch(() => alive && setText('（无法读取产物）'));
    return () => {
      alive = false;
    };
  }, [runId, artifact.id, artifact.kind]);

  return (
    <div className="artifact-cell">
      <div className="artifact-head">
        <span className="artifact-name" title={artifact.id}>
          {artifact.id}
        </span>
        <span className="chip">{artifact.kind}</span>
        <span className="size">{prettySize(artifact.size)}</span>
        <span className="actions">
          {artifact.kind === 'text' && text !== null && (
            <button className="link" onClick={() => navigator.clipboard.writeText(text)}>
              复制
            </button>
          )}
          <a className="link" href={artifactUrl(runId, artifact.id)} download={artifact.id}>
            下载
          </a>
        </span>
      </div>
      {artifact.kind === 'text' ? (
        <pre className="artifact-text">{text ?? '（读取中……）'}</pre>
      ) : artifact.kind === 'image' ? (
        <img src={artifactUrl(runId, artifact.id)} alt={artifact.id} />
      ) : (
        <audio controls src={artifactUrl(runId, artifact.id)} />
      )}
      {artifact.kind === 'text' && (
        <button className="link" onClick={() => setShowRaw(!showRaw)}>
          {showRaw ? '收起字节信息' : `格式 ${artifact.format} · ${artifact.mime}`}
        </button>
      )}
      {showRaw && (
        <div className="artifact-meta">
          provenance: {JSON.stringify(artifact.provenance ?? {})}
        </div>
      )}
    </div>
  );
}

function prettySize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
}
