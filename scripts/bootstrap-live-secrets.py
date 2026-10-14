#!/usr/bin/env python3
"""Bootstrap scoped Cloudflare/R2 secrets without displaying credentials.

--bootstrap-state --state-dir /PRIVATE/STATE --state-mode plan/review/apply/reconcile
binds approval to an exact --state-operation; apply also requires --accept-cost.
Existing buckets require --approve-import. No AWS/OIDC bootstrap or automatic migration.
Plan/review never install partial secrets. --dry-run is fully provider-free.
"""
import importlib.util
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


NORMAL_SECRET_NAMES = (
    'CLOUDFLARE_API_TOKEN', 'CLOUDFLARE_ACCOUNT_ID', 'CLOUDFLARE_ZONE_ID',
    'LIVE_HOSTNAME', 'LIVE_PAGES_PROJECT', 'LIVE_PUBLISHER_KEY_SHA256',
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
         ('Workers Scripts Write', 'Pages Write', 'Workers R2 Storage Write')),
        ('com.cloudflare.api.account.zone', f'com.cloudflare.api.account.zone.{zone}',
         ('Workers Routes Write', 'DNS Write', 'Zone Read')),
    ):
        ids = []
        for name in names:
            matches = [g for g in permission_groups if g.get('name') == name and scope in g.get('scopes', [])]
            if len(matches) != 1 or not isinstance(matches[0].get('id'), str) or not re.fullmatch(r'[a-f0-9]{32}', matches[0]['id']):
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
    selected = args.only or list(SECRET_NAMES)
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
    if any(name in existing or environ.get(name) for name in ('LIVE_STATE_ROLE_ARN', 'LIVE_STATE_ACCESS_KEY_ID', 'LIVE_STATE_SECRET_ACCESS_KEY')):
        raise BootstrapError('Legacy AWS backend configured; manual state migration required')
    if environ.get('LIVE_STATE_REGION', 'auto') != 'auto':
        raise BootstrapError('Legacy backend region configured; manual migration required')
    if args.rotate and 'TOFU_STATE_ENCRYPTION_PASSPHRASE' in selected:
        raise BootstrapError('Passphrase rotation requires explicit encrypted state migration')
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
    # Validate coupled credentials and backend choices before any mutation.
    pair = [n for n in ('R2_STATE_ACCESS_KEY_ID', 'R2_STATE_SECRET_ACCESS_KEY') if n in pending]
    if pair and len(pair) != 2:
        raise BootstrapError('R2 credentials must be installed/rotated together')
    if any(n in pending for n in STATE_SECRET_NAMES):
        account = environ.get('CLOUDFLARE_ACCOUNT_ID')
        bucket = environ.get('LIVE_STATE_BUCKET')
        if not isinstance(account, str) or not re.fullmatch(r'[a-f0-9]{32}', account):
            raise BootstrapError('R2 state inputs require exact Cloudflare account identity')
        if not isinstance(bucket, str) or not re.fullmatch(r'[a-z0-9][a-z0-9-]{1,61}[a-z0-9]', bucket):
            raise BootstrapError('R2 state inputs require selected bucket identity')
        for name, pattern in (('R2_STATE_ACCESS_KEY_ID', r'[a-f0-9]{32}'),
                              ('R2_STATE_SECRET_ACCESS_KEY', r'[a-f0-9]{64}')):
            if name in values and not re.fullmatch(pattern, values[name]):
                raise BootstrapError('Invalid R2 S3 credential format')
        if 'TOFU_STATE_ENCRYPTION_PASSPHRASE' in values and len(values['TOFU_STATE_ENCRYPTION_PASSPHRASE']) < 32:
            raise BootstrapError('State encryption passphrase must be at least 32 characters')
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
STATE_SECRET_NAMES = ('LIVE_STATE_BUCKET', 'LIVE_STATE_REGION', 'R2_STATE_ACCESS_KEY_ID',
                      'R2_STATE_SECRET_ACCESS_KEY', 'TOFU_STATE_ENCRYPTION_PASSPHRASE')
