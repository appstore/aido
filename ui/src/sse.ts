import { token } from './api';
import type { DoneFrame, Frame } from './types';

export type FrameHandler = (frame: Frame | DoneFrame) => void;

/** Subscribe to a live run's progress. The stream ends when the run
 * does (its closing frame is `done`, `error` or `cancelled`); the
 * returned closer detaches early — leaving the page, or a run that is
 * already over (404: the detail endpoint tells the rest). */
export function subscribe(runId: string, onFrame: FrameHandler): () => void {
  const source = new EventSource(
    `/api/runs/${encodeURIComponent(runId)}/events?t=${encodeURIComponent(token)}`,
  );
  source.onmessage = (event) => {
    try {
      onFrame(JSON.parse(event.data));
    } catch {
      // A malformed frame never kills the stream; the closing frame or
      // the detail endpoint is authoritative.
    }
  };
  return () => source.close();
}
