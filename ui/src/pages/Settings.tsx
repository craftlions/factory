import { useCallback, useEffect, useState, type FormEvent } from 'react';
import { fetchSettings, formatTime, removeCredential, saveCredential, type HarnessCredentials, type Settings } from '../api';

/** Backticks mark commands in hints. */
function withCode(text: string) {
  return text.split('`').map((part, i) => (i % 2 ? <code key={i}>{part}</code> : part));
}

function CredentialsPanel({ harness, onChange }: { harness: HarnessCredentials; onChange: () => void }) {
  const [kind, setKind] = useState(harness.stored?.kind ?? harness.kinds[0]?.id ?? '');
  const [secret, setSecret] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const stored = harness.stored;
  const storedKind = harness.kinds.find((k) => k.id === stored?.kind);
  const chosen = harness.kinds.find((k) => k.id === kind);

  async function act(action: () => Promise<unknown>) {
    setBusy(true);
    setError(null);
    try {
      await action();
      setSecret('');
      onChange();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy(false);
    }
  }

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    void act(() => saveCredential(harness.id, kind, secret));
  }

  return (
    <section className="panel">
      <div className="panel-head">
        <h2>{harness.name}</h2>
        <span className="chip">{stored ? `${storedKind?.name ?? stored.kind} ${stored.hint}` : 'Not signed in'}</span>
      </div>
      <p className="detail">
        Sessions pass this to {harness.name} inside their microvm, where code the agent runs can read it.
        It is kept on this host and never shown again.
        {stored && ` Saved ${formatTime(stored.updated_at)}.`}
      </p>
      <form onSubmit={onSubmit}>
        <p className="label">Sign in with</p>
        <div className="pills" role="radiogroup" aria-label="Sign in with">
          {harness.kinds.map((k) => (
            <label key={k.id} className="pill">
              <input type="radio" name={`${harness.id}-kind`} checked={kind === k.id} onChange={() => setKind(k.id)} />
              {k.name}
            </label>
          ))}
        </div>
        {chosen && <p className="hint">{withCode(chosen.hint)}</p>}
        <label className="field">{chosen?.name ?? 'Secret'}
          <input type="password" required autoComplete="off" spellCheck={false} value={secret}
            placeholder={stored?.kind === kind ? 'Enter a new one to replace it' : ''}
            onChange={(e) => setSecret(e.target.value)} />
        </label>
        <div className="actions">
          <button type="button" disabled={!stored || busy} onClick={() => void act(() => removeCredential(harness.id))}>
            Remove
          </button>
          <button type="submit" className="primary" disabled={busy || secret.trim() === ''}>
            {busy ? 'Saving…' : 'Save'}
          </button>
        </div>
        {error && <p className="msg-error" role="alert">{error}</p>}
      </form>
    </section>
  );
}

export function SettingsPage() {
  const [settings, setSettings] = useState<Settings | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [attempt, setAttempt] = useState(0);
  const reload = useCallback(() => setAttempt((n) => n + 1), []);

  useEffect(() => {
    const controller = new AbortController();
    fetchSettings(controller.signal).then(
      (loaded) => { setSettings(loaded); setError(null); },
      (cause: unknown) => {
        if (!controller.signal.aborted) setError(cause instanceof Error ? cause.message : String(cause));
      },
    );
    return () => controller.abort();
  }, [attempt]);

  if (!settings) {
    return (
      <section className="panel">
        <p className="detail">{error ? `Could not load the settings: ${error}` : 'Loading…'}</p>
      </section>
    );
  }
  return (
    <>
      {settings.credentials.map((harness) => (
        <CredentialsPanel key={harness.id} harness={harness} onChange={reload} />
      ))}
    </>
  );
}
