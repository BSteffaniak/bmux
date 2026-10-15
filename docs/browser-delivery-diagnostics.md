# Browser delivery diagnostics

Browser errors preserve the last committed output. A full-document retry is an
attempt, not evidence of recovery. A browser network error cannot distinguish a
stopped host from a rejected connection or network policy by itself.

## Evidence sources

- In browser developer tools, run `window.__bmuxDiagnostics()` to copy the last
  64 delivery events. Events distinguish network unavailability, cancellation,
  HTTP rejection, document repair and recovery. They omit tokens, URLs, headers,
  resource identities and document contents. History is local to the page and
  resets on refresh.
- `GET /diagnostics` on the serving origin returns bounded host events, counters
  and the latest observed publication revision. It follows the normal host and
  token authorization policy; do not put bearer credentials in query strings.
- The host prints fixed-code events to stderr, including producer closure,
  listener accept failure, connection saturation, failed connection tasks,
  rejected requests and document/resource response write failures/timeouts.
  Repetitions are logged at powers of two; counters retain every occurrence.
- The presenter checks host availability when publishing and reports a stopped
  host instead of silently publishing into an unserved feed.

If the listener has stopped, its HTTP diagnostics are also unavailable. The
terminal stderr event and in-process `HostHandle::diagnostics()` retain the
termination reason. Expected shutdown is recorded separately from producer loss
and listener failure. Diagnostics do not restart the host, alter presentation
revisions, or imply playbook cleanup.
