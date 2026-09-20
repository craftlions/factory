import { useCallback, useEffect, useState } from 'react';

// OpenRouter's public model list. It needs no key and allows any origin, so
// the browser queries it directly. Each entry carries its reasoning support;
// there is no per-model endpoint that returns it.
const MODELS_URL = 'https://openrouter.ai/api/v1/models';

export interface OpenRouterModel {
  id: string;
  name: string;
  context_length: number | null;
  /** null when the model does not reason at all. */
  reasoning: {
    mandatory?: boolean;
    default_enabled?: boolean;
    supported_efforts?: string[];
    default_effort?: string;
  } | null;
}

export interface ReasoningLevels {
  levels: string[];
  initial: string;
  note: string;
}

const EFFORT_ORDER = ['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'];

/** Reasoning levels OpenRouter reports for one model, weakest first. */
export function reasoningLevels(model: OpenRouterModel): ReasoningLevels {
  const r = model.reasoning;
  if (!r) return { levels: ['none'], initial: 'none', note: 'OpenRouter lists no reasoning support for this model.' };

  const efforts = r.supported_efforts;
  if (!efforts || efforts.length === 0) {
    return r.mandatory
      ? { levels: ['enabled'], initial: 'enabled', note: 'This model always reasons and has no effort levels.' }
      : {
          levels: ['none', 'enabled'],
          initial: r.default_enabled === false ? 'none' : 'enabled',
          note: 'This model can reason but has no effort levels.',
        };
  }

  const levels = [...new Set(r.mandatory || efforts.includes('none') ? efforts : ['none', ...efforts])];
  const rank = (e: string) => (EFFORT_ORDER.includes(e) ? EFFORT_ORDER.indexOf(e) : EFFORT_ORDER.length);
  levels.sort((a, b) => rank(a) - rank(b));
  const preferred = r.default_enabled === false ? 'none' : r.default_effort;
  return {
    levels,
    initial: preferred && levels.includes(preferred) ? preferred : levels[levels.length - 1],
    note: r.mandatory ? 'Reasoning is mandatory for this model.' : 'Effort levels reported by OpenRouter for this model.',
  };
}

// Only an in-flight request is shared; once it settles the next caller asks
// OpenRouter again, so the list is current every time the picker opens.
let inflight: Promise<OpenRouterModel[]> | null = null;

function loadModels(): Promise<OpenRouterModel[]> {
  inflight ??= fetch(MODELS_URL, { cache: 'no-cache' })
    .then((res) => {
      if (!res.ok) throw new Error(`OpenRouter answered ${res.status}`);
      return res.json() as Promise<{ data: OpenRouterModel[] }>;
    })
    .then((body) => body.data)
    .finally(() => { inflight = null; });
  return inflight;
}

export type ModelsState =
  | { status: 'idle' | 'loading'; models: [] }
  | { status: 'ready'; models: OpenRouterModel[] }
  | { status: 'error'; models: []; message: string };

export function useOpenRouterModels(enabled: boolean): ModelsState & { retry: () => void } {
  const [state, setState] = useState<ModelsState>({ status: 'idle', models: [] });
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    if (!enabled) return;
    let live = true;
    setState({ status: 'loading', models: [] });
    loadModels().then(
      (models) => { if (live) setState({ status: 'ready', models }); },
      (error: unknown) => {
        if (live) setState({ status: 'error', models: [], message: error instanceof Error ? error.message : String(error) });
      },
    );
    return () => { live = false; };
  }, [enabled, attempt]);

  const retry = useCallback(() => setAttempt((n) => n + 1), []);
  return { ...state, retry };
}
