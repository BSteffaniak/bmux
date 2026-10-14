#!/usr/bin/env python3
"""Plugin-owned live installer. No mutation without an exact reviewed operation ID."""
import argparse
import contextlib
import fcntl
import getpass
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
import tarfile
import tempfile

HERE = Path(__file__).resolve().parent
FIELDS = {
    'environment': 'LIVE_ENVIRONMENT', 'account_id': 'CLOUDFLARE_ACCOUNT_ID',
    'zone_id': 'CLOUDFLARE_ZONE_ID', 'hostname': 'LIVE_HOSTNAME',
    'pages_project': 'LIVE_PAGES_PROJECT', 'publisher_key_sha256': 'LIVE_PUBLISHER_KEY_SHA256',
    'bucket': 'LIVE_STATE_BUCKET', 'region': 'LIVE_STATE_REGION',
}
SECRETS = ('CLOUDFLARE_API_TOKEN', 'LIVE_STATE_ACCESS_KEY_ID', 'LIVE_STATE_SECRET_ACCESS_KEY')
ADDRESSES = ('cloudflare_pages_project.live', 'cloudflare_pages_domain.live',
             'cloudflare_dns_record.live', 'cloudflare_worker.live',
             'cloudflare_worker_version.live', 'cloudflare_workers_deployment.live',
             'cloudflare_workers_route.live')


class InstallError(Exception):
    """Safe, actionable error. Never include external process or response text."""


def digest(path):
    hasher = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            hasher.update(block)
    return hasher.hexdigest()


