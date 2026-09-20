import { useEffect, useState, type CSSProperties, type FormEvent, type ReactNode } from 'react';
import { ArrowIcon, CheckIcon, CopyIcon, FolderIcon, GitIcon, SparkIcon } from '../icons';
import { createSession } from '../api';
import { useHarnessModels, useReasoningLevels } from '../harnessCatalog';
import { reasoningLevels, useOpenRouterModels } from '../openrouter';
import { Link, navigate } from '../router';
import {
  DEFAULT_REASONING, EMPTY_DRAFT, HARNESSES, ISOLATION, ISOLATION_ORDER, PROVIDERS, REASONING, WORKDIR,
  buildPlan, buildRequest, describeWorkdir, harnessOf, notCreatable, providerName, withHarness,
  type IsolationId, type SessionDraft, type WorkdirKind,
} from '../sessionOptions';

const STEPS = ['Harness', 'Isolation', 'Workspace', 'Model'];
const REVIEW = STEPS.length;
const WORKDIR_ICON: Record<WorkdirKind, ReactNode> = { local: <FolderIcon />, git: <GitIcon />, empty: <SparkIcon /> };

function stepValid(step: number, draft: SessionDraft): boolean {
  const w = draft.workdir;
  switch (step) {
    case 0: return draft.harness !== null;
    case 1: return draft.isolation !== null;
    case 2: return w.kind === 'empty' || (w.kind === 'local' ? w.path.trim() !== '' : w.url.trim() !== '');
    case 3: return draft.provider !== null && draft.model.trim() !== '';
    default: return true;
  }
}

/** A step opens once every step before it has an answer. */
function canReach(step: number, valid: (step: number) => boolean): boolean {
  for (let i = 0; i < step; i++) if (!valid(i)) return false;
  return true;
}

function Card({ name, checked, disabled, onSelect, title, visual, hue, children }: {
  name: string; checked: boolean; disabled?: boolean; onSelect: () => void;
  title: string; visual?: ReactNode; hue?: number; children?: ReactNode;
}) {
  const style = hue === undefined ? undefined : ({ '--hue': hue } as CSSProperties);
  return (
    <label className={`card${disabled ? ' disabled' : ''}${hue === undefined ? '' : ' hued'}`} style={style}>
      <input type="radio" name={name} checked={checked} disabled={disabled} onChange={onSelect} />
      {visual}
      <span className="card-text">
        <strong>{title}</strong>
        {children && <span className="detail">{children}</span>}
      </span>
      <span className="card-check"><CheckIcon /></span>
    </label>
  );
}

/** Host box with the harness and its tools; the isolated part is outlined. */
function IsolationDiagram({ level }: { level: IsolationId }) {
  return (
    <span className={`iso iso-${level}`} aria-hidden="true">
      <span className="iso-tag">host</span>
      <span className="iso-vm" data-tag="microvm">
        <span className="iso-chip">harness</span>
        <span className="iso-backend" data-tag="backend">
          <span className="iso-chip">tools</span>
        </span>
      </span>
    </span>
  );
}

function highlight(json: string): ReactNode[] {
  const re = /("(?:\\.|[^"\\])*")(\s*:)?|\b(?:true|false|null)\b|-?\d+(?:\.\d+)?/g;
  const out: ReactNode[] = [];
  let last = 0;
  for (let m = re.exec(json); m; m = re.exec(json)) {
    if (m.index > last) out.push(json.slice(last, m.index));
    if (m[1]) {
      out.push(<span key={m.index} className={m[2] ? 'tok-key' : 'tok-str'}>{m[1]}</span>);
      if (m[2]) out.push(m[2]);
    } else {
      out.push(<span key={m.index} className="tok-lit">{m[0]}</span>);
    }
    last = re.lastIndex;
  }
  out.push(json.slice(last));
  return out;
}

function formatContext(tokens: number | null): string {
  if (!tokens) return '';
  return tokens >= 1_000_000 ? `${+(tokens / 1_000_000).toFixed(1)}M ctx` : `${Math.round(tokens / 1000)}K ctx`;
}

const MAX_RESULTS = 60;

interface PickerModel { id: string; name: string; context: number | null; reasoning: boolean }

