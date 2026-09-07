import { StrictMode, useEffect, useState } from 'react';
import { createRoot } from 'react-dom/client';
import './style.css';

function App() {
  const [status, setStatus] = useState('Checking connection…');

  useEffect(() => {
    const controller = new AbortController();
    async function checkHealth() {
      try {
        const response = await fetch('/api/health', { signal: controller.signal });
        if (!response.ok) throw new Error('Health check failed');
        const health: unknown = await response.json();
        if (typeof health !== 'object' || health === null ||
            !('status' in health) || health.status !== 'ok') {
          throw new Error('Invalid health response');
        }
        setStatus('Connected');
      } catch {
        if (!controller.signal.aborted) setStatus('Unavailable');
      }
    }
    void checkHealth();
    return () => controller.abort();
  }, []);

  return (
    <main>
      <h1>craftlions Factory</h1>
      <p>It’s alive.</p>
      <p role="status">Appliance: {status}</p>
    </main>
  );
}

createRoot(document.getElementById('root')!).render(
  <StrictMode><App /></StrictMode>,
);
