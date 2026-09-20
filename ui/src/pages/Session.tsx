import { useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties, type KeyboardEvent, type ReactNode } from 'react';
import { abortSession, formatTime, resumeSession, sendPrompt, stopSession } from '../api';
import { ArrowIcon, CheckIcon } from '../icons';
import { Link } from '../router';
import { HARNESSES, ISOLATION, type IsolationId } from '../sessionOptions';

/** Sessions created before host execution was removed cannot be restarted. */
const canRestart = (isolation: string | null) => isolation !== null && isolation !== 'none';
import { useSession, type Block, type ChatItem, type LiveTool, type StreamBlock } from '../useSession';

/** Fenced code blocks and inline code; everything else stays plain text. */
function RichText({ text }: { text: string }) {
  const parts = text.split(/```[^\n]*\n?([\s\S]*?)(?:```|$)/g);
  return (
    <>
      {parts.map((part, i) => {
        if (i % 2 === 1) return <pre key={i} className="code">{part.replace(/\n$/, '')}</pre>;
        return (
          <span key={i}>
            {part.split(/`([^`\n]+)`/g).map((piece, j) => (j % 2 === 1 ? <code key={j}>{piece}</code> : piece))}
          </span>
        );
      })}
    </>
  );
}

function summarizeArguments(args: unknown): string {
  if (args && typeof args === 'object') {
    const record = args as Record<string, unknown>;
    for (const key of ['command', 'path', 'file_path', 'pattern', 'query', 'url']) {
      if (typeof record[key] === 'string') return record[key];
    }
  }
  return typeof args === 'string' ? args : JSON.stringify(args ?? {});
}

function ToolCard({ name, args, output, state }: {
  name: string; args: unknown; output: string | null; state: 'pending' | 'running' | 'done' | 'error';
}) {
  const summary = summarizeArguments(args);
  return (
    <details className={`tool ${state}`} open={state === 'running' || state === 'error'}>
      <summary>
        <span className="tool-state" aria-label={state}>{state === 'done' ? <CheckIcon /> : state === 'error' ? '!' : ''}</span>
        <span className="tool-name">{name}</span>
        <span className="tool-summary">{summary}</span>
      </summary>
      {typeof args === 'object' && args !== null && Object.keys(args).length > 1 && (
        <pre className="code">{JSON.stringify(args, null, 2)}</pre>
      )}
      {output !== null && output !== '' && <pre className="code output">{output}</pre>}
      {output === '' && state !== 'running' && state !== 'pending' && <p className="detail">No output.</p>}
    </details>
  );
}

function Thinking({ text, live }: { text: string; live?: boolean }) {
  if (!text.trim()) return null;
  return (
    <details className="thinking" open={live}>
      <summary>{live ? 'Thinking…' : 'Thought'}</summary>
      <p>{text}</p>
    </details>
  );
}

type Results = Map<string, Extract<ChatItem, { role: 'tool_result' }>>;

function AssistantBlocks({ blocks, results, tools }: { blocks: Block[]; results: Results; tools: Record<string, LiveTool> }) {
  return (
    <>
      {blocks.map((block, i) => {
        if (block.kind === 'text') return block.text.trim() ? <div key={i} className="prose"><RichText text={block.text} /></div> : null;
        if (block.kind === 'thinking') return <Thinking key={i} text={block.text} />;
        const result = results.get(block.id);
        const live = tools[block.id];
        const state = result ? (result.is_error ? 'error' : 'done')
          : live ? (live.done ? (live.isError ? 'error' : 'done') : 'running')
          : 'pending';
        return <ToolCard key={i} name={block.name} args={block.arguments} state={state} output={result?.text ?? live?.output ?? null} />;
      })}
    </>
  );
}

function StreamingBlocks({ blocks }: { blocks: StreamBlock[] }) {
  return (
    <>
      {blocks.map((block, i) => {
        if (!block) return null;
        if (block.kind === 'text') return <div key={i} className="prose"><RichText text={block.text} /><span className="caret" /></div>;
        if (block.kind === 'thinking') return <Thinking key={i} text={block.text} live />;
        return <ToolCard key={i} name={block.name ?? 'tool'} args={block.text} state="pending" output={null} />;
      })}
    </>
  );
}

export function SessionPage({ id }: { id: string }) {
  const [epoch, setEpoch] = useState(0);
  const chat = useSession(id, epoch);
  const { session, items, streaming, tools, working } = chat;
  const [draft, setDraft] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [stopping, setStopping] = useState(false);
  const [restarting, setRestarting] = useState(false);
  const scroller = useRef<HTMLDivElement>(null);
  const pinned = useRef(true);

  const results = useMemo(() => {
    const map: Results = new Map();
    for (const item of items) if (item.role === 'tool_result' && item.tool_call_id) map.set(item.tool_call_id, item);
    return map;
  }, [items]);
  const callIds = useMemo(() => {
    const ids = new Set<string>();
    for (const item of items) if (item.role === 'assistant') for (const b of item.blocks) if (b.kind === 'tool_call') ids.add(b.id);
    return ids;
  }, [items]);

  // Follow new output only while the reader is already at the bottom.
  useLayoutEffect(() => {
    const el = scroller.current;
    if (el && pinned.current) el.scrollTop = el.scrollHeight;
  }, [items, streaming, tools, working]);

  useEffect(() => { setDraft(''); setError(null); setStopping(false); setRestarting(false); }, [id, epoch]);

  if (chat.connection === 'unavailable' && !session) {
    return (
      <section className="panel">
        <p className="detail">Session {id} could not be loaded. <Link to="/sessions">Back to sessions</Link>.</p>
      </section>
    );
  }
  if (!session) return <section className="panel"><p className="detail">Loading session…</p></section>;

  const running = session.status === 'running';
  const harness = HARNESSES.find((h) => h.id === session.kind);
  const hue = { '--hue': harness?.hue ?? 250 } as CSSProperties;

  async function act(action: () => Promise<unknown>) {
    setError(null);
    try {
      await action();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  }

  async function restart() {
    setRestarting(true);
    setError(null);
    try {
      await resumeSession(id);
      setEpoch((n) => n + 1);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
      setRestarting(false);
    }
  }

  function send() {
    const message = draft.trim();
    if (!message || !running) return;
    setDraft('');
    pinned.current = true;
    void act(() => sendPrompt(id, message)).then(() => undefined);
  }

  function onKeyDown(event: KeyboardEvent<HTMLTextAreaElement>) {
    if (event.key === 'Enter' && !event.shiftKey && !event.nativeEvent.isComposing) {
      event.preventDefault();
      send();
    }
  }

  const meta: ReactNode[] = [
    session.provider && session.model ? `${session.provider} · ${session.model}` : null,
    session.reasoning ? `reasoning ${session.reasoning}` : null,
    session.isolation === 'none' ? 'no isolation' : session.isolation ? ISOLATION[session.isolation as IsolationId]?.name ?? session.isolation : null,
    session.harness_session_id ? `${session.kind} session ${session.harness_session_id.slice(0, 8)}` : null,
    session.workdir === 'empty' ? 'empty directory' : session.workdir,
  ].filter(Boolean);

  return (
    <div className="chat hued" style={hue}>
      <section className="chat-head">
        <span className="mark">{harness?.mark ?? '?'}</span>
        <div className="chat-title">
          <p><strong>{harness?.name ?? session.kind}</strong> <span className={`badge ${session.status}`}>{session.status}</span></p>
          <p className="chips">{meta.map((m, i) => <span key={i} className="chip">{m}</span>)}</p>
        </div>
        {running && (
          <button type="button" disabled={stopping}
            onClick={() => { setStopping(true); void act(() => stopSession(id)); }}>
            {stopping ? 'Stopping…' : 'Stop session'}
          </button>
        )}
        {session.status === 'interrupted' && canRestart(session.isolation) && (
          <button type="button" className="primary" disabled={restarting} onClick={() => void restart()}>
            {restarting ? 'Restarting…' : 'Restart session'}
          </button>
        )}
      </section>

      <div className="chat-log" ref={scroller}
        onScroll={(e) => {
          const el = e.currentTarget;
          pinned.current = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
        }}>
        {items.length === 0 && !streaming && (
          <p className="empty">
            {running ? `${harness?.name ?? 'The harness'} starts in its own microvm. Setup progress appears here, and you can already say what it should do.` : 'This session has no messages.'}
          </p>
        )}
        {items.map((item, i) => {
          switch (item.role) {
            case 'user':
              return <div key={i} className="msg user"><RichText text={item.text} /></div>;
            case 'assistant':
              return (
                <div key={i} className="msg assistant">
                  <AssistantBlocks blocks={item.blocks} results={results} tools={tools} />
                  {item.error && <p className="msg-error">{item.error}</p>}
                  {item.stop_reason === 'aborted' && <p className="detail">Aborted.</p>}
                </div>
              );
            case 'tool_result':
              // Results are shown inside the call that produced them.
              return callIds.has(item.tool_call_id) ? null : (
                <div key={i} className="msg assistant">
                  <ToolCard name={item.tool_name} args={{}} state={item.is_error ? 'error' : 'done'} output={item.text} />
                </div>
              );
            case 'notice':
              return <p key={i} className="notice">{item.text}</p>;
          }
        })}
        {streaming && <div className="msg assistant"><StreamingBlocks blocks={streaming} /></div>}
        {working && !streaming && <div className="msg assistant"><span className="dots" aria-label="Working"><i /><i /><i /></span></div>}
      </div>

      {running ? (
        <form className="composer" onSubmit={(e) => { e.preventDefault(); send(); }}>
          <textarea rows={1} value={draft} autoFocus onKeyDown={onKeyDown}
            placeholder={working ? 'Steer the agent while it works…' : 'Message the agent…'}
            onChange={(e) => setDraft(e.target.value)} />
          {working && <button type="button" onClick={() => void act(() => abortSession(id))}>Abort</button>}
          <button type="submit" className="primary" disabled={!draft.trim()}>
            {working ? 'Steer' : 'Send'} <ArrowIcon />
          </button>
        </form>
      ) : (
        <p className="composer-closed">
          This session {session.status === 'failed' ? 'failed' : 'ended'}
          {session.ended_at ? ` on ${formatTime(session.ended_at)}` : ''}.
          {session.note ? ` ${session.note}` : ''} The conversation above is read from the harness’s own session file.
          {session.status === 'interrupted' && canRestart(session.isolation) && ' Restart it to continue this conversation in the same directory.'}
        </p>
      )}
      {error && <p className="msg-error" role="alert">{error}</p>}
      {chat.connection === 'reconnecting' && running && <p className="detail">Connection lost. Reconnecting…</p>}
    </div>
  );
}
