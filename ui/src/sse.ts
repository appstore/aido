import { token } from './api';
import type { DoneFrame, Frame, WatchFrame } from './types';

export type FrameHandler = (frame: Frame | DoneFrame) => void;
export type WatchFrameHandler = (frame: WatchFrame) => void;

/** Subscribe to a live run's progress. `onEnd` fires when the stream is
 * over for good — the server closed it after the closing frame, or the
 * browser gave up (a 404 on reconnect: the run is no longer live). It
 * fires AFTER a normal closing frame too, so the caller's contract is:
 * on terminal frame, stop caring; on `onEnd` without one, fall back to
 * the detail endpoint — the history is the truth. */
export function subscribe(
  runId: string,
  onFrame: FrameHandler,
  onEnd: () => void,
): () => void {
  return subscribePath<Frame | DoneFrame>(
    `/api/runs/${encodeURIComponent(runId)}/events`,
    onFrame,
    (frame) => frame.type === 'done' || frame.type === 'error' || frame.type === 'cancelled',
    onEnd,
  );
}

/** A watch daemon's live activity; ends with its `stopped` frame. */
export function subscribeWatch(
  id: string,
  onFrame: WatchFrameHandler,
  onEnd: () => void,
): () => void {
  return subscribePath<WatchFrame>(
    `/api/watches/${encodeURIComponent(id)}/events`,
    onFrame,
    (frame) => frame.type === 'stopped',
    onEnd,
  );
}

/** The path-level subscription both streams share: frames are JSON, a
 * true return from `isTerminal` ends the stream locally (no browser
 * reconnect into a 404), and only a CLOSED source counts as an error
 * ending — a blip retries. */
export function subscribePath<T>(
  path: string,
  onFrame: (frame: T) => void,
  isTerminal: (frame: T) => boolean,
  onEnd: () => void,
): () => void {
  const source = new EventSource(`${path}?t=${encodeURIComponent(token)}`);
  let ended = false;
  const end = () => {
    if (ended) return;
    ended = true;
    source.close();
    onEnd();
  };
  source.onmessage = (event) => {
    try {
      const frame = JSON.parse(event.data) as T;
      onFrame(frame);
      if (isTerminal(frame)) {
        // The server closes right after its closing frame; end locally
        // instead of letting the browser reconnect into a 404.
        ended = true;
        source.close();
      }
    } catch {
      // A malformed frame never kills the stream; the closing frame or
      // the detail endpoint is authoritative.
    }
  };
  source.onerror = () => {
    if (source.readyState === EventSource.CLOSED) end();
  };
  return () => {
    ended = true;
    source.close();
  };
}
