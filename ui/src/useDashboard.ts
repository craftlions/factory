import { useCallback, useEffect, useState } from 'react';
import {
  fetchOverview, fetchSamples, fetchSessions, subscribeSamples,
  type Overview, type Sample, type Session,
} from './api';

export const HISTORY_SECONDS = 60 * 60;
export const HISTORY_LABEL = 'over the last hour';

type Stream = 'connecting' | 'live' | 'reconnecting';
export type Connection = Stream | 'unavailable';

export interface Dashboard {
  overview: Overview | null;
  samples: Sample[];
  sessions: Session[];
  connection: Connection;
  /** Reloads sessions and host facts now instead of at the next interval. */
  refresh: () => void;
}

/** Appends samples newer than the tail of `prev`, then drops everything outside the history window. */
function mergeSamples(prev: Sample[], incoming: Sample[]): Sample[] {
  const lastTs = prev.length > 0 ? prev[prev.length - 1].ts : -Infinity;
  const merged = [...prev, ...incoming.filter((s) => s.ts > lastTs)];
  if (merged.length === 0) return merged;
  const cutoff = merged[merged.length - 1].ts - HISTORY_SECONDS;
  return merged.filter((s) => s.ts >= cutoff);
}

export function useDashboard(): Dashboard {
  const [overview, setOverview] = useState<Overview | null>(null);
  const [samples, setSamples] = useState<Sample[]>([]);
  const [sessions, setSessions] = useState<Session[]>([]);
  const [stream, setStream] = useState<Stream>('connecting');
  const [loadFailed, setLoadFailed] = useState(false);
  const [tick, setTick] = useState(0);
  const refresh = useCallback(() => setTick((n) => n + 1), []);

  // Sessions and host facts change rarely; refresh them on a slow cadence.
  useEffect(() => {
    const controller = new AbortController();
    async function load() {
      try {
        const [o, sess] = await Promise.all([
          fetchOverview(controller.signal),
          fetchSessions(controller.signal),
        ]);
        setOverview(o);
        setSessions(sess);
        setLoadFailed(false);
      } catch {
        if (!controller.signal.aborted) setLoadFailed(true);
      }
    }
    void load();
    const timer = setInterval(load, 60_000);
    return () => {
      controller.abort();
      clearInterval(timer);
    };
  }, [tick]);

  // Samples arrive over the stream; history is backfilled on every (re)connect.
  useEffect(() => {
    const controller = new AbortController();
    const unsubscribe = subscribeSamples({
      onOpen: () => {
        setStream('live');
        fetchSamples(HISTORY_SECONDS, controller.signal)
          .then((history) => setSamples((prev) => mergeSamples(history, prev)))
          .catch(() => {
            if (!controller.signal.aborted) setLoadFailed(true);
          });
      },
      onSample: (sample) => setSamples((prev) => mergeSamples(prev, [sample])),
      onError: () => setStream('reconnecting'),
    });
    return () => {
      controller.abort();
      unsubscribe();
    };
  }, []);

  const connection: Connection = loadFailed ? 'unavailable' : stream;
  return { overview, samples, sessions, connection, refresh };
}
