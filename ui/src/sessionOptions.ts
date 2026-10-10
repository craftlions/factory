// Catalog behind the new-session questionnaire. Nothing here talks to the
// server yet; edit these tables to change what the questionnaire offers.

// pi, Hermes and Codex are left out for now; Claude Code comes first.
export type HarnessId = 'claude';
// Running a harness directly on the host is not offered: harnesses execute
// shell commands without asking, so the factory only provides isolated paths.
export type IsolationId = 'backend' | 'harness';
export type WorkdirKind = 'local' | 'git' | 'empty';

export interface Provider {
  id: string;
  name: string;
  /** Suggestions only; the model field accepts any id. */
  models: string[];
}

export interface Harness {
  id: HarnessId;
  name: string;
  /** Short mark shown on the harness card. */
  mark: string;
  /** Accent hue in degrees, used for the card color. */
  hue: number;
  description: string;
  /** Backend isolation needs a harness that can route tool calls elsewhere. */
  isolation: IsolationId[];
  /** Fixed provider ids. Empty when `catalog` is 'server'. */
  providers: string[];
  /** 'server': the factory asks the harness itself for providers, models and reasoning levels. */
  catalog?: 'server';
}

export const PROVIDERS: Record<string, Provider> = {
  anthropic: {
    id: 'anthropic',
    name: 'Anthropic',
    models: ['claude-fable-5-1', 'claude-opus-5', 'claude-sonnet-5', 'claude-haiku-4-5-20251001'],
  },
  openai: { id: 'openai', name: 'OpenAI', models: [] },
  google: { id: 'google', name: 'Google', models: [] },
  openrouter: { id: 'openrouter', name: 'OpenRouter', models: [] },
  nous: { id: 'nous', name: 'Nous Portal', models: [] },
};

export const HARNESSES: Harness[] = [
  {
    id: 'claude',
    mark: 'Cl',
    hue: 40,
    name: 'Claude Code',
    description: 'Anthropic’s coding agent.',
    isolation: ['harness'],
    providers: [],
    catalog: 'server',
  },
];

export const ISOLATION: Record<IsolationId, { name: string; description: string }> = {
  backend: {
    name: 'Backend isolation',
    description: 'Harness runs on the host; tool calls, the terminal and file access run in an isolated backend.',
  },
  harness: {
    name: 'Harness isolation',
    description: 'The whole harness runs inside its own microvm.',
  },
};

export const ISOLATION_ORDER: IsolationId[] = ['backend', 'harness'];

export const WORKDIR: Record<WorkdirKind, { name: string; description: string }> = {
  local: { name: 'Local folder', description: 'An existing folder on the host.' },
  git: { name: 'Git repository', description: 'Cloned fresh for this session.' },
  empty: { name: 'Empty directory', description: 'A new scratch directory.' },
};

/** Generic levels. OpenRouter models replace these with their own, see openrouter.ts. */
export const REASONING = ['off', 'low', 'medium', 'high', 'max'];
export const DEFAULT_REASONING = 'medium';

export type NetworkAccess = 'restricted' | 'public';

export const NETWORK: Record<NetworkAccess, { name: string; description: string }> = {
  restricted: { name: 'Model API only', description: 'The session reaches the model’s API and nothing else.' },
  public: {
    name: 'Public internet',
    description: 'Also any public host on the ports you choose. The host’s own and private networks stay unreachable.',
  },
};

/** Ports offered as switches; others are typed in. */
export const COMMON_PORTS: { port: number; name: string }[] = [
  { port: 443, name: 'HTTPS' },
  { port: 80, name: 'HTTP' },
];

export interface NetworkDraft {
  access: NetworkAccess;
  ports: number[];
  allPorts: boolean;
  /** More ports, comma or space separated. */
  extra: string;
}

/** The typed ports, or null when one of them is not a port. */
export function extraPorts(extra: string): number[] | null {
  const parts = extra.split(/[\s,]+/).filter(Boolean);
  const ports = parts.map(Number);
  return ports.every((p) => Number.isInteger(p) && p >= 1 && p <= 65535) ? ports : null;
}

/** All chosen ports, or null when every port is allowed or a typed one is invalid. */
export function chosenPorts(network: NetworkDraft): number[] | null {
  if (network.allPorts) return null;
  const extra = extraPorts(network.extra);
  return extra && [...new Set([...network.ports, ...extra])];
}

export function networkValid(network: NetworkDraft): boolean {
  if (network.access === 'restricted' || network.allPorts) return true;
  const ports = chosenPorts(network);
  return ports !== null && ports.length > 0;
}

export function describeNetwork(network: NetworkDraft): string {
  if (network.access === 'restricted') return NETWORK.restricted.name;
  return network.allPorts ? 'Public internet, all ports' : `Public internet, ports ${chosenPorts(network)?.join(', ')}`;
}

