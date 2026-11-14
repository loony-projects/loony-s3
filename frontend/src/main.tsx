import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { loadSettings } from '@/lib/env';
import './index.css';

const root = createRoot(document.getElementById('root')!);

// The rest of the app reads its settings as constants when it loads, so it's only imported
// once they've arrived.
loadSettings()
  .then(async () => {
    const [{ BrowserRouter }, { ToastProvider }, { App }] = await Promise.all([
      import('react-router-dom'),
      import('@/context/ToastContext'),
      import('./App'),
    ]);
    root.render(
      <StrictMode>
        <BrowserRouter>
          <ToastProvider>
            <App />
          </ToastProvider>
        </BrowserRouter>
      </StrictMode>,
    );
  })
  .catch((err: unknown) => {
    root.render(
      <div style={{ fontFamily: 'system-ui, sans-serif', maxWidth: '32rem', margin: '4rem auto', padding: '0 1rem', color: '#374151' }}>
        <h1 style={{ fontSize: '1.125rem', fontWeight: 600 }}>This site couldn't start</h1>
        <p style={{ fontSize: '0.875rem' }}>{err instanceof Error ? err.message : 'Its settings could not be loaded.'} Try again in a moment.</p>
      </div>,
    );
  });
