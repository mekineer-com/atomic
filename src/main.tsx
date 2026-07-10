import ReactDOM from 'react-dom/client'
import { BrowserRouter } from 'react-router-dom'
import App from './App'
import { ErrorBoundary } from './components/ui/ErrorBoundary'
import './index.css'
import { initTransport } from './lib/transport'

async function clearStaleServiceWorkers() {
  if ('serviceWorker' in navigator) {
    await Promise.all((await navigator.serviceWorker.getRegistrations()).map(r => r.unregister()))
  }
  if ('caches' in window) {
    await Promise.all((await caches.keys()).map(key => caches.delete(key)))
  }
}

clearStaleServiceWorkers()
  .catch((err) => console.warn('Stale service worker cleanup failed:', err))
  .then(initTransport)
  .then(() => {
    const app = (
      <ErrorBoundary>
        <BrowserRouter>
          <App />
        </BrowserRouter>
      </ErrorBoundary>
    )

    ReactDOM.createRoot(document.getElementById('root')!).render(app)
  })
  .catch((err) => {
    console.error('Transport init failed:', err)
    document.getElementById('root')!.textContent = `Atomic failed to connect: ${String(err)}`
  })