/**
 * Search box over a live model list. With `strict` the model must come from
 * the list; otherwise free text still works if the list fails to load.
 */
function ModelPicker({ source, status, models, message, retry, strict, value, onChange }: {
  source: string; status: 'idle' | 'loading' | 'ready' | 'error'; models: PickerModel[]; message?: string;
  retry: () => void; strict?: boolean; value: string; onChange: (model: string) => void;
}) {
  const query = value.trim().toLowerCase();
  const exact = models.some((m) => m.id === value.trim());
  const matches = exact
    ? models.filter((m) => m.id === value.trim())
    : models.filter((m) => m.id.toLowerCase().includes(query) || m.name.toLowerCase().includes(query));

  return (
    <>
      <label className="field">Model
        <input type="text" required spellCheck={false} value={value} onChange={(e) => onChange(e.target.value)}
          placeholder={status === 'ready' ? `Search ${models.length} models from ${source}` : 'model id'} />
      </label>
      {status === 'loading' && <p className="hint">Loading models from {source}…</p>}
      {status === 'error' && (
        <p className="hint bad">
          Could not load the model list from {source}: {message}.
          {strict ? ' ' : ' You can still type a model id. '}
          <button type="button" className="ghost" onClick={retry}>Retry</button>
        </p>
      )}
      {status === 'ready' && (
        <>
          <ul className="model-list" aria-label={`Models from ${source}`}>
            {matches.slice(0, MAX_RESULTS).map((m) => (
              <li key={m.id}>
                <button type="button" className={m.id === value.trim() ? 'on' : undefined} onClick={() => onChange(m.id)}>
                  <span className="model-name">{m.name}</span>
                  <span className="model-id">{m.id}</span>
                  <span className="model-meta">
                    {formatContext(m.context)}
                    {m.reasoning && <span className="chip">reasoning</span>}
                  </span>
                </button>
              </li>
            ))}
            {matches.length === 0 && <li className="hint">No model matches “{value.trim()}”.</li>}
          </ul>
          <p className="hint">
            {exact
              ? <button type="button" className="ghost" onClick={() => onChange('')}>Choose a different model</button>
              : matches.length > MAX_RESULTS
                ? `Showing ${MAX_RESULTS} of ${matches.length}. Type to narrow the list.`
                : `${matches.length} models`}
          </p>
        </>
      )}
    </>
  );
}

function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  async function copy() {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      // Clipboard access can be denied; the text stays selectable.
    }
  }
  return (
    <button type="button" className="ghost" onClick={copy}>
      {copied ? <CheckIcon /> : <CopyIcon />} {copied ? 'Copied' : 'Copy'}
    </button>
  );
}

