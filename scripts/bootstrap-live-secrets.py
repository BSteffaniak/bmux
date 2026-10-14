#!/usr/bin/env python3
"""Bootstrap BMUX Production secrets without displaying credentials.

Normal: --repo OWNER/REPO --publisher-key-file /PRIVATE/KEY (securely create if absent).
Optional guided AWS: --bootstrap-state --configure-oidc --state-account-id EXPECTED12
--state-dir /PRIVATE/STATE; region us-east-1, deterministic account bucket, bmux-live-production role.
Existing LIVE_STATE_BUCKET is authoritative; conflicts reject. Requires local AWS bootstrap
credentials and separate CLOUDFLARE_PROVISION_TOKEN; never reuses another project's token.
Preconfigure Production and production-approval with master-only custom branch policies;
only production-approval needs reviewers/no admin bypass; self-review is permitted.
Environment protections are inspected only. OIDC customization is repo-wide and explicit.
--state-mode plan/review performs no partial secret installation; apply requires exact
--state-operation plus --accept-cost. --dry-run makes zero subprocess/provider requests.
Advanced --input-env retains supplied values/static backend credentials.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import secrets
import selectors
import signal
import time
import urllib.error
import urllib.parse
import urllib.request


class BootstrapError(Exception):
    """Sanitized error; provider diagnostics are never printed."""


LEGACY_SECRET_NAMES = (
    'CLOUDFLARE_API_TOKEN', 'CLOUDFLARE_ACCOUNT_ID', 'CLOUDFLARE_ZONE_ID',
    'LIVE_HOSTNAME', 'LIVE_PAGES_PROJECT', 'LIVE_PUBLISHER_KEY_SHA256',
    'LIVE_STATE_BUCKET', 'LIVE_STATE_REGION', 'LIVE_STATE_ACCESS_KEY_ID',
    'LIVE_STATE_SECRET_ACCESS_KEY',
)


STATE_COMMAND_TIMEOUT = 1000  # Backend tools own <=900s deadlines and cleanup first.
COMMAND_OUTPUT_LIMIT = 32 * 1024 * 1024


def bounded_run(args, body=None, timeout=120):
    """Pipe-only bounded capture; secrets never touch diagnostic files or argv."""
    payload = (body or '').encode('utf-8')
    if len(payload) > 65536:
        raise BootstrapError('Command input exceeds bound; no operation started')
    process = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, start_new_session=True)
    deadline = time.monotonic() + timeout
    output = bytearray()
    retained = 0
    offset = 0
    try:
        with selectors.DefaultSelector() as selector:
            for stream, kind in ((process.stdout, 'stdout'), (process.stderr, 'stderr')):
                os.set_blocking(stream.fileno(), False)
                selector.register(stream, selectors.EVENT_READ, kind)
            if payload:
                os.set_blocking(process.stdin.fileno(), False)
                selector.register(process.stdin, selectors.EVENT_WRITE, 'stdin')
            else:
                process.stdin.close()
            while selector.get_map():
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise subprocess.TimeoutExpired(args, timeout)
                for key, _ in selector.select(min(remaining, 0.1)):
                    if key.data == 'stdin':
                        try:
                            offset += os.write(key.fd, payload[offset:offset + 4096])
                        except BrokenPipeError:
                            offset = len(payload)
                        if offset == len(payload):
                            selector.unregister(key.fileobj)
                            key.fileobj.close()
                        continue
                    chunk = os.read(key.fd, 65536)
                    if not chunk:
                        selector.unregister(key.fileobj)
                        key.fileobj.close()
                        continue
                    retained += len(chunk)
                    if retained > COMMAND_OUTPUT_LIMIT:
                        raise BootstrapError('Command output exceeds bound; operation ambiguous; inspect durable state and reconcile exact operation ID before retry; output suppressed')
                    if key.data == 'stdout':
                        output.extend(chunk)
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise subprocess.TimeoutExpired(args, timeout)
            result = process.wait(timeout=remaining)
        try:
            text = output.decode('utf-8')
        except UnicodeError:
            raise BootstrapError('Invalid command response encoding; output suppressed; inspect state before retry') from None
        return subprocess.CompletedProcess(args, result, text, '')
    finally:
        # Only our newly created session/process group is addressed. Detached tool
        # groups remain backend-owned; timeout/overflow never prove them stopped.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()
        for stream in (process.stdin, process.stdout, process.stderr):
            stream.close()


def command(args, body=None):
    companion = (len(args) >= 2 and args[0] == sys.executable
                 and args[1] == str(Path(__file__).with_name('provision-live-state.py')))
    timeout = STATE_COMMAND_TIMEOUT if companion else 120
    try:
        result = bounded_run(args, body, timeout=timeout)
    except subprocess.TimeoutExpired:
        if companion:
            raise BootstrapError('Backend operation timed out; operation ambiguous; inspect durable state and reconcile exact operation ID before any retry; backend owns descendant cleanup') from None
        raise BootstrapError('GitHub operation timed out; write may be ambiguous; inspect state before retry') from None
    except OSError:
        raise BootstrapError('Command unavailable; output suppressed; inspect state before retry') from None
    if result.returncode:
        raise BootstrapError('Command operation failed; output redacted; inspect state before retry')
    if len(result.stdout.encode('utf-8')) > COMMAND_OUTPUT_LIMIT or len(result.stderr.encode('utf-8')) > COMMAND_OUTPUT_LIMIT:
        raise BootstrapError('Command response exceeds bound; output suppressed')
    return result.stdout


def read_private_key(path):
    # No symlinks, no group/world permissions; open the actual verified descriptor.
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
        with os.fdopen(fd, 'rb') as stream:
            info = os.fstat(stream.fileno())
            if not stat.S_ISREG(info.st_mode) or info.st_mode & 0o077 or info.st_uid != os.getuid():
                raise BootstrapError('Publisher key must be an owned private regular file (0600)')
            key = stream.read(4097).strip()
    except OSError:
        raise BootstrapError('Cannot read private publisher key') from None
    if not 32 <= len(key) <= 4096 or any(c <= 32 or c >= 127 for c in key):
        raise BootstrapError('Publisher key must be 32–4096 printable ASCII bytes without whitespace')
    return hashlib.sha256(key).hexdigest()


def token_policy(account, zone, permission_groups):
    """Separate account Workers/Pages scope from zone-route scope; no broad zone wildcard."""
    policies = []
    for scope, resource, names in (
        ('com.cloudflare.api.account', f'com.cloudflare.api.account.{account}',
         ('Workers Scripts Write', 'Pages Write')),
        ('com.cloudflare.api.account.zone', f'com.cloudflare.api.account.zone.{zone}',
         ('Workers Routes Write', 'DNS Write', 'Zone Read')),
    ):
        ids = []
        for name in names:
            matches = [g for g in permission_groups if g.get('name') == name and scope in g.get('scopes', [])]
            if len(matches) != 1:
                raise BootstrapError('Cannot uniquely resolve required scoped Cloudflare permission')
            ids.append({'id': matches[0]['id']})
        policies.append({'effect': 'allow', 'resources': {resource: '*'}, 'permission_groups': ids})
    return {'policies': policies}


def verify_environment(repo, environment, run=command):
    try:
        data = json.loads(run(['gh', 'api', f'repos/{repo}/environments/{urllib.parse.quote(environment, safe="")}']))
    except BootstrapError:
        raise BootstrapError(f'Cannot inspect required environment {environment}; create it in repository Settings > Environments, then configure master-only branch protection; never auto-rewrite protections') from None
    # Never PUT the environment: do not accidentally replace/erase existing protections.
    policy = data.get('deployment_branch_policy') or {}
    if not policy.get('custom_branch_policies'):
        raise BootstrapError('Environment needs an existing custom branch policy restricted to master')
    branch_endpoint = f'repos/{repo}/environments/{urllib.parse.quote(environment, safe="")}/deployment-branch-policies'
    rules = []
    for page in range(1, 101):
        endpoint = branch_endpoint if page == 1 else branch_endpoint + f'?page={page}&per_page=30'
        branches = json.loads(run(['gh', 'api', endpoint]))
        rows = branches.get('branch_policies', [])
        total = branches.get('total_count', len(rows))
        if not isinstance(rows, list) or not isinstance(total, int) or total > 3000 or not rows and len(rules) < total:
            raise BootstrapError('Invalid/bounded environment branch policy pagination')
        rules.extend(rows)
        if len(rules) >= total:
            break
    else:
        raise BootstrapError('Environment branch policies exceed bound')
    if policy.get('protected_branches') is True:
        raise BootstrapError('Custom master-only branch policy required; protected-branches wildcard rejected')
    if not rules or any(r.get('name') != 'master' or r.get('type', 'branch') != 'branch' for r in rules):
        raise BootstrapError('Environment branch policy must allow only master (no tags/wildcards)')
    if environment == 'production-approval':
        reviews = [r for r in data.get('protection_rules', []) if r.get('type') == 'required_reviewers']
        if not reviews or not reviews[0].get('reviewers'):
            raise BootstrapError('production-approval needs required reviewers (self-review is allowed)')
        if data.get('can_admins_bypass', True):
            raise BootstrapError('Production must disallow admin protection bypass')


def provision_env(args, run=command, environ=None):
    environ = os.environ if environ is None else environ
    selected = args.only or list(LEGACY_SECRET_NAMES)
    if len(selected) != len(set(selected)):
        raise BootstrapError('Duplicate --only selections rejected')
    if args.rotate and not args.only:
        raise BootstrapError('Rotation requires explicit --only selections; no blanket rotation')
    if args.dry_run:
        print(json.dumps({'repo': args.repo, 'environment': args.environment,
                          'mode': 'replace-selected' if args.rotate else 'skip-existing',
                          'secrets': selected, 'provider_requests': False,
                          'protections': 'verify only; never weaken'}))
        return
    verify_environment(args.repo, args.environment, run)
    verify_environment(args.repo, "production-approval", run)
    existing = {s['name'] for s in json.loads(run([
        'gh', 'secret', 'list', '--repo', args.repo, '--env', args.environment, '--json', 'name']))}
    pending = [name for name in selected if args.rotate or name not in existing]
    values = {}
    # Validate every input before any mutation; publisher key never goes to GitHub.
    for name in pending:
        if name == 'LIVE_PUBLISHER_KEY_SHA256':
            if not args.publisher_key_file:
                raise BootstrapError('--publisher-key-file is required for a new digest')
            value = read_private_key(args.publisher_key_file)
        else:
            value = environ.get(name)
        if not value or '\n' in value or '\r' in value:
            raise BootstrapError(f'Missing/invalid input for {name}')
        values[name] = value
    if args.rotate and not args.only:
        raise BootstrapError('Rotation requires explicit --only selections; no blanket rotation')
    for name, value in values.items():
        run(['gh', 'secret', 'set', name, '--repo', args.repo, '--env', args.environment], value)
        print(f'Set {name} in selected environment')
    for name in selected:
        if name not in values:
            print(f'Skipped existing {name}')
    # No automatic old-token revocation: CI in flight may still hold old credentials.
    if args.rotate:
        print('Rotation installed; verify a protected deployment before manually revoking old credentials')


# Normal bootstrap creates only project-scoped credentials. Backend credentials belong
# to the separate explicitly authorized state provisioner.
NORMAL_SECRET_NAMES = LEGACY_SECRET_NAMES[:6]
STATE_SECRET_NAMES = ('LIVE_STATE_BUCKET', 'LIVE_STATE_REGION', 'LIVE_STATE_ROLE_ARN')
SECRET_NAMES = NORMAL_SECRET_NAMES + STATE_SECRET_NAMES
OIDC_CLAIMS = ['repo', 'context', 'ref']


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


class Cloudflare:
    def __init__(self, environ):
        self.token = environ.get('CLOUDFLARE_PROVISION_TOKEN')
        if not self.token or any(c.isspace() for c in self.token):
            raise BootstrapError('Set separate CLOUDFLARE_PROVISION_TOKEN with token provisioning and target-zone access')

    def request(self, method, path, body=None):
        request = urllib.request.Request('https://api.cloudflare.com/client/v4' + path,
            data=None if body is None else json.dumps(body).encode(), method=method,
            headers={'Authorization': 'Bearer ' + self.token, 'Content-Type': 'application/json'})
        try:
            with urllib.request.build_opener(NoRedirect).open(request, timeout=30) as response:
                raw = response.read(1048577)
                if len(raw) > 1048576:
                    raise BootstrapError('Cloudflare response exceeds bound; output suppressed')
                data = json.loads(raw)
            if not isinstance(data, dict) or data.get('success') is not True:
                raise BootstrapError('Cloudflare rejected request; inspect scoped permissions and provider state; output suppressed')
            return data
        except (urllib.error.URLError, OSError, ValueError, TypeError, KeyError):
            raise BootstrapError('Cloudflare request failed; response suppressed; inspect provider state before retry') from None

    def items(self, path):
        for page in range(1, 101):
            data = self.request('GET', path + ('&' if '?' in path else '?') + f'page={page}&per_page=50')
            rows = data.get('result')
            if not isinstance(rows, list) or len(rows) > 1000:
                raise BootstrapError('Invalid Cloudflare page; output suppressed')
            yield from rows
            pages = data.get('result_info', {}).get('total_pages', 1)
            if not isinstance(pages, int) or pages < 1 or pages > 100:
                raise BootstrapError('Invalid Cloudflare pagination; output suppressed')
            if page >= pages:
                return
        raise BootstrapError('Cloudflare pagination exceeds bound')


def private_key_digest(path):
    """Generate only at an explicit private absolute path; never replace existing keys."""
    if not path or not path.is_absolute():
        raise BootstrapError('--publisher-key-file must be an explicit absolute private path')
    try:
        info = path.parent.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o022:
            raise BootstrapError('Publisher key parent must be an owned directory without group/world write')
        try:
            fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        except FileExistsError:
            return read_private_key(path)
        with os.fdopen(fd, 'wb') as stream:
            stream.write(secrets.token_urlsafe(48).encode())
            stream.flush()
            os.fsync(stream.fileno())
        directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    except OSError:
        raise BootstrapError('Private publisher key creation failed; inspect local file before retry') from None
    return read_private_key(path)


def discover(cf, args):
    zone_name = getattr(args, 'zone', 'bmux.dev')
    zones = list(cf.items('/zones?name=' + urllib.parse.quote(zone_name, safe='')))
    exact = [z for z in zones if z.get('name') == zone_name]
    if len(zones) != 1 or len(exact) != 1:
        raise BootstrapError('Zone discovery must return one exact root-zone/account match; grant Zone Read or select --zone')
    zone = exact[0]
    account_id, zone_id = zone['account']['id'], zone['id']
    if not all(re.fullmatch('[a-fA-F0-9]{32}', value) for value in (account_id, zone_id)):
        raise BootstrapError('Invalid discovered Cloudflare identity')
    return account_id, zone_id


def pages_project(cf, account, name):
    projects = list(cf.items(f'/accounts/{account}/pages/projects'))
    exact = [p for p in projects if p.get('name') == name]
    if len(exact) > 1:
        raise BootstrapError('Ambiguous Pages project; inspect ownership before deployment')
    if exact:
        if exact[0].get('production_branch') != 'master':
            raise BootstrapError('Existing Pages project must use production branch master; review ownership explicitly')
        if exact[0].get('source'):
            raise BootstrapError('Existing Pages project is Git-integrated; review explicit ownership conversion before deployment')
    # Missing project is declaratively created later, never implicitly here.
    return name


def verify_oidc(repo, run, configure=False):
    endpoint = f'repos/{repo}/actions/oidc/customization/sub'
    current = json.loads(run(['gh', 'api', endpoint]))
    desired = {'use_default': False, 'include_claim_keys': OIDC_CLAIMS}
    if not isinstance(current, dict) or not isinstance(current.get('use_default'), bool):
        raise BootstrapError('Invalid existing OIDC template; no modification authorized')
    if current.get('use_default') is False:
        if current.get('include_claim_keys') != OIDC_CLAIMS:
            raise BootstrapError('Existing custom OIDC subject differs; do not overwrite other consumers; explicit migration required')
        return
    if not configure:
        raise BootstrapError('OIDC subject template needed; --configure-oidc explicitly authorizes repo-wide change affecting existing consumers')
    print('WARNING: configuring repo-wide OIDC subject template changes all existing OIDC consumers')
    run(['gh', 'api', '--method', 'PUT', endpoint, '--input', '-'], json.dumps(desired))
    verified = json.loads(run(['gh', 'api', endpoint]))
    if verified.get('use_default') is not False or verified.get('include_claim_keys') != OIDC_CLAIMS:
        raise BootstrapError('OIDC template write not verified; inspect before retry')


def validate_state_result(result, args, operation, phases, allow_empty_operation=False):
    allowed = {'schema', 'operation', 'phase', 'backend', 'role_arn', 'github_oidc', 'resources'}
    if not isinstance(result, dict) or set(result) - allowed or not (allow_empty_operation and operation is None or isinstance(operation, str) and re.fullmatch(r'[A-Za-z0-9_-]{1,128}', operation)):
        raise BootstrapError('Invalid public backend response schema')
    resources = result.get('resources', [])
    if not isinstance(resources, list) or len(resources) > 64:
        raise BootstrapError('Invalid backend resource review')
    for item in resources:
        if (not isinstance(item, dict) or set(item) != {'address', 'actions', 'import'}
                or not re.fullmatch(r'[A-Za-z0-9_.\[\]\"-]{1,256}', item.get('address', ''))
                or not isinstance(item.get('import'), bool)
                or not isinstance(item.get('actions'), list)
                or not item['actions'] or 'delete' in item['actions']
                or any(action not in ('no-op', 'read', 'create', 'update') for action in item['actions'])):
            raise BootstrapError('Invalid backend resource review')
    expected_subject = f'repo:{args.repo}:environment:Production:ref:refs/heads/master'
    backend = result.get('backend', {})
    if not isinstance(backend, dict) or set(backend) != {'bucket', 'region', 'key', 'use_lockfile', 'encrypt'} or not isinstance(result.get('github_oidc'), dict) or set(result['github_oidc']) != {'include_claim_keys', 'subject'}:
        raise BootstrapError('Invalid backend public metadata shape')
    if (result.get('schema') != 1 or result.get('operation') != operation
            or result.get('phase') not in phases
            or backend.get('bucket') != args.state_bucket or backend.get('region') != args.state_region
            or backend.get('key') != 'bmux/live/production/terraform.tfstate'
            or backend.get('encrypt') is not True or backend.get('use_lockfile') is not True
            or result.get('github_oidc', {}).get('subject') != expected_subject
            or result.get('github_oidc', {}).get('include_claim_keys') != OIDC_CLAIMS
            or result.get('role_arn') != f'arn:aws:iam::{args.state_account_id}:role/{args.state_role}'):
        raise BootstrapError('State result does not match approved backend/OIDC identity; no secrets installed')


def state_bootstrap(args, run, pending, environ=None):
    environ = os.environ if environ is None else environ
    if not getattr(args, 'bootstrap_state', False) or not pending:
        return {}
    if args.environment != 'Production':
        raise BootstrapError('OIDC backend requires exact Production environment subject')
    args.state_region = getattr(args, 'state_region', None) or 'us-east-1'
    configured_bucket = environ.get('LIVE_STATE_BUCKET')
    if configured_bucket and args.state_bucket and configured_bucket != args.state_bucket:
        raise BootstrapError('Explicit bucket conflicts with authoritative LIVE_STATE_BUCKET')
    args.state_bucket = args.state_bucket or configured_bucket or f'bmux-live-state-{args.state_account_id}-production'
    args.state_role = args.state_role or 'bmux-live-production'
    required = ('state_region', 'state_bucket', 'state_role', 'state_account_id', 'state_dir')
    if any(not getattr(args, key, None) for key in required):
        raise BootstrapError('--bootstrap-state requires --state-region/bucket/role/account-id/dir')
    if not args.state_dir.is_absolute() or not re.fullmatch(r'[0-9]{12}', args.state_account_id):
        raise BootstrapError('Backend requires private absolute --state-dir and expected 12-digit account')
    base = [sys.executable, str(Path(__file__).with_name('provision-live-state.py')),
            '--repo', args.repo, '--region', args.state_region, '--bucket', args.state_bucket,
            '--role', args.state_role, '--account-id', args.state_account_id,
            '--state-dir', str(args.state_dir)]
    if getattr(args, 'approve_import', False):
        base.append('--approve-import')
    operation = getattr(args, 'state_operation', None)
    mode = getattr(args, 'state_mode', 'guided')
    if mode in ('apply', 'review', 'reconcile') and not operation:
        raise BootstrapError('State review/apply/reconcile requires exact --state-operation')
    if mode == 'apply' and not getattr(args, 'accept_cost', False):
        raise BootstrapError('State apply requires explicit --accept-cost')
    if mode == 'guided' and (not sys.stdin.isatty() or not sys.stderr.isatty()):
        raise BootstrapError('Guided backend setup requires TTY; use --state-mode plan then exact apply')
    dry = json.loads(run(base))
    validate_state_result(dry, args, dry.get('operation'), ('dryrun',), allow_empty_operation=True)
    if mode != 'review':
        verify_oidc(args.repo, run, getattr(args, 'configure_oidc', False))
    if mode == 'guided':
        if not sys.stdin.isatty() or not sys.stderr.isatty():
            raise BootstrapError('Guided backend setup requires TTY; use explicit --state-mode plan then apply --state-operation ID --accept-cost')
        planned = json.loads(run(base + ['--plan']))
        operation = planned.get('operation')
        if planned.get('schema') != 1 or not isinstance(operation, str) or not re.fullmatch(r'[A-Za-z0-9_-]{1,128}', operation):
            raise BootstrapError('Invalid backend plan identity; output suppressed')
        reviewed = json.loads(run(base + ['--review']))
        if reviewed.get('schema') != 1 or reviewed.get('operation') != operation:
            raise BootstrapError('Backend review does not match planned operation')
        validate_state_result(reviewed, args, operation, ('planned', 'complete'))
        print(json.dumps({key: reviewed[key] for key in ('schema', 'operation', 'phase', 'backend', 'role_arn', 'github_oidc', 'resources')}))
        print('Backend apply creates billable infrastructure. Review above; type exact operation ID to approve cost and apply:', file=sys.stderr)
        confirmed = sys.stdin.readline(130).strip()
        if confirmed != operation:
            raise BootstrapError('Backend apply cancelled; no mutation authorized')
        mode = 'apply'
        args.accept_cost = True
    flags = ['--' + mode] + ([operation] if mode in ('apply', 'reconcile') else [])
    if mode == 'apply':
        if not getattr(args, 'accept_cost', False):
            raise BootstrapError('State apply requires explicit --accept-cost')
        # Read exact retained plan first; apply only the same operation identity.
        reviewed = json.loads(run(base + ['--review']))
        validate_state_result(reviewed, args, operation, ('planned', 'complete'))
        flags.append('--accept-cost')
    result = json.loads(run(base + flags))
    if mode in ('plan', 'review'):
        validate_state_result(result, args, result.get('operation') if mode == 'plan' else operation, ('planned', 'complete'))
        print(json.dumps(result))
    if mode not in ('apply', 'reconcile'):
        print('State plan/review completed; review exact operation and explicitly --state-mode apply --state-operation ID --accept-cost')
        return {}
    validate_state_result(result, args, operation, ('complete',))
    backend = result['backend']
    return {'LIVE_STATE_BUCKET': backend['bucket'], 'LIVE_STATE_REGION': backend['region'],
            'LIVE_STATE_ROLE_ARN': result['role_arn']}


def provision(args, run=command, environ=None, cloudflare=None):
    environ = os.environ if environ is None else environ
    # Older direct API callers retain explicit environment-value behavior.
    if getattr(args, 'input_env', not hasattr(args, 'zone')):
        return provision_env(args, run, environ)
    selected = args.only or list(NORMAL_SECRET_NAMES)
    if len(selected) != len(set(selected)):
        raise BootstrapError('Duplicate --only selections rejected')
    if getattr(args, 'bootstrap_state', False) and not args.only:
        selected += list(STATE_SECRET_NAMES)
    if args.rotate and not args.only:
        raise BootstrapError('Rotation requires explicit --only selections; no blanket rotation')
    if args.dry_run:
        print(json.dumps({'repo': args.repo, 'environment': args.environment,
                          'mode': 'replace-selected' if args.rotate else 'skip-existing',
                          'secrets': selected, 'provider_requests': False,
                          'protections': 'verify only; never weaken',
                          'state_mode': getattr(args, 'state_mode', 'plan')}))
        return
    verify_environment(args.repo, args.environment, run)
    verify_environment(args.repo, 'production-approval', run)
    existing = {s['name'] for s in json.loads(run(['gh', 'secret', 'list', '--repo', args.repo,
                                                '--env', args.environment, '--json', 'name']))}
    pending = [name for name in selected if args.rotate or name not in existing]
    explicit_state = getattr(args, 'bootstrap_state', False)
    if not pending and not explicit_state:
        for name in selected:
            print(f'Skipped existing {name}')
        return
    if any(name in ('LIVE_STATE_ACCESS_KEY_ID', 'LIVE_STATE_SECRET_ACCESS_KEY') for name in pending):
        raise BootstrapError('Static backend credentials require explicit --input-env; normal setup uses --bootstrap-state OIDC')
    hostname = getattr(args, 'hostname', None) or 'bmux.dev'
    project = getattr(args, 'pages_project', None) or 'bmux'
    zone_name = getattr(args, 'zone', 'bmux.dev')
    if (not re.fullmatch(r'[a-z0-9](?:[a-z0-9.-]{0,251}[a-z0-9])?', hostname)
            or hostname != zone_name and not hostname.endswith('.' + zone_name)
            or not re.fullmatch(r'[a-z0-9][a-z0-9-]{0,57}', project)):
        raise BootstrapError('Invalid hostname/Pages project or hostname outside selected zone')
    if 'LIVE_PUBLISHER_KEY_SHA256' in pending and not args.publisher_key_file:
        raise BootstrapError('--publisher-key-file required; key is generated securely only at explicit path')
    state_pending = [name for name in pending if name in STATE_SECRET_NAMES]
    if state_pending and not getattr(args, 'bootstrap_state', False):
        raise BootstrapError('State metadata requires --bootstrap-state or advanced --input-env')
    state_values = state_bootstrap(args, run, list(STATE_SECRET_NAMES) if explicit_state else state_pending, environ)
    if explicit_state and not state_values:
        return  # Plan/review cannot create tokens, keys or install partial secrets.
    values = {}
    if 'LIVE_PUBLISHER_KEY_SHA256' in pending:
        values['LIVE_PUBLISHER_KEY_SHA256'] = private_key_digest(args.publisher_key_file)
    if any(name in pending for name in ('CLOUDFLARE_API_TOKEN', 'CLOUDFLARE_ACCOUNT_ID', 'CLOUDFLARE_ZONE_ID', 'LIVE_PAGES_PROJECT')):
        cf = cloudflare or Cloudflare(environ)
        account, zone = discover(cf, args)
        if 'LIVE_PAGES_PROJECT' in pending:
            values['LIVE_PAGES_PROJECT'] = pages_project(cf, account, project)
        if 'CLOUDFLARE_ACCOUNT_ID' in pending:
            values['CLOUDFLARE_ACCOUNT_ID'] = account
        if 'CLOUDFLARE_ZONE_ID' in pending:
            values['CLOUDFLARE_ZONE_ID'] = zone
        if 'CLOUDFLARE_API_TOKEN' in pending:
            groups = list(cf.items('/user/tokens/permission_groups'))
            policy = token_policy(account, zone, groups)
            policy['name'] = f'bmux:github:{args.repo}:{args.environment}:deployment'
            # No DELETE, including uncertain gh writes; never reuse an unrelated project token.
            result = cf.request('POST', '/user/tokens', policy)['result']
            value = result.get('value')
            if not isinstance(value, str) or not value or any(c.isspace() for c in value):
                raise BootstrapError('Token creation returned no valid credential; inspect provider state before retry')
            values['CLOUDFLARE_API_TOKEN'] = value
    if 'LIVE_HOSTNAME' in pending:
        values['LIVE_HOSTNAME'] = hostname
    state_pending = [name for name in pending if name in STATE_SECRET_NAMES]
    if state_pending and not getattr(args, 'bootstrap_state', False):
        raise BootstrapError('State metadata requires --bootstrap-state or advanced --input-env')
    values.update({key: value for key, value in state_values.items() if key in pending})
    # Plan/review intentionally leaves backend metadata pending until explicit apply.
    for name in pending:
        if name not in values and name not in STATE_SECRET_NAMES:
            raise BootstrapError('Selected secret has no validated input; no writes performed')
    for name in selected:
        if name in values:
            run(['gh', 'secret', 'set', name, '--repo', args.repo, '--env', args.environment], values[name])
            print(f'Set {name} in selected environment')
        elif name in existing:
            print(f'Skipped existing {name}')
    if args.rotate:
        print('Rotation installed; verify protected deployment before manually revoking old credentials')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', required=True)
    parser.add_argument('--environment', default='Production')
    parser.add_argument('--zone', default='bmux.dev')
    parser.add_argument('--hostname')
    parser.add_argument('--pages-project', default='bmux')
    parser.add_argument('--only', action='append', choices=tuple(dict.fromkeys(SECRET_NAMES + LEGACY_SECRET_NAMES)))
    parser.add_argument('--publisher-key-file', type=Path)
    parser.add_argument('--rotate', action='store_true')
    parser.add_argument('--dry-run', action='store_true')
    parser.add_argument('--input-env', action='store_true', help='Advanced supplied environment values (legacy static backend keys)')
    parser.add_argument('--bootstrap-state', action='store_true')
    parser.add_argument('--configure-oidc', action='store_true', help='Approve repo-wide OIDC subject customization; affects existing consumers')
    parser.add_argument('--state-mode', choices=('guided', 'plan', 'review', 'apply', 'reconcile'), default='guided')
    parser.add_argument('--state-operation')
    parser.add_argument('--state-region', default='us-east-1')
    parser.add_argument('--state-bucket')
    parser.add_argument('--state-role')
    parser.add_argument('--state-account-id')
    parser.add_argument('--state-dir', type=Path)
    parser.add_argument('--accept-cost', action='store_true')
    parser.add_argument('--approve-import', action='store_true')
    args = parser.parse_args()
    if not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', args.repo):
        parser.error('--repo must be OWNER/REPO')
    if not re.fullmatch(r'[A-Za-z0-9_. -]{1,64}', args.environment):
        parser.error('invalid environment name')
    try:
        provision(args)
    except BootstrapError as error:
        print(f'ERROR: {error}', file=sys.stderr)
        return 1
    except (ValueError, KeyError, TypeError, AttributeError):
        print('ERROR: Invalid provider response; output suppressed; inspect state before retry', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