SECRET_NAMES = NORMAL_SECRET_NAMES + STATE_SECRET_NAMES


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
    return account_id.lower(), zone_id.lower()


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


def validate_state_result(result, args, operation, phases, allow_empty_operation=False):
    backend = result.get('backend') if isinstance(result, dict) else None
    expected = dict(bucket=args.state_bucket, region='auto', key='bmux/live/production/terraform.tfstate',
                    endpoint=f'https://{args.state_account_id}.r2.cloudflarestorage.com', use_lockfile=True, encrypt=True)
    if (not isinstance(result, dict) or set(result) != {'schema', 'operation', 'phase', 'backend', 'resources'}
            or result['schema'] != 2 or result['operation'] != operation or result['phase'] not in phases
            or backend != expected or not (allow_empty_operation and operation is None
                or isinstance(operation, str) and re.fullmatch(r'[a-f0-9]{32}', operation))):
        raise BootstrapError('Invalid/incompatible R2 backend public response')
    rows = result['resources']
    if rows not in ([], [{'address': 'cloudflare_r2_bucket.state', 'actions': ['create'], 'import': False}],
                    [{'address': 'cloudflare_r2_bucket.state', 'actions': ['no-op'], 'import': True}]):
        raise BootstrapError('Invalid backend resource review')


def state_bootstrap(args, run, pending, environ):
    if not pending and not getattr(args, 'bootstrap_state', False):
        return {}
    configured = environ.get('LIVE_STATE_BUCKET')
    if args.state_bucket and configured and args.state_bucket != configured:
        raise BootstrapError('Configured bucket differs; refusing competing backend')
    args.state_bucket = args.state_bucket or configured or 'bmux-opentofu-state'
    if args.state_region != 'auto' or environ.get('LIVE_STATE_REGION', 'auto') != 'auto':
        raise BootstrapError('Legacy AWS region configured; manual state migration required')
    if not args.state_dir or not args.state_dir.is_absolute():
        raise BootstrapError('--bootstrap-state requires absolute private --state-dir')
    base = [sys.executable, str(Path(__file__).with_name('provision-live-state.py')), '--repo', args.repo,
            '--region', 'auto', '--bucket', args.state_bucket, '--account-id', args.state_account_id,
            '--state-dir', str(args.state_dir)]
    if args.approve_import:
        base.append('--approve-import')
    if getattr(args, 'state_completion_file', None):
        base.extend(['--completion-file', str(args.state_completion_file)])
    completion_base = base
    if '--completion-file' in base:
        i = base.index('--completion-file')
        base = base[:i] + base[i + 2:]
    mode, operation = args.state_mode, args.state_operation
    if mode in ('review', 'apply', 'reconcile') and not operation:
        raise BootstrapError('State review/apply/reconcile requires exact --state-operation')
    if operation and not re.fullmatch(r'[a-f0-9]{32}', operation):
        raise BootstrapError('Invalid state operation identity')
    if (operation and (args.state_dir / 'github-environments' / (operation + '.json')).exists()
            and not getattr(args, '_github', None)):
        raise BootstrapError('This operation includes GitHub setup; retain --approval-reviewer selection')
    if mode == 'apply' and not args.accept_cost:
        raise BootstrapError('State apply requires explicit --accept-cost')
    fresh_backend = False
    if mode == 'plan':
        result = json.loads(run(base + ['--plan']))
        validate_state_result(result, args, result.get('operation'), ('planned',))
        if getattr(args, '_github', None):
            args._github.plan(result['operation'])
    else:
        review_base = base
        reviewed = json.loads(run(review_base + ['--review', operation])) if mode != 'reconcile' else None
        if reviewed:
            validate_state_result(reviewed, args, operation, ('planned', 'complete'))
        completion = getattr(args, 'state_completion_file', None) or args.state_dir / ('completion-' + operation + '.json')
        if mode == 'apply' and completion.exists():
            # An existing receipt must match the original completed operation before any GitHub mutation.
            receipt = json.loads(run(base + ['--review', operation, '--completion-file', str(completion)]))
            validate_state_result(receipt, args, operation, ('complete',))
        if getattr(args, '_github', None):
            args._github.execute(operation, mode)
        flags = ['--' + mode, operation]
        if mode == 'apply':
            flags.append('--accept-cost')
        if mode == 'apply' and reviewed['phase'] == 'complete':
            result = json.loads(run(base + ['--review', operation])) if getattr(args, 'state_completion_file', None) else reviewed
        else:
            result = reviewed if mode == 'review' else json.loads(run(base + flags))
            fresh_backend = (mode == 'apply' and reviewed['phase'] == 'planned'
                             and reviewed['resources'] == [{'address': 'cloudflare_r2_bucket.state', 'actions': ['create'], 'import': False}])
        validate_state_result(result, args, operation, ('planned', 'complete') if mode == 'review' else ('complete',))
    # Public metadata only; no credentials are ever returned by the companion.
    print(json.dumps(result, sort_keys=True))
    if mode in ('plan', 'review'):
        return {}
    if fresh_backend:
        completion = getattr(args, 'state_completion_file', None) or args.state_dir / ('completion-' + operation + '.json')
        receipt = json.loads(run(base + ['--review', operation, '--completion-file', str(completion)]))
        validate_state_result(receipt, args, operation, ('complete',))
        args._completion_file = completion
    elif getattr(args, 'state_completion_file', None) and mode == 'review':
        receipt = json.loads(run(completion_base + ['--review', operation]))
        validate_state_result(receipt, args, operation, ('complete',))
    return dict(LIVE_STATE_BUCKET=args.state_bucket, LIVE_STATE_REGION='auto', _fresh_backend=fresh_backend)