export function NewSessionPage() {
  const [draft, setDraft] = useState<SessionDraft>(EMPTY_DRAFT);
  const [step, setStep] = useState(0);
  const harness = harnessOf(draft);
  const setWorkdir = (patch: Partial<SessionDraft['workdir']>) =>
    setDraft((d) => ({ ...d, workdir: { ...d.workdir, ...patch } }));

  // Three sources for the model step. A harness with a server catalog (pi) is
  // asked through the factory for providers, models and per-model reasoning
  // levels. Elsewhere OpenRouter's public list is queried directly, and the
  // remaining providers share one generic list.
  const serverCatalog = harness?.catalog === 'server' ? harness.id : null;
  const catalog = useHarnessModels(serverCatalog);
  const openrouter = useOpenRouterModels(!serverCatalog && draft.provider === 'openrouter');
  const modelId = draft.model.trim();

  const catalogMatch = catalog.models.find((m) => m.provider === draft.provider && m.id === modelId);
  const harnessLevels = useReasoningLevels(serverCatalog, catalogMatch ? draft.provider : null, catalogMatch ? modelId : null);
  const openrouterMatch = !serverCatalog && draft.provider === 'openrouter'
    ? openrouter.models.find((m) => m.id === modelId)
    : undefined;
  const openrouterLevels = openrouterMatch && reasoningLevels(openrouterMatch);

  let levels: string[] = REASONING;
  let fallbackLevel = DEFAULT_REASONING;
  let reasoningNote: string | null = null;
  if (serverCatalog) {
    levels = harnessLevels.status === 'ready' ? harnessLevels.levels : [];
    fallbackLevel = levels.includes(DEFAULT_REASONING) ? DEFAULT_REASONING : levels[0] ?? DEFAULT_REASONING;
    reasoningNote = harnessLevels.status === 'ready' ? `Levels ${harness!.name} reports for this model.`
      : harnessLevels.status === 'loading' ? `Asking ${harness!.name} which levels this model supports…`
      : harnessLevels.status === 'error' ? `Could not get the levels: ${harnessLevels.message}`
      : 'Pick a model from the list to see the levels it supports.';
  } else if (draft.provider === 'openrouter') {
    levels = openrouterLevels?.levels ?? REASONING;
    fallbackLevel = openrouterLevels?.initial ?? DEFAULT_REASONING;
    reasoningNote = openrouterLevels?.note ?? 'Pick a model from the list to see the levels it supports.';
  }

  // Levels can arrive after the model was chosen; keep the selection valid for them.
  const levelValid = levels.length === 0 || levels.includes(draft.reasoning);
  useEffect(() => {
    if (!levelValid) setDraft((d) => ({ ...d, reasoning: fallbackLevel }));
  }, [levelValid, fallbackLevel]);

  function setModel(model: string) {
    setDraft((d) => {
      const match = !serverCatalog && d.provider === 'openrouter'
        ? openrouter.models.find((m) => m.id === model.trim())
        : undefined;
      return { ...d, model, reasoning: match ? reasoningLevels(match).initial : d.reasoning };
    });
  }

  // A server catalog is strict: the model must be one the harness offers, with a level it reports.
  const valid = (i: number) =>
    stepValid(i, draft) && (i !== 3 || !serverCatalog || (catalogMatch !== undefined && levels.includes(draft.reasoning)));

  const providerIds = serverCatalog ? [...new Set(catalog.models.map((m) => m.provider))] : harness?.providers ?? [];
  const pickerModels: PickerModel[] = serverCatalog
    ? catalog.models.filter((m) => m.provider === draft.provider)
        .map((m) => ({ id: m.id, name: m.name, context: m.context_window, reasoning: m.reasoning }))
    : openrouter.models.map((m) => ({ id: m.id, name: m.name, context: m.context_length, reasoning: m.reasoning !== null }));
  const pickerSource = serverCatalog ? catalog : openrouter;

  const [creating, setCreating] = useState(false);
  const [createError, setCreateError] = useState<string | null>(null);
  const blocked = notCreatable(draft);
  async function create() {
    setCreating(true);
    setCreateError(null);
    try {
      navigate(`/sessions/${await createSession(buildRequest(draft))}`);
    } catch (cause) {
      setCreateError(cause instanceof Error ? cause.message : String(cause));
      setCreating(false);
    }
  }

  const answers: (string | null)[] = [
    harness?.name ?? null,
    draft.isolation ? ISOLATION[draft.isolation].name : null,
    valid(2) ? describeWorkdir(draft.workdir) : null,
    valid(3) ? `${providerName(draft.provider!)} · ${modelId} · ${draft.reasoning}` : null,
  ];

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    if (valid(step)) setStep(step + 1);
  }

  if (step === REVIEW) {
    const json = JSON.stringify(buildRequest(draft), null, 2);
    const hueStyle = { '--hue': harness?.hue ?? 250 } as CSSProperties;
    return (
      <div className="review rise">
        <section className="hero hued" style={hueStyle}>
          <span className="mark big">{harness?.mark}</span>
          <div>
            <p className="eyebrow">Ready to launch</p>
            <p className="hero-title">{harness?.name} session</p>
            <p className="chips">
              {answers.slice(1).map((a) => <span key={a} className="chip">{a}</span>)}
            </p>
          </div>
        </section>

        <div className="review-grid">
          <section className="panel">
            <h2>Your choices</h2>
            <dl className="answers">
              {STEPS.map((label, i) => (
                <div key={label}>
                  <dt><span className="dot small">{i + 1}</span>{label}</dt>
                  <dd>{answers[i]}</dd>
                  <button type="button" className="ghost" onClick={() => setStep(i)}>Change</button>
                </div>
              ))}
            </dl>
          </section>

          <section className="panel">
            <h2>What would happen</h2>
            <ol className="timeline">
              {buildPlan(draft).map((line) => <li key={line}>{line}</li>)}
            </ol>
          </section>
        </div>

        <section className="panel">
          <div className="panel-head">
            <h2>Debug: generated request</h2>
            <CopyButton text={json} />
          </div>
          <p className="detail">
            Nothing is sent until you press Create session. This is the body for <code>POST /api/sessions</code>.
          </p>
          <pre className="debug">{highlight(json)}</pre>
        </section>

        <div className="actions">
          <button type="button" onClick={() => { setDraft(EMPTY_DRAFT); setStep(0); }}>Start over</button>
          <button type="button" className="primary" disabled={blocked !== null || creating} onClick={create}>
            {creating ? 'Starting…' : 'Create session'} <ArrowIcon />
          </button>
        </div>
        {blocked && <p className="hint">{blocked} Sessions can be created once an isolated runner exists.</p>}
        {createError && <p className="msg-error" role="alert">{createError}</p>}
      </div>
    );
  }

  return (
    <div className="wizard">
      <form className="panel wizard-main" onSubmit={onSubmit}>
        <ol className="stepper" aria-label="Progress">
          {STEPS.map((name, i) => {
            const done = i !== step && valid(i) && canReach(i, valid);
            return (
              <li key={name} className={done ? 'done' : undefined} aria-current={i === step ? 'step' : undefined}>
                <button type="button" disabled={!canReach(i, valid)} onClick={() => setStep(i)}>
                  <span className="dot">{done ? <CheckIcon /> : i + 1}</span>
                  <span className="step-name">{name}</span>
                </button>
              </li>
            );
          })}
        </ol>

        <div key={step} className="rise">
          {step === 0 && (
            <fieldset>
              <legend>Which harness should run the session?</legend>
              <p className="hint">The harness is the agent runtime. It decides which isolation levels and providers are on offer.</p>
              <div className="cards two">
                {HARNESSES.map((h) => (
                  <Card key={h.id} name="harness" title={h.name} hue={h.hue} checked={draft.harness === h.id}
                    visual={<span className="mark">{h.mark}</span>}
                    onSelect={() => setDraft((d) => withHarness(d, h.id))}>
                    {h.description}
                  </Card>
                ))}
              </div>
            </fieldset>
          )}

          {step === 1 && harness && (
            <fieldset>
              <legend>How isolated should {harness.name} be?</legend>
              <p className="hint">The outlined part runs away from the host.</p>
              <div className="cards two stacked">
                {ISOLATION_ORDER.map((id) => {
                  const supported = harness.isolation.includes(id);
                  return (
                    <Card key={id} name="isolation" title={ISOLATION[id].name} disabled={!supported}
                      visual={<IsolationDiagram level={id} />}
                      checked={draft.isolation === id} onSelect={() => setDraft((d) => ({ ...d, isolation: id }))}>
                      {supported ? ISOLATION[id].description : `Not supported by ${harness.name}.`}
                    </Card>
                  );
                })}
              </div>
            </fieldset>
          )}

          {step === 2 && (
            <fieldset>
              <legend>Where should the session work?</legend>
              <div className="cards three">
                {(Object.keys(WORKDIR) as WorkdirKind[]).map((kind) => (
                  <Card key={kind} name="workdir" title={WORKDIR[kind].name} checked={draft.workdir.kind === kind}
                    visual={<span className="glyph">{WORKDIR_ICON[kind]}</span>}
                    onSelect={() => setWorkdir({ kind })}>
                    {WORKDIR[kind].description}
                  </Card>
                ))}
              </div>
              <div key={draft.workdir.kind} className="rise">
                {draft.workdir.kind === 'local' && (
                  <label className="field">Folder path
                    <input type="text" required autoFocus spellCheck={false} placeholder="/home/me/project"
                      value={draft.workdir.path} onChange={(e) => setWorkdir({ path: e.target.value })} />
                  </label>
                )}
                {draft.workdir.kind === 'git' && (
                  <div className="field-row">
                    <label className="field grow">Repository URL
                      <input type="text" required autoFocus spellCheck={false}
                        placeholder="https://github.com/owner/repo.git"
                        value={draft.workdir.url} onChange={(e) => setWorkdir({ url: e.target.value })} />
                    </label>
                    <label className="field">Branch, tag or commit
                      <input type="text" spellCheck={false} placeholder="default branch" value={draft.workdir.ref}
                        onChange={(e) => setWorkdir({ ref: e.target.value })} />
                    </label>
                  </div>
                )}
              </div>
            </fieldset>
          )}

          {step === 3 && harness && (
            <fieldset>
              <legend>Which model should {harness.name} use?</legend>
              <p className="label">Provider</p>
              <div className="pills" role="radiogroup" aria-label="Provider">
                {providerIds.map((id) => (
                  <label key={id} className="pill">
                    <input type="radio" name="provider" checked={draft.provider === id}
                      onChange={() => setDraft((d) => ({ ...d, provider: id, model: '', reasoning: DEFAULT_REASONING }))} />
                    {providerName(id)}
                  </label>
                ))}
              </div>

              {serverCatalog && catalog.status === 'loading' && <p className="hint">Asking {harness.name} for its providers and models…</p>}
              {serverCatalog && catalog.status === 'ready' && (
                <p className="hint">Providers {harness.name} has credentials for on this host.</p>
              )}

              {serverCatalog || draft.provider === 'openrouter' ? (
                (draft.provider || pickerSource.status === 'error') && (
                  <ModelPicker source={serverCatalog ? harness.name : 'OpenRouter'} status={pickerSource.status}
                    models={pickerModels} message={pickerSource.status === 'error' ? pickerSource.message : undefined}
                    retry={pickerSource.retry} strict={serverCatalog !== null} value={draft.model} onChange={setModel} />
                )
              ) : (
                <>
                  <label className="field">Model
                    <input type="text" required spellCheck={false} placeholder="model id" value={draft.model}
                      disabled={draft.provider === null} onChange={(e) => setModel(e.target.value)} />
                  </label>
                  {draft.provider && PROVIDERS[draft.provider].models.length > 0 && (
                    <div className="pills suggestions">
                      {PROVIDERS[draft.provider].models.map((m) => (
                        <button key={m} type="button" className={`pill${draft.model === m ? ' on' : ''}`}
                          onClick={() => setModel(m)}>{m}</button>
                      ))}
                    </div>
                  )}
                </>
              )}

              <p className="label">Reasoning level</p>
              <div className="segmented" role="radiogroup" aria-label="Reasoning level">
                {levels.map((r, i) => (
                  <label key={r}>
                    <input type="radio" name="reasoning" checked={draft.reasoning === r}
                      onChange={() => setDraft((d) => ({ ...d, reasoning: r }))} />
                    <span className="bars" aria-hidden="true">
                      {levels.slice(1).map((_, bar) => (
                        <i key={bar} className={bar < i ? 'lit' : undefined}
                          style={{ height: `${((bar + 1) / (levels.length - 1)) * 100}%` }} />
                      ))}
                    </span>
                    {r}
                  </label>
                ))}
              </div>
              {reasoningNote && <p className="hint">{reasoningNote}</p>}
            </fieldset>
          )}
        </div>

        <div className="actions">
          {step > 0
            ? <button type="button" onClick={() => setStep(step - 1)}>Back</button>
            : <Link to="/sessions">Cancel</Link>}
          <button type="submit" className="primary" disabled={!valid(step)}>
            {step === REVIEW - 1 ? 'Review' : 'Next'} <ArrowIcon />
          </button>
        </div>
      </form>

      <aside className="panel recipe" aria-label="Session so far">
        <h2>Session so far</h2>
        <ol>
          {STEPS.map((name, i) => (
            <li key={name} className={answers[i] ? 'filled' : undefined} aria-current={i === step ? 'step' : undefined}>
              <span className="dot small">{answers[i] ? <CheckIcon /> : i + 1}</span>
              <span>
                <span className="recipe-label">{name}</span>
                <span className="recipe-value">{answers[i] ?? 'Not chosen yet'}</span>
              </span>
            </li>
          ))}
        </ol>
      </aside>
    </div>
  );
}
