# Private live monitoring

This optional operator-managed stack supplies authenticated Prometheus scraping,
30-day/5-GB capped persistent metric storage, a provisioned Grafana dashboard,
and Alertmanager webhook delivery. It is **not started by relay deployment**.
Checking these files does not establish that production monitoring is running.
No terminal frames, credentials, or publisher identities belong in metric
labels.

## Start on a deliberately selected monitoring host

Use Docker Compose v2 and Trivy. Exact versioned registry digests are recorded
in `images.json`. Startup is blocked until every image passes a fresh
HIGH/CRITICAL scan; scanner errors also block startup. As of 2026-10-28, the
selected Grafana 13.3.0 and Alertmanager 0.34.1 images still have upstream
dependency findings. **Do not deploy this stack until patched images pass.**
Prometheus 3.15.1 passed this severity gate. A passing scan is not a security
guarantee. Do not override Compose image variables to bypass the gate.
Create three private, absolute-path regular files outside the checkout:

- The independent **metrics bearer credential**, not the publisher credential.
- An Alertmanager-compatible HTTPS webhook URL, including any receiver token.
- A unique Grafana administrator password.

Set `MONITORING_METRICS_KEY_FILE`, `MONITORING_ALERT_WEBHOOK_FILE`, and
`MONITORING_GRAFANA_PASSWORD_FILE` to those paths. Give secret files mode 0600;
ensure the container runtime can mount them readable by the service's non-root
user without making them globally readable on the host. Never paste values into
commands, tracked files, or support bundles. Validate URL/credential rotation
against your receiver and relay before relying on alerts.

```sh
python3 scripts/check-live-monitoring-images.py
# Only after successful scans, explicitly start:
python3 scripts/check-live-monitoring-images.py --up
```

All UI ports bind to loopback. Access Grafana on port 3000 through an SSH
tunnel;
Prometheus (9090) and Alertmanager (9093) have no additional HTTP authentication
and must not be exposed on public interfaces. Local host users are inside the
trust boundary. A remote-access reverse proxy must authenticate users and use
TLS. Do not enable Grafana anonymous access.

The collector requires outbound HTTPS to `live.bmux.dev:443`; alerts require
outbound HTTPS to the configured webhook. Scraping disables redirects so the
bearer credential cannot be forwarded to a different origin. Configure a
different relay by changing `prometheus.json`, not by adding credential text.

## Acceptance and maintenance

1. In Prometheus, confirm `up{job="bmux-relay"} == 1`. Test a deliberately wrong
   metrics credential in an isolated test instance and confirm the scrape fails.
2. Confirm the **BMUX live delivery** dashboard appears in Grafana.
3. Send a deliberate test alert to the configured receiver and confirm both firing
   and resolution delivery. A configured URL is not evidence notifications work.
4. Restart this monitoring stack (not the relay) and verify earlier samples remain.
5. Back up named volumes; test restoration and disk-space alerts under your host
   monitoring policy. Alertmanager state persists silences, not an incident log.
6. Rotate credentials through private files and recreate the affected containers.
   Do not use `down --volumes` unless deliberate telemetry deletion is intended.

No low-frame-rate alert is provided: an unchanged terminal is legitimately idle.
Repairs and throttling are symptoms; use publisher and browser diagnostics to
identify the failing stage. Relay health and retained frames do not prove
delivery.
Prometheus retains sampled counters, not causal event histories. Preserve
rotated
publisher/relay structured logs separately under your private log retention
policy.
No hosted account, remote backup, public ingress, or paid resources are created
by
this directory. Live deployment and alert receiver selection remain operator
steps.

## Configuration validation

Run `python3 scripts/test-live-monitoring.py`. With reviewed images available,
validate native configuration too:

```sh
docker run --rm --network none --entrypoint /bin/promtool -v "$PWD/infra/live-monitoring:/etc/prometheus:ro" "$(python3 -c 'import json; print(json.load(open("infra/live-monitoring/images.json"))["prometheus"])')" check rules /etc/prometheus/alerts.json
docker run --rm --network none --entrypoint /bin/amtool -v "$PWD/infra/live-monitoring:/etc/alertmanager:ro" "$(python3 -c 'import json; print(json.load(open("infra/live-monitoring/images.json"))["alertmanager"])')" check-config /etc/alertmanager/alertmanager.json
```

Full Prometheus config validation additionally needs the private credential file
mounted at `/run/secrets/relay_metrics_key`; do not bake it into a test image.

Image updates must change `images.json`, re-run the scanner gate, and verify
native configuration compatibility. The gate uses immutable references and
does not accept ignore files or ignore unfixed findings. Trivy must be available
on PATH; for a temporary Nix shell use `nix shell nixpkgs#trivy`. No scan result
is persisted as permission to deploy later.
