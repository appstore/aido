import { token } from './api';
import type { DoneFrame, Frame } from './types';

export type FrameHandler = (frame: Frame | DoneFrame) => void;

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
  const source = new EventSource(
    `/api/runs/${encodeURIComponent(runId)}/events?t=${encodeURIComponent(token)}`,
  );
  let ended = false;
  const end = () => {
    if (ended) return;
    ended = true;
    source.close();
    onEnd();
  };
  source.onmessage = (event) => {
    try {
      const frame = JSON.parse(event.data) as Frame | DoneFrame;
      onFrame(frame);
      if (frame.type === 'done' || frame.type === 'error' || frame.type === 'cancelled') {
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
    // Fatal (e.g. 404 on reconnect: CLOSED) or a blip (CONNECTING, the
    // browser retries). Only a closed source is the end.
    if (source.readyState === EventSource.CLOSED) end();
  };
  return () => {
    ended = true;
    source.close();
  };
}
