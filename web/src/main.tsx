import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { App } from './App';
import { installChunkReloadListener } from './chunkReload';
import './styles/app.css';

const root = document.getElementById('root');
if (!root) throw new Error('Missing #root element');

installChunkReloadListener();

createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
);

// Registered after load so it never competes with the first paint. The worker
// caches hashed /assets/* and leaves /api/ and /auth/ alone.
if ('serviceWorker' in navigator) {
  window.addEventListener('load', () => {
    navigator.serviceWorker.register('/service-worker.js').catch(error => {
      console.warn('Service worker registration failed', error);
    });
  });
}
