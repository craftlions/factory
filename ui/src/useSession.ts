import { useEffect, useReducer } from 'react';
import type { Session } from './api';

// Mirrors the harness-agnostic chat model in src/harness/mod.rs.
export type Block =
  | { kind: 'text'; text: string }
  | { kind: 'thinking'; text: string }
  | { kind: 'tool_call'; id: string; name: string; arguments: unknown };

export type ChatItem =
  | { role: 'user'; text: string; ts: number }
  | { role: 'assistant'; blocks: Block[]; model: string | null; stop_reason: string | null; error: string | null; ts: number }
  | { role: 'tool_result'; tool_call_id: string; tool_name: string; text: string; is_error: boolean; ts: number }
  | { role: 'notice'; text: string; ts: number };

type LiveEvent =
  | { type: 'message_start' }
  | { type: 'block_start'; index: number; kind: Block['kind']; id: string | null; name: string | null }
  | { type: 'block_delta'; index: number; delta: string }
  | { type: 'message_end'; item: ChatItem }
  | { type: 'tool_start'; id: string; name: string; arguments: unknown }
  | { type: 'tool_update'; id: string; output: string }
  | { type: 'tool_end'; id: string; output: string; is_error: boolean }
  | { type: 'working'; working: boolean }
  | { type: 'notice'; text: string };

type ChatEvent =
  | LiveEvent
  | { type: 'snapshot'; session: Session; items: ChatItem[]; pending: LiveEvent[]; working: boolean };

/** A block of the assistant message in flight. A tool call's text is its raw JSON arguments so far. */
export interface StreamBlock { kind: Block['kind']; text: string; id: string | null; name: string | null }
export interface LiveTool { name: string; arguments: unknown; output: string; done: boolean; isError: boolean }

export interface ChatState {
  session: Session | null;
  items: ChatItem[];
  streaming: StreamBlock[] | null;
  tools: Record<string, LiveTool>;
  working: boolean;
  connection: 'connecting' | 'live' | 'reconnecting' | 'closed' | 'unavailable';
}

const EMPTY: ChatState = { session: null, items: [], streaming: null, tools: {}, working: false, connection: 'connecting' };

function applyLive(state: ChatState, event: LiveEvent): ChatState {
  switch (event.type) {
    case 'message_start':
      return { ...state, streaming: [] };
    case 'block_start': {
      const streaming = [...(state.streaming ?? [])];
      streaming[event.index] = { kind: event.kind, text: '', id: event.id, name: event.name };
      return { ...state, streaming };
    }
    case 'block_delta': {
      const streaming = [...(state.streaming ?? [])];
      const block = streaming[event.index];
      if (!block) return state;
      streaming[event.index] = { ...block, text: block.text + event.delta };
      return { ...state, streaming };
    }
    case 'message_end': {
      const items = [...state.items, event.item];
      if (event.item.role === 'assistant') return { ...state, items, streaming: null };
      if (event.item.role === 'tool_result') {
        const { [event.item.tool_call_id]: _finished, ...tools } = state.tools;
        return { ...state, items, tools };
      }
      return { ...state, items };
    }
    case 'tool_start':
      return { ...state, tools: { ...state.tools, [event.id]: { name: event.name, arguments: event.arguments, output: '', done: false, isError: false } } };
    case 'tool_update':
    case 'tool_end': {
      const tool = state.tools[event.id];
      if (!tool) return state;
      const done = event.type === 'tool_end';
      return { ...state, tools: { ...state.tools, [event.id]: { ...tool, output: event.output, done, isError: done && event.is_error } } };
    }
    case 'working':
      return { ...state, working: event.working };
    case 'notice':
      return { ...state, items: [...state.items, { role: 'notice', text: event.text, ts: Date.now() }] };
  }
}

type Action = ChatEvent | { type: 'connection'; connection: ChatState['connection'] } | { type: 'reset' };

function reduce(state: ChatState, action: Action): ChatState {
  switch (action.type) {
    case 'reset':
      return EMPTY;
    case 'connection':
      return { ...state, connection: action.connection };
    case 'snapshot': {
      const base: ChatState = { ...EMPTY, session: action.session, items: action.items, working: action.working, connection: 'live' };
      return action.pending.reduce(applyLive, base);
    }
    default:
      return applyLive(state, action);
  }
}

/**
 * Subscribes to one session's chat stream. Every (re)connect starts from a full
 * snapshot. The stream of an ended session is closed; change `epoch` to
 * subscribe again after restarting it.
 */
export function useSession(id: string, epoch: number): ChatState {
  const [state, dispatch] = useReducer(reduce, EMPTY);

  useEffect(() => {
    dispatch({ type: 'reset' });
    const source = new EventSource(`/api/sessions/${id}/events`);
    source.addEventListener('chat', (message: MessageEvent<string>) => {
      const event = JSON.parse(message.data) as ChatEvent;
      dispatch(event);
      // An ended session is one snapshot and nothing more.
      if (event.type === 'snapshot' && event.session.status !== 'running') {
        source.close();
        dispatch({ type: 'connection', connection: 'closed' });
      }
    });
    source.onerror = () => {
      // The browser retries by itself unless the server refused the stream outright.
      dispatch({ type: 'connection', connection: source.readyState === EventSource.CLOSED ? 'unavailable' : 'reconnecting' });
    };
    return () => source.close();
  }, [id, epoch]);

  return state;
}
