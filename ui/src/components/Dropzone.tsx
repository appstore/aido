import { useEffect, useRef, useState } from 'react';

/** Material intake: drag files in, paste a screenshot from the
 * clipboard, or pick through the file dialog — the browser equivalent
 * of `aido ocr screenshot.png`'s material half. Text material rides
 * along as `--text`. */
export default function Dropzone({
  files,
  texts,
  onFiles,
  onTexts,
}: {
  files: File[];
  texts: string[];
  onFiles: (files: File[]) => void;
  onTexts: (texts: string[]) => void;
}) {
  const [dragging, setDragging] = useState(false);
  const [draft, setDraft] = useState('');
  const input = useRef<HTMLInputElement>(null);

  useEffect(() => {
    // Ctrl+V anywhere on the page is material — a pasted screenshot
    // lands exactly where a dragged one does.
    const onPaste = (event: ClipboardEvent) => {
      const pasted = Array.from(event.clipboardData?.files ?? []);
      if (pasted.length > 0) onFiles([...files, ...pasted]);
    };
    window.addEventListener('paste', onPaste);
    return () => window.removeEventListener('paste', onPaste);
  }, [files, onFiles]);

  return (
    <div className="dropzone-wrap">
      <div
        className={dragging ? 'dropzone dragging' : 'dropzone'}
        onDragOver={(e) => {
          e.preventDefault();
          setDragging(true);
        }}
        onDragLeave={() => setDragging(false)}
        onDrop={(e) => {
          e.preventDefault();
          setDragging(false);
          const dropped = Array.from(e.dataTransfer.files);
          if (dropped.length > 0) onFiles([...files, ...dropped]);
        }}
        onClick={() => input.current?.click()}
      >
        把文件拖到这里，或 Ctrl+V 粘贴截图（也可点击选择）
        <input
          ref={input}
          type="file"
          multiple
          hidden
          onChange={(e) => {
            const picked = Array.from(e.target.files ?? []);
            if (picked.length > 0) onFiles([...files, ...picked]);
            e.target.value = '';
          }}
        />
      </div>
      <div className="add-text">
        <textarea
          rows={2}
          placeholder="添加一段文字材料（--text）：术语表、说明、上下文……（Ctrl+Enter 收入）"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && (e.ctrlKey || e.metaKey) && draft.trim()) {
              onTexts([...texts, draft.trim()]);
              setDraft('');
            }
          }}
        />
      </div>
      {(files.length > 0 || texts.length > 0) && (
        <ul className="material-list">
          {files.map((file, index) => (
            <li key={`${file.name}-${index}`}>
              <span className="kind" data-kind={kindOf(file.type, file.name)}>
                {kindOf(file.type, file.name)}
              </span>
              <span className="name" title={file.name}>
                {file.name}
              </span>
              <span className="size">{prettySize(file.size)}</span>
              <button
                className="link"
                onClick={() => onFiles(files.filter((_, i) => i !== index))}
              >
                移除
              </button>
            </li>
          ))}
          {texts.map((text, index) => (
            <li key={`text-${index}`}>
              <span className="kind" data-kind="text">
                text
              </span>
              <span className="name">{text.slice(0, 60)}</span>
              <span className="size">{prettySize(text.length)}</span>
              <button className="link" onClick={() => onTexts(texts.filter((_, i) => i !== index))}>
                移除
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function kindOf(mime: string, name: string): string {
  if (mime.startsWith('image/')) return 'image';
  if (mime.startsWith('audio/')) return 'audio';
  if (mime === 'application/pdf' || name.toLowerCase().endsWith('.pdf')) return 'pdf';
  return 'text';
}

function prettySize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
}
