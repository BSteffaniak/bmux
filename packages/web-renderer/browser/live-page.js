import { mountLiveViewer } from './live-viewer.js';

// A small, opt-in local panel. The export is displayed as text for deliberate
// copying; it never uploads diagnostics or asks for clipboard permission.
export function mountDiagnosticPanel({ root, handle, doc = root.ownerDocument,
  setTimer = setTimeout, clearTimer = clearTimeout }) {
  const details = doc.createElement('details');
  const summary = doc.createElement('summary');
  const output = doc.createElement('pre');
  const button = doc.createElement('button');
  const exported = doc.createElement('textarea');
  summary.textContent = 'Delivery diagnostics';
  output.style.cssText = 'white-space:pre-wrap;font-size:.8rem';
  button.type = 'button';
  button.textContent = 'Show sanitized export';
  exported.readOnly = true;
  exported.hidden = true;
  exported.rows = 10;
  exported.style.cssText = 'width:100%';
  exported.setAttribute('aria-label', 'Sanitized delivery diagnostic export');
  details.append(summary, output, button, exported);
  root.after(details);
  let timer, disposed = false;
  function render() {
    const d = handle.diagnostics;
    if (!d) return;
    const age = value => value === null ? 'unknown' : `${value} ms`;
    output.textContent = `${d.progress} · ${d.connection} · source ${d.source}\n`
      + `Received ${d.counts.receive} · decoded ${d.counts.decoded} · renderer-submitted ${d.counts.submitted} · ACK sent ${d.counts.ackSent}\n`
      + `Receive age ${age(d.agesMs.receive)} · submission age ${age(d.agesMs.submitted)} · pending age ${age(d.agesMs.pending)}\n`
      + `Last submitted cursor ${d.submitted ? `${d.submitted.generation}/${d.submitted.revision}` : 'unknown'} · last ACK sent cursor ${d.ackSent ? `${d.ackSent.generation}/${d.ackSent.revision}` : 'unknown'} (local generation ordinals)\n`
      + `Reason: ${d.reason}\nRetained frame is not proof of ongoing delivery. Idle means no known pending frame; upstream progress is unknown.\n`
      + 'Renderer-submitted is not physical display. ACK sent is not server receipt. Pending warnings concern local work only (5 s), not upstream freshness.';
  }
  function refresh() {
    if (disposed || !details.open) return;
    render();
    timer = setTimer(refresh, 1000);
  }
  details.addEventListener('toggle', () => {
    clearTimer(timer);
    if (details.open) refresh();
  });
  button.addEventListener('click', () => {
    exported.value = handle.exportDiagnostics();
    exported.hidden = false;
  });
  render();
  return { dispose() { disposed = true; clearTimer(timer); details.remove(); } };
}

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
    mounted?.panel?.dispose();
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
      else {
        mounted.handle = handle;
        mounted.panel = mountDiagnosticPanel({ root, handle, doc });
      }
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
