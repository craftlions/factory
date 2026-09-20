import { useSyncExternalStore, type MouseEvent, type ReactNode } from 'react';

function subscribe(onChange: () => void): () => void {
  window.addEventListener('popstate', onChange);
  return () => window.removeEventListener('popstate', onChange);
}

/** Current pathname without a trailing slash; the root stays `/`. */
export function usePath(): string {
  const path = useSyncExternalStore(subscribe, () => window.location.pathname);
  return path.length > 1 ? path.replace(/\/+$/, '') : path;
}

export function navigate(to: string): void {
  if (to === window.location.pathname) return;
  window.history.pushState(null, '', to);
  // pushState fires no event of its own; reuse popstate so subscribers update.
  window.dispatchEvent(new PopStateEvent('popstate'));
  window.scrollTo(0, 0);
}

export function Link({ to, current, children }: { to: string; current?: boolean; children: ReactNode }) {
  function onClick(event: MouseEvent<HTMLAnchorElement>) {
    // Leave new-tab, new-window and download gestures to the browser.
    if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    event.preventDefault();
    navigate(to);
  }
  return (
    <a href={to} onClick={onClick} aria-current={current ? 'page' : undefined}>{children}</a>
  );
}
