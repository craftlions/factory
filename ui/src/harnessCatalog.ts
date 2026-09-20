import { useCallback, useEffect, useState } from 'react';
import { fetchHarnessModels, fetchReasoningLevels, type HarnessModel } from './api';

export type CatalogState =
  | { status: 'idle' | 'loading'; models: [] }
  | { status: 'ready'; models: HarnessModel[] }
  | { status: 'error'; models: []; message: string };

/** Models a harness reports for this host. `harness` is null when its catalog is not served by the factory. */
export function useHarnessModels(harness: string | null): CatalogState & { retry: () => void } {
  const [state, setState] = useState<CatalogState>({ status: 'idle', models: [] });
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    if (!harness) {
      setState({ status: 'idle', models: [] });
      return;
    }
    const controller = new AbortController();
    setState({ status: 'loading', models: [] });
    fetchHarnessModels(harness, controller.signal).then(
      (models) => setState({ status: 'ready', models }),
      (error: unknown) => {
        if (!controller.signal.aborted) setState({ status: 'error', models: [], message: error instanceof Error ? error.message : String(error) });
      },
    );
    return () => controller.abort();
  }, [harness, attempt]);

  const retry = useCallback(() => setAttempt((n) => n + 1), []);
  return { ...state, retry };
}

export type LevelsState =
  | { status: 'idle' | 'loading' }
  | { status: 'ready'; levels: string[] }
  | { status: 'error'; message: string };

/** Asks the harness which reasoning levels one model supports. Pass null until a model is chosen. */
export function useReasoningLevels(harness: string | null, provider: string | null, model: string | null): LevelsState {
  const [state, setState] = useState<LevelsState>({ status: 'idle' });

  useEffect(() => {
    if (!harness || !provider || !model) {
      setState({ status: 'idle' });
      return;
    }
    const controller = new AbortController();
    setState({ status: 'loading' });
    fetchReasoningLevels(harness, provider, model, controller.signal).then(
      (levels) => setState({ status: 'ready', levels }),
      (error: unknown) => {
        if (!controller.signal.aborted) setState({ status: 'error', message: error instanceof Error ? error.message : String(error) });
      },
    );
    return () => controller.abort();
  }, [harness, provider, model]);

  return state;
}