def sync_file(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    try:
        if not stat.S_ISREG(os.fstat(fd).st_mode):
            raise InstallError('Durable asset is not a regular file')
        os.fsync(fd)
    finally:
        os.close(fd)


def identity(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def private_dir(path):
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise InstallError('Deployment state directory must be owned, non-symlink, mode 0700')


def private_read(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd) as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077 or info.st_size > 16 * 1024 * 1024:
            raise InstallError('Deployment files must be bounded owned regular files, mode 0600')
        return json.load(stream)


def save(path, value):
    temp = path.with_name('.' + path.name + '.' + uuid.uuid4().hex)
    fd = os.open(temp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'w') as stream:
        json.dump(value, stream, sort_keys=True)
        stream.write('\n')
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temp, path)
    fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


@contextlib.contextmanager
def lock(root):
    private_dir(root)
    fd = os.open(root / 'lock', os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    try:
        info = os.fstat(fd)
        if info.st_uid != os.getuid() or not stat.S_ISREG(info.st_mode) or info.st_mode & 0o077:
            raise InstallError('Unsafe deployment lock')
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise InstallError('Another installer owns this profile; wait, never force-unlock') from None
        yield
    finally:
        os.close(fd)


def validate_profile(value):
    if set(value) != {'schema', 'choices'} or value['schema'] != 1 or set(value['choices']) != set(FIELDS):
        raise InstallError('Unsupported/corrupt deployment profile; credentials never belong here')
    c = value['choices']
    patterns = {'environment': r'staging|production', 'account_id': r'[a-f0-9]{32}',
                'zone_id': r'[a-f0-9]{32}', 'hostname': r'(?=.{1,253}$)[a-z0-9]+(?:[.-][a-z0-9]+)*\.[a-z]{2,63}',
                'pages_project': r'[a-z0-9][a-z0-9-]{0,57}', 'publisher_key_sha256': r'[a-f0-9]{64}',
                'bucket': r'[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]', 'region': r'[a-z]{2}-[a-z]+-\d'}
    for key, pattern in patterns.items():
        if not isinstance(c[key], str) or not re.fullmatch(pattern, c[key]):
            raise InstallError('Invalid deployment choice: ' + key)
    return c


def select_provider_defaults():
    """Explicit read-only authenticated discovery; select public identities, never persist token."""
    print('Cloudflare: create a scoped token with account Workers/Pages and zone DNS access; SQLite DO and billing require your account approval.')
    if input('Discover accounts and active zones using your token (read-only)? Type DISCOVER: ') != 'DISCOVER':
        return {}
    token = os.environ.get('CLOUDFLARE_API_TOKEN') or getpass.getpass('Cloudflare scoped API token (memory only): ')
    tools = Tools(Path('/'), {'CLOUDFLARE_API_TOKEN': token})
    accounts = tools.cf_list('accounts')
    if not accounts:
        raise InstallError('No authorized Cloudflare account; check token account scope')
    def choose(items, label):
        for number, item in enumerate(items, 1):
            # Remote names can contain terminal escapes; display only checked stable identities.
            if not re.fullmatch(r'[a-f0-9]{32}', item.get('id', '')):
                raise InstallError('Unsupported provider account/zone identity')
            print(f'{number}. {label} {item["id"]}')
        selection = input(f'Select {label} number: ').strip()
        if not selection.isdigit() or not 1 <= int(selection) <= len(items):
            raise InstallError('Invalid provider selection')
        return items[int(selection) - 1]
    account = choose(accounts, 'account')
    zones = [z for z in tools.cf_list('zones?account.id=' + account['id']) if z.get('status') == 'active']
    if not zones:
        raise InstallError('No active delegated zone; complete Cloudflare/registrar zone activation')
    zone = choose(zones, 'zone')
    if not re.fullmatch(r'[a-z0-9.-]{1,253}', zone.get('name', '')):
        raise InstallError('Unsupported DNS zone name')
    return {'account_id': account['id'], 'zone_id': zone['id'], 'hostname': 'live.' + zone['name']}


def setup(path, interactive=True):
    old = validate_profile(private_read(path)) if path.exists() else {}
    choices = {}
    discovered = select_provider_defaults() if interactive and not old and not os.environ.get('CLOUDFLARE_ACCOUNT_ID') else {}
    for key, env_key in FIELDS.items():
        default = os.environ.get(env_key, old.get(key, discovered.get(key, '')))
        if interactive:
            defaults = {'environment': 'staging', 'pages_project': 'bmux-live-staging', 'region': 'us-east-1'}
            default = default or defaults.get(key, '')
            answer = input(f'{key} [{default}]: ').strip()
            choices[key] = answer or default
        else:
            choices[key] = default
    value = {'schema': 1, 'choices': choices}
    validate_profile(value)
    save(path, value)
    return choices


def verify_artifact(root):
    if not root.is_absolute() or root.is_symlink():
        raise InstallError('Candidate must be an absolute, non-symlink directory')
    manifest = root / 'SHA256SUMS'
    files = set()
    try:
        lines = manifest.read_text().splitlines()
        for line in lines:
            match = re.fullmatch(r'([a-f0-9]{64})  (?:\./)?(.+)', line)
            if not match:
                raise InstallError('Invalid artifact checksum inventory')
            expected, relative = match.groups()
            path = Path(relative)
            if path.is_absolute() or '..' in path.parts or relative in files:
                raise InstallError('Unsafe/duplicate artifact inventory entry')
            files.add(relative)
            target = root / path
            if any(part.is_symlink() for part in [target, *target.parents] if part != root.parent):
                raise InstallError('Artifact contains symlink assets')
            if not target.is_file() or digest(target) != expected:
                raise InstallError('Artifact integrity mismatch; reinstall exact candidate')
        if any(path.is_symlink() for path in root.rglob('*')):
            raise InstallError('Artifact contains symlink assets')
        actual = {str(p.relative_to(root)) for p in root.rglob('*') if p.is_file() and p != manifest}
        if actual != files:
            raise InstallError('Artifact has unlisted or missing files')
        for required in ('REVISION', 'infrastructure/main.tf', 'infrastructure/.terraform.lock.hcl',
                         'infrastructure/bootstrap/main.tf', 'infrastructure/bootstrap/.terraform.lock.hcl',
                         'tools/node_modules/wrangler/bin/wrangler.js'):
            if required not in files:
                raise InstallError('Candidate missing required asset: ' + required)
        if not re.fullmatch(r'[a-f0-9]{40,64}\n?', (root / 'REVISION').read_text()):
            raise InstallError('Invalid immutable candidate revision')
        if not any(p.startswith('site/') for p in files) or not any(p.startswith('relay/') for p in files):
            raise InstallError('Candidate missing compiled site or relay')
        return digest(manifest)
    except OSError:
        raise InstallError('Candidate unavailable at ' + json.dumps(str(root), ensure_ascii=True) +
                           '; run bmux broadcast deploy --setup to select an approved packaged-release HTTPS URL and independently trusted archive SHA-256, or select --artifact ABSOLUTE_PATH. No repository build is required.') from None


class Tools:
    def __init__(self, artifact, env):
        self.artifact = artifact
        self.env = env
        self.tofu = shutil.which('tofu')
        self.node = shutil.which('node')
        self.aws = shutil.which('aws')

    def run(self, args, cwd=None, codes=(0,), timeout=900):
        try:
            result = subprocess.run(args, cwd=cwd, env=self.env, stdin=subprocess.DEVNULL,
                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout, check=False)
        except (OSError, subprocess.TimeoutExpired):
            raise InstallError('Tool unavailable/timed out; interrupted mutations require reconciliation') from None
        if len(result.stdout) > 32 * 1024 * 1024 or result.returncode not in codes:
            raise InstallError('Provider operation failed; output suppressed; check authentication, scope and locked state')
        return result

    def check(self, bootstrap=False):
        if not self.tofu:
            raise InstallError('Install OpenTofu 1.10.9 (provider signatures verified by packaged lockfiles)')
        version = json.loads(self.run([self.tofu, 'version', '-json']).stdout)['terraform_version']
        if version != '1.10.9':
            raise InstallError('Use pinned OpenTofu 1.10.9; do not upgrade provider locks during deployment')
        if not self.aws:
            raise InstallError('Install AWS CLI v2 for account/bucket/IAM preflight and secure bootstrap identity attachment')
        if not bootstrap:
            if not self.node:
                raise InstallError('Install Node.js 22 for pinned Wrangler Pages-only upload')
            if not self.run([self.node, '--version']).stdout.decode().startswith('v22.'):
                raise InstallError('Use Node.js 22 for packaged Wrangler')

    def tofu_run(self, workspace, *args, codes=(0,)):
        return self.run([self.tofu, *args], cwd=workspace, codes=codes)

    def cf(self, path):
        request = urllib.request.Request('https://api.cloudflare.com/client/v4/' + path,
                                        headers={'Authorization': 'Bearer ' + self.env['CLOUDFLARE_API_TOKEN']})
        try:
            # Never follow redirects with an authorization header.
            class NoRedirect(urllib.request.HTTPRedirectHandler):
                def redirect_request(self, req, fp, code, msg, headers, newurl):
                    return None
            with urllib.request.build_opener(NoRedirect).open(request, timeout=30) as response:
                raw = response.read(4 * 1024 * 1024 + 1)
                if len(raw) > 4 * 1024 * 1024:
                    raise InstallError('Provider inventory exceeds bounded response')
                body = json.loads(raw)
            if not body.get('success'):
                raise InstallError('Cloudflare preflight rejected; check account, zone, scoped token and DO entitlement')
            return body
        except (urllib.error.URLError, ValueError, KeyError):
            raise InstallError('Cloudflare preflight unavailable; check account/zone token scopes, DNS and network') from None

    def cf_list(self, path):
        result = []
        for page in range(1, 101):
            body = self.cf(path + ('&' if '?' in path else '?') + f'per_page=100&page={page}')
            items = body['result']
            if isinstance(items, dict) and 'workers' in items:
                items = items['workers']
            if not isinstance(items, list):
                raise InstallError('Unsupported provider inventory response; refusing guessed empty state')
            result.extend(items)
            info = body.get('result_info', {})
            if info.get('total_pages') is not None and page >= info['total_pages']:
                return result
            if not info and len(items) < 100:
                return result
        raise InstallError('Provider inventory exceeds page limit; narrow account resources')


def preflight(tools, choices, bootstrap=False):
    tools.check(bootstrap)
    region = choices['region']
    tools.env['AWS_REGION'] = region
    tools.env['AWS_DEFAULT_REGION'] = region
    account = json.loads(tools.run([tools.aws, 'sts', 'get-caller-identity', '--output', 'json']).stdout).get('Account')
    if not isinstance(account, str) or not re.fullmatch(r'\d{12}', account):
        raise InstallError('AWS authoritative account identity unavailable')
    if bootstrap:
        return account
    token = tools.cf('user/tokens/verify')['result']
    if token.get('status') != 'active':
        raise InstallError('Cloudflare token is not active; create a scoped deployment token')
    tools.cf('accounts/' + choices['account_id'])
    zone = tools.cf('zones/' + choices['zone_id'])['result']
    if zone.get('account', {}).get('id') != choices['account_id'] or zone.get('status') != 'active':
        raise InstallError('Zone must be active and belong to selected account; complete registrar delegation')
    if choices['hostname'] != zone['name'] and not choices['hostname'].endswith('.' + zone['name']):
        raise InstallError('Hostname is outside the selected zone')
    for command in (
        ['get-bucket-location'], ['get-bucket-versioning'], ['get-public-access-block'], ['get-bucket-encryption']
    ):
        answer = json.loads(tools.run([tools.aws, 's3api', *command, '--bucket', choices['bucket'], '--output', 'json']).stdout)
        if command[0] == 'get-bucket-location' and (answer.get('LocationConstraint') or 'us-east-1') != choices['region']:
            raise InstallError('State bucket region mismatch')
        if command[0] == 'get-bucket-versioning' and answer.get('Status') != 'Enabled':
            raise InstallError('State bucket must have versioning enabled; use --bootstrap plan/review/apply')
        if command[0] == 'get-public-access-block' and not all(answer.get('PublicAccessBlockConfiguration', {}).get(k) is True for k in
                ('BlockPublicAcls', 'IgnorePublicAcls', 'BlockPublicPolicy', 'RestrictPublicBuckets')):
            raise InstallError('State bucket must block all public access')
        if command[0] == 'get-bucket-encryption' and not answer.get('ServerSideEncryptionConfiguration'):
            raise InstallError('State bucket encryption required')
    return account


def discover(tools, c):
    account, zone = c['account_id'], c['zone_id']
    specifications = [
        ('cloudflare_pages_project.live', f'accounts/{account}/pages/projects', 'name', c['pages_project']),
        ('cloudflare_dns_record.live', f'zones/{zone}/dns_records?name={c["hostname"]}', 'name', c['hostname']),
        ('cloudflare_worker.live', f'accounts/{account}/workers/workers', 'name', 'bmux-live-' + c['environment']),
        ('cloudflare_workers_route.live', f'zones/{zone}/workers/routes', 'pattern', c['hostname'] + '/api/live/*'),
    ]
    found = {}
    for address, endpoint, field, expected in specifications:
        matches = [item for item in tools.cf_list(endpoint) if item.get(field) == expected]
        if len(matches) > 1:
            raise InstallError('Multiple matching provider resources; reconcile explicitly before planning')
        if matches:
            item = matches[0]
            if address == 'cloudflare_pages_project.live':
                provider_id = f'{account}/{c["pages_project"]}'
            elif address in ('cloudflare_dns_record.live', 'cloudflare_workers_route.live'):
                provider_id = f'{zone}/{item["id"]}'
            else:
                provider_id = f'{account}/{item["id"]}'
            if not re.fullmatch(r'[a-zA-Z0-9_:/.-]{1,512}', provider_id):
                raise InstallError('Unsupported provider resource identity')
            found[address] = provider_id
    if 'cloudflare_pages_project.live' in found:
        items = tools.cf_list(f'accounts/{account}/pages/projects/{c["pages_project"]}/domains')
        if any(item.get('name') == c['hostname'] for item in items):
            found['cloudflare_pages_domain.live'] = f'{account}/{c["pages_project"]}/{c["hostname"]}'
    return found


def environment(choices, bootstrap=False, interactive=False):
    env = dict(os.environ)
    # TF_CLI_ARGS can weaken locking or alter plan/apply; provider logging can leak secrets.
    for key in list(env):
        if key.startswith(('TF_', 'TOFU_')) or key in ('AWS_ENDPOINT_URL', 'AWS_ENDPOINT_URL_S3'):
            del env[key]
    env['TF_IN_AUTOMATION'] = '1'
    env['CHECKPOINT_DISABLE'] = '1'
    env['WRANGLER_LOG'] = 'none'
    env['WRANGLER_SEND_METRICS'] = 'false'
    env['AWS_REGION'] = choices['region']
    env['AWS_DEFAULT_REGION'] = choices['region']
    if not bootstrap:
        for name in SECRETS:
            if not env.get(name) and interactive:
                env[name] = getpass.getpass(name + ' (used only in memory): ')
            if not env.get(name):
                raise InstallError('Missing ' + name + '; provide private environment or run interactively')
        env['AWS_ACCESS_KEY_ID'] = env['LIVE_STATE_ACCESS_KEY_ID']
        env['AWS_SECRET_ACCESS_KEY'] = env['LIVE_STATE_SECRET_ACCESS_KEY']
        for key in ('AWS_PROFILE', 'AWS_SESSION_TOKEN', 'AWS_CONFIG_FILE', 'AWS_SHARED_CREDENTIALS_FILE'):
            env.pop(key, None)
        if env.get('LIVE_STATE_SESSION_TOKEN'):
            env['AWS_SESSION_TOKEN'] = env['LIVE_STATE_SESSION_TOKEN']
        for key in ('environment', 'account_id', 'zone_id', 'hostname', 'pages_project', 'publisher_key_sha256'):
            env['TF_VAR_' + key] = choices[key]
    return env


def imports(values, bootstrap=False):
    result = {}
    for value in values:
        address, separator, provider_id = value.partition('=')
        allowed = address in ADDRESSES if not bootstrap else bool(re.fullmatch(
            r'aws_(?:s3_bucket|s3_bucket_versioning|s3_bucket_server_side_encryption_configuration|s3_bucket_public_access_block|s3_bucket_policy|iam_policy)\.(?:state|tls)\["(?:staging|production)"\]', address))
        if not separator or not allowed or address in result or not re.fullmatch(r'[a-zA-Z0-9_:/.-]{1,512}', provider_id):
            raise InstallError('Invalid explicit import ADDRESS=PROVIDER_ID')
        result[address] = provider_id
    return result


def review_summary(plan):
    result = []
    for resource in plan.get('resource_changes', []):
        address = resource['address']
        if not re.fullmatch(r'[a-zA-Z0-9_.\[\]"-]{1,200}', address):
            raise InstallError('Unsupported resource address in provider plan')
        actions = resource['change']['actions']
        if any(action not in ('no-op', 'read', 'create', 'update', 'delete') for action in actions):
            raise InstallError('Unsupported provider plan action')
        if 'delete' in actions:
            raise InstallError('Destructive replacement/deletion blocked; use reviewed advanced infrastructure workflow')
        result.append({'address': address, 'actions': actions,
                       'import': bool(resource.get('change', {}).get('importing'))})
    return result


class Deployment:
    def __init__(self, root, artifact, choices, bootstrap, tools):
        self.root, self.artifact, self.choices = root, artifact, choices
        self.bootstrap, self.tools = bootstrap, tools
        self.guided = False
        self.ledger_path = root / ('bootstrap-ledger.json' if bootstrap else 'ledger.json')
        self.artifact_id = verify_artifact(artifact)
        self.binding = identity({'choices': choices, 'artifact': self.artifact_id, 'bootstrap': bootstrap})
        self.ledger = private_read(self.ledger_path) if self.ledger_path.exists() else None
        if self.ledger is not None:
            phases = {'planning', 'planned', 'reviewed', 'applying', 'applied', 'pages-uploading', 'published', 'complete'}
            allowed = {'schema', 'operation', 'binding', 'phase', 'imports', 'started', 'cost_accepted', 'provider_account',
                       'plan_sha256', 'summary', 'outputs', 'abandoned', 'pages_operator_ack',
                       'policy_attachment', 'policy_attachment_confirmed'}
            if not isinstance(self.ledger, dict) or set(self.ledger) - allowed or self.ledger.get('phase') not in phases:
                raise InstallError('Unsupported/corrupt deployment operation ledger')
            if self.ledger.get('schema') != 1 or (self.ledger.get('binding') != self.binding and self.ledger['phase'] != 'complete'):
                raise InstallError('Resume choices/candidate differ from operation; restore original profile/artifact')
            if not re.fullmatch(r'[a-f0-9]{32}', self.ledger.get('operation', '')):
                raise InstallError('Corrupt operation identity')
            imported = self.ledger.get('imports')
            if not isinstance(imported, dict) or imports([k + '=' + v for k, v in imported.items()], bootstrap) != imported:
                raise InstallError('Corrupt saved import identities')
            if self.ledger['phase'] != 'planning' and not (self.ledger['phase'] == 'complete' and self.ledger.get('abandoned')):
                if not re.fullmatch(r'[a-f0-9]{64}', self.ledger.get('plan_sha256', '')) or not re.fullmatch(r'\d{12}', self.ledger.get('provider_account', '')):
                    raise InstallError('Corrupt plan/provider identity')
                summary = self.ledger.get('summary')
                if not isinstance(summary, list):
                    raise InstallError('Corrupt saved plan summary')
                for resource in summary:
                    if not isinstance(resource, dict) or set(resource) != {'address', 'actions', 'import'} or not isinstance(resource['import'], bool):
                        raise InstallError('Corrupt saved plan resource')
                    review_summary({'resource_changes': [{'address': resource['address'], 'change': {'actions': resource['actions']}}]})
        self.workspace = root / ('bootstrap-work' if bootstrap else 'deploy-work')

    def phase(self, phase, **extra):
        self.ledger.update(phase=phase, **extra)
        save(self.ledger_path, self.ledger)

    def exact(self, operation):
        if not self.ledger or operation != self.ledger['operation'] or self.ledger['binding'] != self.binding:
            raise InstallError('Approval must name the exact current operation ID')
        if verify_artifact(self.artifact) != self.artifact_id:
            raise InstallError('Candidate changed during operation')

    def init(self):
        private_dir(self.workspace)
        self.tools.env['TF_DATA_DIR'] = str(self.workspace / '.tofu')
        source = self.artifact / 'infrastructure'
        if self.bootstrap:
            source /= 'bootstrap'
            self.tools.env['TF_VAR_region'] = self.choices['region']
            suffix = '-' + self.choices['environment']
            if not self.choices['bucket'].endswith(suffix):
                raise InstallError('Bootstrap bucket must end in -staging or -production; both environments are created')
            self.tools.env['TF_VAR_bucket_prefix'] = self.choices['bucket'][:-len(suffix)]
        else:
            self.tools.env['TF_VAR_relay_directory'] = str(self.artifact / 'relay')
        for name in ('main.tf', '.terraform.lock.hcl'):
            embedded = HERE / (('bootstrap.tf' if name == 'main.tf' else 'bootstrap.lock.hcl') if self.bootstrap else name)
            if embedded.exists() and digest(source / name) != digest(embedded):
                raise InstallError('Candidate provider module/lock differs from installed authoritative identity; use matching release')
            shutil.copyfile(source / name, self.workspace / name)
            os.chmod(self.workspace / name, 0o600)
        args = ['init', '-input=false', '-lockfile=readonly']
        if not self.bootstrap:
            spec = importlib.util.spec_from_file_location('live_state', HERE / 'live-state.py')
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)
            save(self.workspace / 'backend.json', module.config(self.choices['environment'], self.choices['bucket'], self.choices['region']))
            args += ['-backend-config=' + str(self.workspace / 'backend.json')]
        self.tools.tofu_run(self.workspace, *args)
        if self.ledger and 'imports' in self.ledger:
            save(self.workspace / 'approved-imports.tf.json', {'import': [
                {'to': '${' + address + '}', 'id': provider_id}
                for address, provider_id in self.ledger['imports'].items()]})

    def plan(self, requested_imports, approved_imports, accept_cost):
        if self.ledger and self.ledger['phase'] not in ('planning', 'complete'):
            raise InstallError('Existing operation must be reviewed/applied or reconciled; refusing replacement')
        if requested_imports and not approved_imports:
            raise InstallError('Import requires explicit --approve-import; review exact plan before apply')
        if self.bootstrap and not accept_cost:
            raise InstallError('AWS bootstrap creates billable resources; explicit --accept-cost required for plan')
        if self.ledger and self.ledger['phase'] == 'complete':
            save(self.root / ('completed-' + self.ledger['operation'] + '.json'), self.ledger)
        self.ledger = {'schema': 1, 'operation': uuid.uuid4().hex, 'binding': self.binding,
                       'phase': 'planning', 'imports': requested_imports, 'started': time.time(),
                       'cost_accepted': bool(accept_cost)}
        self.phase('planning')
        provider_account = preflight(self.tools, self.choices, self.bootstrap)
        self.phase('planning', provider_account=provider_account)
        self.init()
        # Remove only a prior completed/failed planning operation's exact local import file.
        import_file = self.workspace / 'approved-imports.tf.json'
        save(import_file, {'import': [{'to': '${' + address + '}', 'id': value} for address, value in requested_imports.items()]})
        state = self.tools.tofu_run(self.workspace, 'state', 'list').stdout.decode().splitlines()
        if not self.bootstrap:
            found = discover(self.tools, self.choices)
            conflicts = [address for address in found if address in requested_imports and requested_imports[address] != found[address]]
            if conflicts:
                raise InstallError('Approved import ID does not match provider discovery: ' + ', '.join(conflicts))
            missing = sorted(set(found) - set(state) - set(requested_imports))
            if missing and self.guided:
                print('Existing provider resources are not tracked by this state. Exact proposed imports:')
                print(json.dumps({k: found[k] for k in missing}, indent=2))
                if input('Approve these imports in the REVIEWABLE PLAN only? Type IMPORT: ') == 'IMPORT':
                    requested_imports.update({k: found[k] for k in missing})
                    self.phase('planning', imports=requested_imports)
                    save(import_file, {'import': [{'to': '${' + address + '}', 'id': value} for address, value in requested_imports.items()]})
                    missing = []
            if missing:
                raise InstallError('Existing resources outside state: ' + json.dumps({k: found[k] for k in missing}, sort_keys=True) +
                                   '; select exact provider import IDs with --import and --approve-import (no duplication)')
        else:
            # HeadBucket is ambiguous on 403; never interpret unauthorized resources as absent.
            for env in ('staging', 'production'):
                bucket = self.tools.env['TF_VAR_bucket_prefix'] + '-' + env
                answer = self.tools.run([self.tools.aws, 's3api', 'list-buckets', '--output', 'json'])
                listed = json.loads(answer.stdout).get('Buckets')
                if not isinstance(listed, list):
                    raise InstallError('AWS bucket inventory unavailable')
                address = f'aws_s3_bucket.state["{env}"]'
                if any(item['Name'] == bucket for item in listed) and address not in state and address not in requested_imports:
                    raise InstallError('Existing bootstrap bucket requires explicit approved import: ' + address)
            policies = json.loads(self.tools.run([self.tools.aws, 'iam', 'list-policies', '--scope', 'Local', '--output', 'json']).stdout)
            for env in ('staging', 'production'):
                address = f'aws_iam_policy.state["{env}"]'
                if any(p['PolicyName'] == 'bmux-live-state-' + env for p in policies.get('Policies', [])) and address not in state and address not in requested_imports:
                    raise InstallError('Existing state policy requires explicit approved import: ' + address)
        plan_path = self.workspace / 'reviewed.plan'
        self.tools.tofu_run(self.workspace, 'plan', '-input=false', '-lock-timeout=5m', '-out=' + str(plan_path))
        os.chmod(plan_path, 0o600)
        sync_file(plan_path)
        plan = json.loads(self.tools.tofu_run(self.workspace, 'show', '-json', str(plan_path)).stdout)
        summary = review_summary(plan)
        self.phase('planned', plan_sha256=digest(plan_path), summary=summary)
        return self.ledger['operation']

    def review(self):
        if not self.ledger or self.ledger['phase'] not in ('planned', 'reviewed'):
            raise InstallError('Create a plan first; review never plans or applies')
        self.exact(self.ledger['operation'])
        if digest(self.workspace / 'reviewed.plan') != self.ledger['plan_sha256']:
            raise InstallError('Reviewed plan content changed; refusing approval')
        self.phase('reviewed')
        # Deliberately never print provider values, tokens, bindings or raw plan JSON.
        return {'operation': self.ledger['operation'], 'plan_sha256': self.ledger['plan_sha256'],
                'environment': self.choices['environment'], 'hostname': self.choices['hostname'],
                'bootstrap': self.bootstrap, 'resources': self.ledger['summary'],
                'imports': self.ledger['imports'], 'cost_accepted': self.ledger['cost_accepted']}

    def apply(self, operation, accept_cost):
        self.exact(operation)
        if self.bootstrap and not accept_cost:
            raise InstallError('Explicit --accept-cost required again to apply AWS bootstrap')
        phase = self.ledger['phase']
        if phase == 'complete':
            if self.ledger.get('abandoned'):
                raise InstallError('Abandoned operation cannot be applied; create a fresh reviewed plan')
            return
        if phase in ('applying', 'pages-uploading'):
            raise InstallError('Prior mutation outcome ambiguous; use --reconcile exact operation, never blind reapply')
        if phase not in ('reviewed', 'applied', 'published'):
            raise InstallError('Review exact saved plan before applying')
        if preflight(self.tools, self.choices, self.bootstrap) != self.ledger['provider_account']:
            raise InstallError('AWS account differs from planned authoritative identity')
        self.init()
        if phase == 'reviewed':
            if digest(self.workspace / 'reviewed.plan') != self.ledger['plan_sha256']:
                raise InstallError('Plan content differs from reviewed identity')
            self.phase('applying')  # durable BEFORE spawn, even if spawn/timeout fails
            self.tools.tofu_run(self.workspace, 'apply', '-input=false', '-lock-timeout=5m', str(self.workspace / 'reviewed.plan'))
            self.phase('applied')
        if self.bootstrap:
            outputs = json.loads(self.tools.tofu_run(self.workspace, 'output', '-json').stdout)
            self.phase('complete', outputs={'buckets': outputs['buckets']['value'],
                                           'state_policy_arns': outputs['state_policy_arns']['value']})
            return
        if self.ledger['phase'] == 'applied':
            self.phase('pages-uploading')
            self.tools.run([self.tools.node, str(self.artifact / 'tools/node_modules/wrangler/bin/wrangler.js'),
                            'pages', 'deploy', str(self.artifact / 'site'),
                            '--project-name=' + self.choices['pages_project'], '--branch=master',
                            '--commit-hash=' + (self.artifact / 'REVISION').read_text().strip()])
            self.phase('published')
        self.tools.run([sys.executable, '-I', str(HERE / 'smoke-live-broadcast.py'),
                        '--url', 'https://' + self.choices['hostname']])
        self.phase('complete')

    def reconcile(self, operation):
        self.exact(operation)
        if self.ledger['phase'] != 'applying':
            raise InstallError('Only ambiguous OpenTofu apply can be auto-reconciled; Pages ambiguity needs explicit --ack-pages OP after public smoke')
        if preflight(self.tools, self.choices, self.bootstrap) != self.ledger['provider_account']:
            raise InstallError('AWS account differs from ambiguous operation identity')
        self.init()
        # Never apply a reconciliation plan. Zero diff proves desired state, not a replay.
        result = self.tools.tofu_run(self.workspace, 'plan', '-input=false', '-lock-timeout=5m',
                                     '-detailed-exitcode', codes=(0, 2))
        if result.returncode != 0:
            raise InstallError('Provider state differs after ambiguous apply; inspect protected state and explicitly --abandon OP before a fresh reviewed plan')
        self.phase('applied')

    def abandon(self, operation):
        self.exact(operation)
        if self.ledger['phase'] not in ('planning', 'planned', 'reviewed', 'applying'):
            raise InstallError('Cannot abandon a Pages upload; reconcile publication separately')
        # Preserve the old identity permanently; a fresh plan gets a new operation ID.
        save(self.root / ('abandoned-' + operation + '.json'), self.ledger)
        self.phase('complete', abandoned=True)

    def attach_policy(self, operation, role, approved):
        self.exact(operation)
        if not self.bootstrap or self.ledger['phase'] != 'complete' or not approved:
            raise InstallError('Bootstrap complete and --approve-policy-attachment required for state role attachment')
        if not re.fullmatch(r'[A-Za-z0-9+=,.@_-]{1,64}', role):
            raise InstallError('Invalid existing AWS role name')
        if preflight(self.tools, self.choices, True) != self.ledger['provider_account']:
            raise InstallError('AWS account differs from bootstrap identity; refusing policy attachment')
        policy = self.ledger['outputs']['state_policy_arns'][self.choices['environment']]
        if not re.fullmatch(r'arn:aws:iam::' + self.ledger['provider_account'] + r':policy/[A-Za-z0-9+=,.@_/-]{1,512}', policy):
            raise InstallError('Corrupt bootstrap policy identity')
        intent = {'role': role, 'policy': policy}
        previous = self.ledger.get('policy_attachment')
        if previous and previous != intent:
            raise InstallError('Conflicting role/policy reuse of bootstrap operation')
        self.phase('complete', policy_attachment=intent)
        # AttachRolePolicy is idempotent; prove current state before retrying its exact identity.
        attached = json.loads(self.tools.run([self.tools.aws, 'iam', 'list-attached-role-policies', '--role-name', role, '--output', 'json']).stdout)
        if not any(p['PolicyArn'] == policy for p in attached.get('AttachedPolicies', [])):
            self.tools.run([self.tools.aws, 'iam', 'attach-role-policy', '--role-name', role, '--policy-arn', policy])
        attached = json.loads(self.tools.run([self.tools.aws, 'iam', 'list-attached-role-policies', '--role-name', role, '--output', 'json']).stdout)
        if not any(p['PolicyArn'] == policy for p in attached.get('AttachedPolicies', [])):
            raise InstallError('State policy attachment not confirmed; retry exact bootstrap identity')
        self.phase('complete', policy_attachment_confirmed=True)

    def ack_pages(self, operation):
        self.exact(operation)
        if self.ledger['phase'] != 'pages-uploading':
            raise InstallError('No ambiguous Pages upload to acknowledge')
        project = self.tools.cf(f'accounts/{self.choices["account_id"]}/pages/projects/{self.choices["pages_project"]}')['result']
        trigger = project.get('latest_deployment', {}).get('deployment_trigger', {}).get('metadata', {})
        if trigger.get('commit_hash') != (self.artifact / 'REVISION').read_text().strip() or trigger.get('branch') != 'master':
            raise InstallError('Authoritative Pages deployment does not match exact candidate revision/branch; do not acknowledge')
        self.tools.run([sys.executable, '-I', str(HERE / 'smoke-live-broadcast.py'), '--url', 'https://' + self.choices['hostname']])
        self.phase('published', pages_operator_ack=True)


def validate_release(url, expected):
    parsed = urllib.parse.urlsplit(url or '')
    if parsed.scheme != 'https' or not parsed.hostname or parsed.username or parsed.password or parsed.fragment or parsed.query:
        raise InstallError('Candidate URL must be credential-free HTTPS without query parameters')
    if not re.fullmatch(r'[a-f0-9]{64}', expected or ''):
        raise InstallError('Download requires independently approved --candidate-sha256')


def acquire_candidate(url, expected, root):
    """Explicit operator-selected immutable package; no redirect, traversal, links or builds."""
    validate_release(url, expected)
    private_dir(root)
    destination = root / expected
    if destination.exists():
        provenance = private_read(root / (expected + '.json'))
        if provenance != {'schema': 1, 'archive_sha256': expected, 'inventory_sha256': verify_artifact(destination)}:
            raise InstallError('Cached release differs from approved archive identity; reacquire exact release')
        return destination
    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, req, fp, code, msg, headers, newurl):
            return None
    with tempfile.TemporaryDirectory(dir=root) as directory:
        work = Path(directory)
        archive = work / 'candidate.tar.gz'
        hasher = hashlib.sha256()
        total = 0
        try:
            with urllib.request.build_opener(NoRedirect).open(url, timeout=60) as response, archive.open('wb') as stream:
                while True:
                    chunk = response.read(1024 * 1024)
                    if not chunk:
                        break
                    total += len(chunk)
                    if total > 1024 * 1024 * 1024:
                        raise InstallError('Candidate exceeds 1 GiB download limit')
                    hasher.update(chunk)
                    stream.write(chunk)
        except urllib.error.URLError:
            raise InstallError('Candidate download unavailable; verify approved release URL') from None
        if hasher.hexdigest() != expected:
            raise InstallError('Candidate package does not match approved SHA-256')
        unpacked = work / 'unpacked'
        private_dir(unpacked)
        with tarfile.open(archive, 'r:gz') as package:
            members = []
            size = 0
            for member in package:
                size += member.size
                if len(members) >= 100000 or size > 2 * 1024 * 1024 * 1024:
                    raise InstallError('Candidate archive exceeds extraction limits')
                members.append(member)
            seen = set()
            for member in members:
                path = Path(member.name)
                if path.is_absolute() or '..' in path.parts or not (member.isfile() or member.isdir()) or member.name in seen:
                    raise InstallError('Unsafe candidate archive member')
                seen.add(member.name)
            package.extractall(unpacked, members=members, filter='data')
        inventory_sha256 = verify_artifact(unpacked)
        for path in unpacked.rglob('*'):
            if path.is_file():
                sync_file(path)
        for path in [*sorted((p for p in unpacked.rglob('*') if p.is_dir()), reverse=True), unpacked]:
            fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(fd)
            finally:
                os.close(fd)
        os.rename(unpacked, destination)
        save(root / (expected + '.json'), {'schema': 1, 'archive_sha256': expected, 'inventory_sha256': inventory_sha256})
        return destination