def r2_token_policy(account, bucket, groups):
    # Cloudflare documented R2 bucket object read/write scope, not account-admin.
    matches = [g for g in groups if g.get('name') == 'Workers R2 Storage Bucket Item Write'
               and 'com.cloudflare.edge.r2.bucket' in g.get('scopes', [])]
    if len(matches) != 1 or not re.fullmatch(r'[a-f0-9]{32}', matches[0].get('id', '')):
        raise BootstrapError('Missing/ambiguous bucket-scoped R2 object permission')
    return [dict(effect='allow', resources={f'com.cloudflare.edge.r2.bucket.{account}_default_{bucket}': '*'},
                 permission_groups=[dict(id=matches[0]['id'])])]


def r2_credentials(cf, account, bucket):
    policy = r2_token_policy(account, bucket, list(cf.items('/user/tokens/permission_groups')))
    data = cf.request('POST', '/user/tokens', dict(name='bmux-live-production-state', policies=policy))
    result = data.get('result', {})
    key, value = result.get('id'), result.get('value')
    if (not isinstance(key, str) or not re.fullmatch(r'[a-f0-9]{32}', key)
            or not isinstance(value, str) or not 20 <= len(value) <= 1024 or any(c.isspace() for c in value)):
        raise BootstrapError('Invalid R2 token result; output suppressed; inspect tokens before retry')
    # https://developers.cloudflare.com/r2/api/tokens/ : Access key=token ID,
    # secret key=SHA-256 of token value. Raw token is never retained or printed.
    return dict(R2_STATE_ACCESS_KEY_ID=key, R2_STATE_SECRET_ACCESS_KEY=hashlib.sha256(value.encode()).hexdigest())


def provision(args, run=command, environ=None, cloudflare=None):
    if not getattr(args, 'approval_reviewer', None) or args.dry_run:
        return provision_selected(args, run, environ, cloudflare)
    if getattr(args, 'input_env', False) or not getattr(args, 'bootstrap_state', False):
        raise BootstrapError('--approval-reviewer requires --bootstrap-state, not --input-env')
    if not args.state_dir or not args.state_dir.is_absolute():
        raise BootstrapError('Environment setup requires absolute private --state-dir')
    spec = importlib.util.spec_from_file_location('live_github', Path(__file__).with_name('live-github-environments.py'))
    github = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(github)
    try:
        github.local.private_dir(args.state_dir)
        with github.local.lock(args.state_dir / 'github-environments'):
            args._github = github.Environments(args, run)
            return provision_selected(args, run, environ, cloudflare)
    except github.Error as error:
        raise BootstrapError(str(error)) from None


