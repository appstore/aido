import { useEffect, useRef, useState } from 'react';
import { ApiError, cancelRun, getRun } from './api';
import { subscribe } from './sse';
import type { DoneFrame, Frame, RunReport } from './types';

export type Phase = 'idle' | 'running' | 'done' | 'failed';

export interface StepState {
  done: number;
  total: number;
  label: string;
  /** The input file this step belongs to — set in per-part batches. */
  part?: string | null;
}

/** The run lifecycle both pages share: submit (any 202-returning starter),
 * stream over SSE, cancel, and settle — with the late-subscriber
 * fallback the fast-failure race requires (the closing frame may be
 * gone before the subscription exists; the detail endpoint is the
 * truth, and "no record" must say so instead of hanging). */
export function useRunStream() {
  const [phase, setPhase] = useState<Phase>('idle');
  const [starting, setStarting] = useState(false);
  const [runId, setRunId] = useState('');
  const [streamText, setStreamText] = useState('');
  const [step, setStep] = useState<StepState | null>(null);
  const [warnings, setWarnings] = useState<string[]>([]);
  const [report, setReport] = useState<RunReport | null>(null);
  const [failure, setFailure] = useState('');
  // Cancelled runs are recorded; error-frame runs (a failed execute)
  // are not — the failed view only links to a record that exists.
  const [failureHasRecord, setFailureHasRecord] = useState(false);
  const closeStream = useRef<(() => void) | null>(null);

  useEffect(() => () => closeStream.current?.(), []);

  async function start(startFn: () => Promise<string>, submitError: string) {
    if (starting || phase === 'running') return;
    setStarting(true);
    setFailure('');
    setReport(null);
    setFailureHasRecord(false);
    setWarnings([]);
    setStreamText('');
    setStep(null);
    let settled = false;
    try {
      const id = await startFn();
      setRunId(id);
      setPhase('running');
      closeStream.current = subscribe(
        id,
        (frame: Frame | DoneFrame) => {
          if (frame.type === 'delta') {
            setStreamText((current) => current + frame.text);
          } else if (frame.type === 'step') {
            setStep({
              done: frame.done,
              total: frame.total,
              label: frame.label,
              part: frame.part ?? null,
            });
          } else if (frame.type === 'warning') {
            setWarnings((current) => [...current, frame.text]);
          } else if (frame.type === 'done') {
            settled = true;
            setReport(frame as DoneFrame);
            setPhase('done');
          } else if (frame.type === 'error') {
            settled = true;
            setFailure(`${frame.kind}: ${frame.message}`);
            setPhase('failed');
          } else if (frame.type === 'cancelled') {
            settled = true;
            setFailureHasRecord(true);
            setFailure('已取消。');
            setPhase('failed');
          }
        },
        () => {
          if (settled) return;
          settled = true;
          getRun(id)
            .then((record) => {
              setFailureHasRecord(true);
              setReport(record);
              if (record.error || record.status.status !== 'complete') {
                setFailure(record.error?.message ?? record.status.reason ?? '运行未完成。');
                setPhase('failed');
              } else {
                setPhase('done');
              }
            })
            .catch(() => {
              setFailure('运行中断，未留下记录（连接失败或服务未及应答）。');
              setPhase('failed');
            });
        },
      );
    } catch (e) {
      setFailure(e instanceof ApiError ? e.message : submitError);
      setPhase('failed');
    } finally {
      setStarting(false);
    }
  }

  async function cancel() {
    if (!runId) return;
    try {
      await cancelRun(runId);
    } catch {
      // The run already ended; the stream says which way.
    }
  }

  function reset() {
    closeStream.current?.();
    closeStream.current = null;
    setPhase('idle');
    setReport(null);
    setFailure('');
    setFailureHasRecord(false);
    setStreamText('');
    setStep(null);
    setWarnings([]);
    setRunId('');
  }

  return {
    phase,
    starting,
    runId,
    streamText,
    step,
    warnings,
    report,
    failure,
    failureHasRecord,
    start,
    cancel,
    reset,
  };
}