def setup_release(path, url=None, checksum=None, interactive=False):
    release = private_read(path) if path.exists() else {}
    url = url or release.get('url', '')
    checksum = checksum or release.get('sha256', '')
    if interactive:
        print('Select an approved immutable BMUX live candidate release. Obtain its SHA-256 from your trusted release/protected CI channel, not an unsigned mutable URL response.')
        url = input(f'Approved immutable candidate HTTPS URL [{url}]: ').strip() or url
        checksum = input(f'Independently approved archive SHA-256 [{checksum}]: ').strip() or checksum
    if url or checksum:
        validate_release(url, checksum)
        save(path, {'schema': 1, 'url': url, 'sha256': checksum})


def guided_operation(deployment, requested_imports, approved_imports=False, accepted_cost=False):
    deployment.guided = True
    if deployment.ledger and deployment.ledger['phase'] in ('applying', 'pages-uploading'):
        raise InstallError('Interrupted mutation requires explicit reconciliation; operation ' + deployment.ledger['operation'])
    if not deployment.ledger or deployment.ledger['phase'] in ('planning', 'complete'):
        if deployment.bootstrap:
            accepted_cost = accepted_cost or input('AWS bootstrap creates billable resources. Type ACCEPT COST to plan: ') == 'ACCEPT COST'
        if input('Contact providers and create a read-only locked plan? Type PLAN: ') != 'PLAN':
            return False
        deployment.plan(requested_imports, approved_imports, accepted_cost)
    if deployment.ledger['phase'] in ('planned', 'reviewed'):
        print(json.dumps(deployment.review(), indent=2))
        operation = deployment.ledger['operation']
        if input('To approve provider changes and billing, type operation ' + operation + ': ') != operation:
            print('Plan saved; no apply. Resume using --apply ' + operation)
            return False
        deployment.apply(operation, deployment.bootstrap)
    elif deployment.ledger['phase'] in ('applied', 'published'):
        operation = deployment.ledger['operation']
        if input('Resume remaining approved phases? Type ' + operation + ': ') != operation:
            return False
        deployment.apply(operation, deployment.bootstrap)
    print('Deployment phase: ' + deployment.ledger['phase'])
    return deployment.ledger['phase'] == 'complete'


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--profile', default='default')
    parser.add_argument('--state-dir', type=Path, default=Path(os.environ.get('XDG_STATE_HOME', str(Path.home() / '.local/state'))) / 'bmux/broadcast-deploy')
    parser.add_argument('--artifact', type=Path, help='Explicit existing immutable candidate; never replaced by a release selection')
    parser.add_argument('--default-artifact', type=Path, help=argparse.SUPPRESS)
    parser.add_argument('--candidate-url', help='Explicit approved release archive HTTPS URL (saved without credentials)')
    parser.add_argument('--candidate-sha256', help='Independently approved release archive SHA-256')
    parser.add_argument('--state-role', help='Existing AWS role to attach the bootstrap environment policy to; requires explicit --approve-policy-attachment')
    parser.add_argument('--approve-policy-attachment', action='store_true')
    actions = parser.add_mutually_exclusive_group()
    actions.add_argument('--setup', action='store_true')
    actions.add_argument('--doctor', action='store_true')
    actions.add_argument('--plan', action='store_true')
    actions.add_argument('--review', action='store_true')
    actions.add_argument('--apply', metavar='OPERATION_ID')
    actions.add_argument('--reconcile', metavar='OPERATION_ID')
    actions.add_argument('--abandon', metavar='OPERATION_ID')
    actions.add_argument('--ack-pages', metavar='OPERATION_ID')
    parser.add_argument('--bootstrap', action='store_true', help='Authoritative AWS state bucket bootstrap, separate operation ledger')
    parser.add_argument('--accept-cost', action='store_true')
    parser.add_argument('--import', dest='imports', action='append', default=[])
    parser.add_argument('--approve-import', action='store_true')
    parser.add_argument('--non-interactive', action='store_true')
    args = parser.parse_args(argv)
    explicit_artifact = args.artifact is not None
    args.artifact = args.artifact or args.default_artifact
    if args.artifact is not None and not args.artifact.is_absolute():
        raise InstallError('Candidate path must be absolute; no cwd-relative deployment authority')
    if explicit_artifact and (args.candidate_url or args.candidate_sha256):
        raise InstallError('Choose either explicit --artifact or approved release URL/digest, not both')
    if not re.fullmatch(r'[a-zA-Z0-9_-]{1,64}', args.profile):
        raise InstallError('Profile name must be 1-64 letters, digits, underscore or hyphen')
    os.umask(0o077)
    root = args.state_dir.absolute() / args.profile
    with lock(root):
        profile_path = root / 'profile.json'
        interactive = sys.stdin.isatty() and not args.non_interactive
        if args.setup:
            setup(profile_path, interactive)
            setup_release(root / 'release.json', args.candidate_url, args.candidate_sha256, interactive)
            print('Named choices/release identity saved privately. Credentials must be supplied separately on every invocation.')
            return 0
        if args.doctor:
            if args.artifact is not None and (explicit_artifact or not (root / 'release.json').exists()):
                verify_artifact(args.artifact)
            elif (root / 'release.json').exists():
                release = private_read(root / 'release.json')
                if set(release) != {'schema', 'url', 'sha256'} or release['schema'] != 1:
                    raise InstallError('Corrupt saved release identity')
                validate_release(release['url'], release['sha256'])
                print('Approved release identity configured; candidate will be verified/acquired on explicit deploy (doctor does not download).')
            else:
                raise InstallError('Candidate unavailable; run guided deploy setup to select trusted immutable release URL and independently approved SHA-256, or select --artifact')
            Tools(args.artifact, dict(os.environ)).check(args.bootstrap)
            if profile_path.exists():
                validate_profile(private_read(profile_path))
            else:
                raise InstallError('Run deploy --setup to save account/domain/state choices; doctor never contacts providers')
            print('Local packaging/tools/profile valid. Provider auth, delegated zone, DO entitlement, state IAM access and billing remain online plan preflight gates.')
            return 0
        if not profile_path.exists():
            if not interactive:
                raise InstallError('Run deploy --setup (or --setup --non-interactive with advanced environment choices)')
            setup(profile_path)
        if interactive and not explicit_artifact and not (root / 'release.json').exists() and not (args.artifact is not None and args.artifact.exists()):
            setup_release(root / 'release.json', args.candidate_url, args.candidate_sha256, True)
        if args.candidate_url or args.candidate_sha256:
            validate_release(args.candidate_url, args.candidate_sha256)
            save(root / 'release.json', {'schema': 1, 'url': args.candidate_url, 'sha256': args.candidate_sha256})
        if not explicit_artifact and (root / 'release.json').exists():
            release = private_read(root / 'release.json')
            if set(release) != {'schema', 'url', 'sha256'} or release['schema'] != 1:
                raise InstallError('Corrupt saved release identity')
            args.artifact = acquire_candidate(release['url'], release['sha256'], root / 'candidates')
        if args.artifact is None:
            raise InstallError('No installed candidate or approved release identity; run guided deploy to select a trusted immutable release')
        choices = validate_profile(private_read(profile_path))
        no_action = not any((args.plan, args.review, args.apply, args.reconcile, args.abandon, args.ack_pages))
        if interactive and no_action and not args.bootstrap and not (root / 'ledger.json').exists():
            if input('Need new AWS state buckets/policies first? Type BOOTSTRAP, or Enter to use existing protected backend: ') == 'BOOTSTRAP':
                print('Bootstrap uses your separately authenticated AWS administrator identity (AWS SSO/profile or private temporary environment). No long-lived access keys are created or saved.')
                bootstrap_env = environment(choices, True, True)
                bootstrap = Deployment(root, args.artifact, choices, True, Tools(args.artifact, bootstrap_env))
                if not guided_operation(bootstrap, {}):
                    return 0
                role = input('Existing AWS state role name to attach the environment-scoped policy (Enter to skip): ').strip()
                if role:
                    print('Policy: ' + bootstrap.ledger['outputs']['state_policy_arns'][choices['environment']])
                    if input('Approve attachment to ' + role + '? Type ATTACH: ') == 'ATTACH':
                        bootstrap.attach_policy(bootstrap.ledger['operation'], role, True)
                print('State bootstrap completed. Supply the selected environment identity credentials privately for live plan; keep bootstrap local state backed up securely.')
        env = environment(choices, args.bootstrap, interactive) if (args.plan or args.apply or args.reconcile or args.ack_pages or (interactive and not (args.review or args.abandon))) else dict(os.environ)
        deployment = Deployment(root, args.artifact, choices, args.bootstrap, Tools(args.artifact, env))
        if args.plan:
            print('Operation ' + deployment.plan(imports(args.imports, args.bootstrap), args.approve_import, args.accept_cost) + '; run --review then --apply OPERATION_ID')
        elif args.review:
            print(json.dumps(deployment.review(), indent=2))
        elif args.apply:
            deployment.apply(args.apply, args.accept_cost)
            if args.state_role:
                deployment.attach_policy(args.apply, args.state_role, args.approve_policy_attachment)
            print('Deployment phase: ' + deployment.ledger['phase'])
        elif args.reconcile:
            deployment.reconcile(args.reconcile)
            print('Desired provider state reconciled; resume --apply with same operation ID')
        elif args.abandon:
            deployment.abandon(args.abandon)
            print('Operation archived; next --plan creates a new identity and refreshes authoritative state')
        elif args.ack_pages:
            deployment.ack_pages(args.ack_pages)
            print('Operator acknowledged Pages identity; resume --apply with same operation ID')
        else:
            if not interactive:
                print('Next: --plan, then --review, then --apply EXACT_OPERATION_ID. No implicit spending.')
                return 0
            guided_operation(deployment, imports(args.imports, args.bootstrap), args.approve_import, args.accept_cost)
        return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (InstallError, OSError, ValueError, KeyError, TypeError, EOFError, tarfile.TarError) as error:
        message = str(error) if isinstance(error, InstallError) else 'Local deployment input/state unavailable or corrupt; restore exact protected profile/state'
        print('ERROR: ' + message, file=sys.stderr)
        sys.exit(2)