def provision_selected(args, run=command, environ=None, cloudflare=None):
    environ = os.environ if environ is None else environ
    if getattr(args, 'input_env', False):
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
                          'protections': ('plan/review/apply master-only environments; explicit conflict approval'
                                          if getattr(args, 'approval_reviewer', None) else 'verify only; never weaken'),
                          'approval_reviewer': getattr(args, 'approval_reviewer', None),
                          'state_mode': getattr(args, 'state_mode', 'plan')}))
        return
    github = getattr(args, '_github', None)
    if github is None:
        verify_environment(args.repo, args.environment, run)
        verify_environment(args.repo, 'production-approval', run)
    existing = ({s['name'] for s in json.loads(run(['gh', 'secret', 'list', '--repo', args.repo,
                                                 '--env', args.environment, '--json', 'name']))}
                if github is None or github.current[args.environment] is not None else set())
    pending = [name for name in selected if args.rotate or name not in existing]
    if any(name in existing or environ.get(name) for name in ('LIVE_STATE_ROLE_ARN', 'LIVE_STATE_ACCESS_KEY_ID', 'LIVE_STATE_SECRET_ACCESS_KEY')):
        raise BootstrapError('Legacy AWS backend configured; manual state migration required')
    if environ.get('LIVE_STATE_REGION', 'auto') != 'auto':
        raise BootstrapError('Legacy backend region configured; manual migration required')
    if args.rotate and 'TOFU_STATE_ENCRYPTION_PASSPHRASE' in selected:
        raise BootstrapError('Passphrase rotation requires explicit encrypted state migration')
    explicit_state = getattr(args, 'bootstrap_state', False)
    if not pending and not explicit_state:
        for name in selected:
            print(f'Skipped existing {name}')
        return
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
    if getattr(args, 'state_completion_file', None) and args.state_mode == 'plan':
        raise BootstrapError('Completion file requires a completed exact operation, not plan')
    pair = [n for n in ('R2_STATE_ACCESS_KEY_ID', 'R2_STATE_SECRET_ACCESS_KEY') if n in pending]
    if pair and len(pair) != 2:
        raise BootstrapError('R2 credentials must be installed/rotated together; select both with --only')
    if getattr(args, 'approve_import', False) and 'TOFU_STATE_ENCRYPTION_PASSPHRASE' in pending:
        raise BootstrapError('Existing backend requires original passphrase via --input-env, never replacement generation')
    if args.state_mode == 'apply':
        if 'TOFU_STATE_ENCRYPTION_PASSPHRASE' in pending:
            passphrase = environ.get('TOFU_STATE_ENCRYPTION_PASSPHRASE')
            if not isinstance(passphrase, str) or len(passphrase) < 32 or '\n' in passphrase or '\r' in passphrase:
                raise BootstrapError('Before apply securely supply original/high-entropy TOFU_STATE_ENCRYPTION_PASSPHRASE; no provider mutation performed')
        completion = getattr(args, 'state_completion_file', None) or args.state_dir / ('completion-' + str(args.state_operation) + '.json')
        if not completion.is_absolute() or completion.is_symlink():
            raise BootstrapError('Completion file must be explicit private absolute file before apply')
        if completion.exists():
            info = completion.lstat()
            if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
                raise BootstrapError('Existing completion must be an owned private regular file')
        info = completion.parent.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise BootstrapError('Completion parent must be owned private directory before apply')
    state_values = {}
    cf = cloudflare
    account = zone = None
    if explicit_state or state_pending:
        cf = cf or Cloudflare(environ)
        account, zone = discover(cf, args)
        configured_account = environ.get('CLOUDFLARE_ACCOUNT_ID')
        if (args.state_account_id and args.state_account_id != account
                or configured_account and configured_account != account):
            raise BootstrapError('Discovered account differs from selected backend identity')
        args.state_account_id = account
        state_values = state_bootstrap(args, run, list(STATE_SECRET_NAMES), environ)
        if not state_values:
            return  # Plan/review cannot create tokens, keys or install partial secrets.
        if 'TOFU_STATE_ENCRYPTION_PASSPHRASE' in pending:
            passphrase = environ.get('TOFU_STATE_ENCRYPTION_PASSPHRASE')
            if passphrase:
                if len(passphrase) < 32 or '\n' in passphrase or '\r' in passphrase:
                    raise BootstrapError('Invalid supplied original encryption passphrase')
            else:
                raise BootstrapError('Original encryption passphrase required; never generate replacement')
            if len(passphrase) < 32 or '\n' in passphrase or '\r' in passphrase:
                raise BootstrapError('Invalid original encryption passphrase')
            state_values['TOFU_STATE_ENCRYPTION_PASSPHRASE'] = passphrase
        credential_pending = [n for n in ('R2_STATE_ACCESS_KEY_ID', 'R2_STATE_SECRET_ACCESS_KEY') if n in pending]
        if credential_pending:
            if len(credential_pending) != 2:
                raise BootstrapError('R2 credentials must be installed/rotated together; select both with --only')
            state_values.update(r2_credentials(cf, account, args.state_bucket))
    values = {}
    if 'LIVE_PUBLISHER_KEY_SHA256' in pending:
        values['LIVE_PUBLISHER_KEY_SHA256'] = private_key_digest(args.publisher_key_file)
    if any(name in pending for name in ('CLOUDFLARE_API_TOKEN', 'CLOUDFLARE_ACCOUNT_ID', 'CLOUDFLARE_ZONE_ID', 'LIVE_PAGES_PROJECT')):
        cf = cf or Cloudflare(environ)
        if account is None:
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
        if name not in values:
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
    parser.add_argument('--approval-reviewer', help='GitHub user login; manage environments in the state plan/review/apply')
    parser.add_argument('--approve-environment-changes', action='store_true',
                        help='Explicitly approve planned changes to existing GitHub environment protections')
    parser.add_argument('--zone', default='bmux.dev')
    parser.add_argument('--hostname')
    parser.add_argument('--pages-project', default='bmux')
    parser.add_argument('--only', action='append', choices=SECRET_NAMES)
    parser.add_argument('--publisher-key-file', type=Path)
    parser.add_argument('--rotate', action='store_true')
    parser.add_argument('--dry-run', action='store_true')
    parser.add_argument('--input-env', action='store_true', help='Advanced supplied R2 environment values')
    parser.add_argument('--bootstrap-state', action='store_true')
    parser.add_argument('--state-mode', choices=('plan', 'review', 'apply', 'reconcile'), default='plan')
    parser.add_argument('--state-operation')
    parser.add_argument('--state-region', default='auto')
    parser.add_argument('--state-bucket')
    parser.add_argument('--state-account-id')
    parser.add_argument('--state-dir', type=Path)
    parser.add_argument('--state-completion-file', type=Path)
    parser.add_argument('--accept-cost', action='store_true')
    parser.add_argument('--approve-import', action='store_true')
    args = parser.parse_args()
    if not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', args.repo):
        parser.error('--repo must be OWNER/REPO')
    if not re.fullmatch(r'[A-Za-z0-9_. -]{1,64}', args.environment):
        parser.error('invalid environment name')
    if args.approval_reviewer and (not args.bootstrap_state or args.input_env):
        parser.error('--approval-reviewer requires --bootstrap-state, not --input-env')
    if args.approve_environment_changes and not args.approval_reviewer:
        parser.error('--approve-environment-changes requires --approval-reviewer')
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
