import { mountLiveViewer } from './live-viewer.js';

// Loaded on every docs entry route: SPA body replacements don't rerun scripts.
export function mountLivePage({ doc = document, Observer = MutationObserver,
  events = globalThis, viewerFactory = mountLiveViewer } = {}) {
  let mounted = null;
  let generation = 0;
  let suspended = false;
  let disposed = false;
  function reconcile() {
    const root = suspended || disposed ? null : doc.getElementById('live-broadcast');
    if (mounted?.root === root) return;
    generation++;
    mounted?.handle?.dispose();
    mounted = null;
    if (!root) return;
    const ticket = generation;
    mounted = { root, handle: null };
    const status = doc.getElementById('live-status');
    root.style.cssText = 'min-height:12rem;display:flex;align-items:center;justify-content:center;background:#080b10;overflow:hidden';
    void viewerFactory({ container: root, status,
      apiBase: root.dataset?.liveRelay || 'https://live.bmux.dev/', assetBase: new URL('.', import.meta.url).href }).then(handle => {
      if (ticket !== generation || !root.isConnected) handle.dispose();
      else mounted.handle = handle;
    }).catch(() => {
      if (ticket === generation) status.textContent = 'Broadcast viewer unavailable';
    });
  }
  const observer = new Observer(reconcile);
  const observe = () => observer.observe(doc.documentElement, { childList: true, subtree: true });
  const hide = () => { suspended = true; observer.disconnect(); reconcile(); };
  const show = () => {
    if (disposed) return;
    suspended = false;
    observe();
    reconcile();
  };
  observe();
  events.addEventListener('pagehide', hide);
  events.addEventListener('pageshow', show);
  reconcile();
  return {
    dispose() {
      disposed = true;
      hide();
      events.removeEventListener('pagehide', hide);
      events.removeEventListener('pageshow', show);
    },
  };
}
if (typeof document !== 'undefined') mountLivePage();