export interface SessionDraft {
  harness: HarnessId | null;
  isolation: IsolationId | null;
  workdir: { kind: WorkdirKind; path: string; url: string; ref: string };
  provider: string | null;
  model: string;
  reasoning: string;
  network: NetworkDraft;
}

export const EMPTY_DRAFT: SessionDraft = {
  harness: null,
  isolation: null,
  workdir: { kind: 'local', path: '', url: '', ref: '' },
  provider: null,
  model: '',
  reasoning: DEFAULT_REASONING,
  network: { access: 'restricted', ports: [443, 80], allPorts: false, extra: '' },
};

export function providerName(id: string): string {
  return PROVIDERS[id]?.name ?? id;
}

/** Harnesses the factory can start. */
const IMPLEMENTED: HarnessId[] = ['claude'];

/** Why this draft cannot be created yet, or null when the factory can start it. */
export function notCreatable(draft: SessionDraft): string | null {
  if (!draft.harness || !IMPLEMENTED.includes(draft.harness)) return `The ${harnessOf(draft)?.name ?? 'chosen'} harness is not implemented yet.`;
  if (draft.isolation !== 'harness') return `${draft.isolation ? ISOLATION[draft.isolation].name : 'This isolation level'} is not implemented yet.`;
  if (draft.workdir.kind !== 'empty') return `The working directory kind “${WORKDIR[draft.workdir.kind].name}” is not implemented yet.`;
  return null;
}

export function harnessOf(draft: SessionDraft): Harness | undefined {
  return HARNESSES.find((h) => h.id === draft.harness);
}

/** Picking a harness drops answers the new harness does not support. */
export function withHarness(draft: SessionDraft, id: HarnessId): SessionDraft {
  const harness = HARNESSES.find((h) => h.id === id)!;
  const isolation = draft.isolation && harness.isolation.includes(draft.isolation) ? draft.isolation : null;
  const keep = draft.provider !== null && harness.providers.includes(draft.provider);
  const provider = keep ? draft.provider : harness.providers.length === 1 ? harness.providers[0] : null;
  return {
    ...draft, harness: id, isolation, provider,
    model: keep ? draft.model : '',
    reasoning: keep ? draft.reasoning : DEFAULT_REASONING,
  };
}

export function describeWorkdir(w: SessionDraft['workdir']): string {
  if (w.kind === 'local') return w.path.trim();
  if (w.kind === 'git') return w.ref.trim() ? `${w.url.trim()} @ ${w.ref.trim()}` : w.url.trim();
  return 'new empty directory';
}

/** The request the UI would send once session creation exists. */
export function buildRequest(draft: SessionDraft) {
  const w = draft.workdir;
  const workdir =
    w.kind === 'local' ? { kind: 'local', path: w.path.trim() }
    : w.kind === 'git' ? { kind: 'git', url: w.url.trim(), ref: w.ref.trim() || null }
    : { kind: 'empty' };
  return {
    harness: draft.harness,
    isolation: draft.isolation,
    workdir,
    model: { provider: draft.provider, id: draft.model.trim(), reasoning: draft.reasoning },
    network: draft.network.access === 'restricted'
      ? { access: 'restricted' }
      : { access: 'public', ports: chosenPorts(draft.network) },
  };
}

/** Plain-language steps the server would take for this draft. */
export function buildPlan(draft: SessionDraft): string[] {
  const harness = harnessOf(draft);
  if (!harness || !draft.isolation || !draft.provider) return [];
  const w = draft.workdir;
  const inVm = draft.isolation === 'harness';
  const steps: string[] = [];

  if (inVm) {
    steps.push('Boot a Firecracker microvm with its own disk and no network device.');
    steps.push(draft.network.access === 'restricted'
      ? 'Allow outbound connections only to the model’s API host and the hosts tool setup needs.'
      : `Allow outbound connections to the model’s API host and to public hosts on ${draft.network.allPorts ? 'any port' : `ports ${chosenPorts(draft.network)?.join(', ')}`}; private networks stay blocked.`);
  }
  if (draft.isolation === 'backend') steps.push('Start an isolated backend for tool calls and the terminal.');

  const where = inVm ? 'inside the microvm' : 'inside the backend';
  if (w.kind === 'local') {
    steps.push(`Mount ${w.path.trim()} ${where}.`);
  } else if (w.kind === 'git') {
    steps.push(`Clone ${w.url.trim()}${w.ref.trim() ? ` at ${w.ref.trim()}` : ''} ${where}.`);
  } else {
    steps.push(`Create an empty working directory ${where}.`);
  }

  if (inVm) steps.push('Install the harness with mise inside the microvm.');
  steps.push(`Launch ${harness.name} ${inVm ? 'inside the microvm' : 'on the host'}${inVm ? '' : ', with tools routed to the backend'}.`);
  steps.push(`Configure ${providerName(draft.provider)} model ${draft.model.trim()} with reasoning ${draft.reasoning}.`);
  steps.push('Record the session as running.');
  if (inVm) steps.push('When it ends, copy the conversation and the workspace out of the session’s disk.');
  return steps;
}
