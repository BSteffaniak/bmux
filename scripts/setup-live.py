#!/usr/bin/env python3
"""Interactive, resumable local live setup. Never dispatches CI or adopts existing state."""
import argparse
import contextlib
import getpass
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import urllib.error
import urllib.request

HERE = Path(__file__).resolve().parent


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, HERE / filename)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


b = load('setup_bootstrap', 'bootstrap-live-secrets.py')
p = load('setup_state', 'provision-live-state.py')
g = load('setup_github', 'live-github-environments.py')
r = load('setup_recovery', 'live-state-recovery.py')
local = p.local
Error = b.BootstrapError


def confirm(message):
    if input(message + ' [y/N] ').strip().lower() not in ('y', 'yes'):
        raise Error('Stopped without further changes; rerun the same command to resume')


def secret(prompt):
    value = getpass.getpass(prompt + ': ')
    if not value or any(c.isspace() for c in value):
        raise Error('Empty/whitespace credential rejected')
    return value


class DiagnosticCloudflare(b.Cloudflare):
    """Setup-only transport: preserve bound bootstrap code while reporting safe request evidence."""
    def items(self, path):
        if not re.fullmatch(r'/accounts/[a-f0-9]{32}/pages/projects', path):
            yield from super().items(path)
            return
        # Match Wrangler's Pages-specific page size; generic bootstrap uses 50.
        # Do not rely on optional total_pages: a full page requires another read.
        for page in range(1, 101):
            data = self.request('GET', path + f'?page={page}&per_page=10')
            rows = data.get('result')
            if not isinstance(rows, list) or len(rows) > 10 or any(not isinstance(row, dict) for row in rows):
                raise Error('Invalid Pages project inventory; no adoption or secret writes')
            yield from rows
            if len(rows) < 10:
                return
        raise Error('Pages project inventory exceeds bound; no adoption or secret writes')

    def request(self, method, path, body=None):
        if method not in ('GET', 'POST') or not path.startswith('/') or path.startswith('//'):
            raise Error('Unsupported Cloudflare setup request')
        # Labels are constants, never raw paths, query values, request bodies or provider messages.
        endpoint = path.split('?', 1)[0]
        if re.fullmatch(r'/accounts/[a-f0-9]{32}/pages/projects', endpoint):
            stage = 'List Pages projects'
        elif endpoint == '/user/tokens/permission_groups':
            stage = 'List token permission groups'
        elif endpoint == '/user/tokens':
            stage = 'Create scoped token' if method == 'POST' else 'List scoped tokens'
        elif endpoint == '/zones':
            stage = 'Discover zone/account'
        else:
            stage = 'Cloudflare setup request'
        request = urllib.request.Request('https://api.cloudflare.com/client/v4' + path,
            data=None if body is None else json.dumps(body).encode(), method=method,
            headers={'Authorization': 'Bearer ' + self.token, 'Content-Type': 'application/json'})

        def codes(data):
            if not isinstance(data, dict) or not isinstance(data.get('errors'), list):
                return []
            return [v['code'] for v in data['errors'] if isinstance(v, dict) and type(v.get('code')) is int][:8]

        hint = ('; mutation may be ambiguous; do not retry token creation blindly'
                if method == 'POST' else '; read-only request failed; no automatic retry')
        try:
            with urllib.request.build_opener(urllib.request.ProxyHandler({}), b.NoRedirect).open(request, timeout=30) as response:
                status = response.status
                raw = response.read(1048577)
                if len(raw) > 1048576:
                    raise Error(stage + ': response exceeds bound' + hint)
                data = json.loads(raw)
                if status != 200 or not isinstance(data, dict) or data.get('success') is not True:
                    raise Error(stage + ': HTTP ' + str(status) + '; Cloudflare codes ' + json.dumps(codes(data)) + hint)
                return data
        except urllib.error.HTTPError as error:
            safe_codes = []
            try:
                raw = error.read(65537)
                if len(raw) <= 65536:
                    safe_codes = codes(json.loads(raw))
            except (OSError, ValueError, TypeError):
                pass
            finally:
                error.close()
            raise Error(stage + ': HTTP ' + str(error.code) + '; Cloudflare codes ' + json.dumps(safe_codes)
                        + hint + '; provider messages suppressed') from None
        except (urllib.error.URLError, OSError):
            raise Error(stage + ': transport failure' + hint + '; diagnostics suppressed') from None
        except (ValueError, TypeError, KeyError):
            raise Error(stage + ': invalid response' + hint + '; response suppressed') from None


class Setup:
    def __init__(self, args, run=b.command, environ=None):
        self.args, self.run = args, run
        self.env = dict(os.environ if environ is None else environ)
        self.env['GH_HOST'] = 'github.com'
        if run is b.command:
            self.run = self.command
        self.root = args.state_dir
        self.cloudflare = None
        self.credential_path = self.root / 'credentials.json'
        self.record_path = self.root / 'setup.json'
        self.binding = dict(repo=args.repo, reviewer=args.approval_reviewer, bucket=args.bucket,
                            publisher_key_file=str(args.publisher_key_file))

    def command(self, cmd, body=None):
        stage = 'GitHub setup'
        companion = len(cmd) > 1 and cmd[1] == str(HERE / 'provision-live-state.py')
        if companion:
            stage = 'R2 backend ' + next((flag[2:] for flag in ('--plan', '--review', '--apply', '--reconcile') if flag in cmd), 'operation')
        try:
            return b.command(cmd, body, environ=self.env)
        except b.BootstrapError:
            # Companion owns sanitized errors but arbitrary stderr may contain credentials.
            # Never forward it: inspect only authoritative public checkpoints for safe context.
            phase = 'unknown'
            if companion and (self.root / 'operation.json').is_file():
                value = local.private_read(self.root / 'operation.json').get('phase')
                if value in ('planned', 'applying', 'complete'):
                    phase = value
            raise Error(stage + ' failed; saved backend phase=' + phase
                        + '. Provider output suppressed. Preserve original state; if applying, verify creation audit before recovery.') from None

    def names(self):
        inventory = g.pages(self.run, f'repos/{self.args.repo}/environments', 'environments')
        if any(not isinstance(v, dict) or not isinstance(v.get('name'), str) for v in inventory):
            raise Error('Corrupt environment inventory')
        if any(v['name'].lower() == 'production' and v['name'] != 'Production' for v in inventory):
            raise Error('Environment capitalization conflicts with Production')
        if not any(v['name'] == 'Production' for v in inventory):
            return set()
        rows = json.loads(self.run(['gh', 'secret', 'list', '--repo', self.args.repo,
                                   '--env', 'Production', '--json', 'name']))
        if not isinstance(rows, list) or any(not isinstance(v, dict) or not isinstance(v.get('name'), str) for v in rows):
            raise Error('Corrupt secret inventory')
        return {v['name'] for v in rows}

    def retained(self, name, create):
        if self.credential_path.is_symlink():
            raise Error('Private credential store must not be a symlink')
        record = local.private_read(self.credential_path) if self.credential_path.exists() else dict(schema=1, binding=self.binding, values={})
        if (not isinstance(record, dict) or set(record) != {'schema', 'binding', 'values'}
                or record['schema'] != 1 or record['binding'] != self.binding or not isinstance(record['values'], dict)):
            raise Error('Private credential store belongs to another selection or is corrupt')
        values = record['values']
        if name not in values:
            if name == 'r2' and len({'R2_STATE_ACCESS_KEY_ID', 'R2_STATE_SECRET_ACCESS_KEY'} & self.names()) == 1:
                raise Error('Partial R2 secrets without retained original pair; explicit recovery required')
            if name == 'passphrase':
                values[name] = create()
                local.save(self.credential_path, record)
                return values[name]
            # A durable null means token creation may have succeeded without returning its secret.
            values[name] = None
            local.save(self.credential_path, record)
            value = create()
            values[name] = value
            local.save(self.credential_path, record)
        value = values[name]
        if name == 'r2':
            valid = (isinstance(value, dict) and set(value) == {'R2_STATE_ACCESS_KEY_ID', 'R2_STATE_SECRET_ACCESS_KEY'}
                     and re.fullmatch(r'[a-f0-9]{32}', value.get('R2_STATE_ACCESS_KEY_ID', ''))
                     and re.fullmatch(r'[a-f0-9]{64}', value.get('R2_STATE_SECRET_ACCESS_KEY', '')))
        else:
            valid = isinstance(value, str) and bool(value) and not any(c.isspace() for c in value)
        if not valid:
            raise Error('Credential creation ambiguous/corrupt; reconcile provider tokens before retry, never rotate automatically')
        return value

    def bootstrap_args(self):
        return argparse.Namespace(repo=self.args.repo, environment='Production', approval_reviewer=self.args.approval_reviewer,
            approve_environment_changes=False, publisher_key_file=self.args.publisher_key_file,
            bootstrap_state=True, state_dir=self.root, state_region='auto', state_bucket=self.args.bucket,
            state_account_id=None, state_mode='plan', state_operation=None, state_completion_file=None,
            accept_cost=False, approve_import=False, dry_run=False, input_env=False, only=None, rotate=False,
            zone='bmux.dev', hostname='bmux.dev', pages_project='bmux', _credential_store=self.retained)

    def choices(self, account):
        return p.choices(argparse.Namespace(repo=self.args.repo, bucket=self.args.bucket,
                                           region='auto', account_id=account), self.env)

    def invoke(self, args):
        # Only bootstrap's public operation reports are captured; never subprocess/provider diagnostics.
        with contextlib.redirect_stdout(io.StringIO()):
            b.provision(args, self.run, self.env, cloudflare=self.cloudflare)

    def execute(self):
        with local.lock(self.root / 'setup-lock'):
            return self.locked()

    def locked(self):
        if self.record_path.is_symlink():
            raise Error('Setup record must not be a symlink')
        record = local.private_read(self.record_path) if self.record_path.exists() else None
        if record is not None and (not isinstance(record, dict) or set(record) != {'schema', 'binding', 'phase', 'operation'}
                                   or record['schema'] != 1 or record['binding'] != self.binding
                                   or record['phase'] not in ('planned', 'applying', 'complete')
                                   or not re.fullmatch(r'[a-f0-9]{32}', record['operation'] or '')):
            raise Error('Setup record is corrupt or belongs to different choices; explicit reconciliation required')
        names = self.names()
        required = set(b.NORMAL_SECRET_NAMES + b.STATE_SECRET_NAMES)
        if 'LIVE_PUBLISHER_KEY_SHA256' in names:
            # Remote secret names cannot recover a lost publisher credential.
            b.read_private_key(self.args.publisher_key_file)
        if any(n in names or self.env.get(n) for n in ('LIVE_STATE_ROLE_ARN', 'LIVE_STATE_ACCESS_KEY_ID', 'LIVE_STATE_SECRET_ACCESS_KEY')):
            raise Error('Legacy backend detected; explicit migration required')
        if record and record['phase'] == 'complete' and required <= names:
            check = self.bootstrap_args()
            # Verify selected protections without requiring a Cloudflare provisioning token.
            manager = g.Environments(check, self.run)
            manager.execute(record['operation'], 'reconcile')
            self.ready()
            print('Setup verified; existing credentials unchanged. No deployment dispatched.')
            return
        if not self.env.get('CLOUDFLARE_PROVISION_TOKEN'):
            self.env['CLOUDFLARE_PROVISION_TOKEN'] = secret('Cloudflare provisioning token')
        args = self.bootstrap_args()
        cf = DiagnosticCloudflare(self.env)
        self.cloudflare = cf
        account, _ = b.discover(cf, args)
        args.state_account_id = account
        choices = self.choices(account)
        # Companion's original ledger is authoritative, even if setup was interrupted before saving its record.
        if (self.root / 'operation.json').is_symlink():
            raise Error('Backend operation must not be a symlink')
        with local.lock(self.root):
            provision = p.Provision(choices, self.root, cf)
            ledger = provision.ledger
        if ledger is None:
            if record is not None or set(b.STATE_SECRET_NAMES) & names or self.credential_path.exists():
                raise Error('Existing credentials/record without original operation; recover authority, never initialize replacement state')
            self.invoke(args)
            with local.lock(self.root):
                ledger = p.Provision(choices, self.root, cf).ledger
        if ledger['phase'] == 'applying':
            if not getattr(self.args, 'recover_creation_audit', None):
                raise Error('Backend apply ambiguous. Use --recover-creation-audit AUDIT_UUID --creation-time RFC3339 to verify and explicitly reconcile original creation')
            r.recover(p, choices, self.root, ledger['operation'], self.args.recover_creation_audit,
                      self.args.creation_time, r.Reader(self.env['CLOUDFLARE_PROVISION_TOKEN'], b.NoRedirect), confirm)
            with local.lock(self.root):
                obj = p.Provision(choices, self.root, None)
                obj.completion(ledger['operation'], self.root / 'ready.json')
                ledger = obj.ledger
        elif getattr(self.args, 'recover_creation_audit', None):
            r.recover(p, choices, self.root, ledger['operation'], self.args.recover_creation_audit,
                      self.args.creation_time, r.Reader(self.env['CLOUDFLARE_PROVISION_TOKEN'], b.NoRedirect), confirm)
            with local.lock(self.root):
                obj = p.Provision(choices, self.root, None)
                obj.completion(ledger['operation'], self.root / 'ready.json')
        operation = ledger['operation']
        if record and record['operation'] != operation:
            raise Error('Setup and backend operation identities differ')
        args.state_operation = operation
        env_path = self.root / 'github-environments' / (operation + '.json')
        if not env_path.exists():
            # Recover interruption between backend plan and its read-only GitHub plan save.
            with local.lock(self.root / 'github-environments'):
                g.Environments(args, self.run).plan(operation)
        record = dict(schema=1, binding=self.binding, phase='planned' if record is None else record['phase'], operation=operation)
        local.save(self.record_path, record)
        args.state_mode = 'review'
        self.invoke(args)
        github_plan = local.private_read(env_path)
        print(json.dumps(dict(repository=self.args.repo, reviewer=self.args.approval_reviewer, backend=choices,
                              bucket_changes=ledger['resources'], environments_before=github_plan['before'],
                              environments_desired=github_plan['desired'],
                              secrets_to_install=sorted(required - names)), indent=2))
        confirm('Apply/resume this exact setup, including displayed environment changes and possible Cloudflare costs?')
        if 'TOFU_STATE_ENCRYPTION_PASSPHRASE' not in names:
            def passphrase():
                value = secret('Original/high-entropy state encryption passphrase from your password manager')
                if len(value) < 32:
                    raise Error('Passphrase must be at least 32 characters')
                if secret('Confirm state encryption passphrase') != value:
                    raise Error('Passphrases differ')
                return value
            value = self.retained('passphrase', passphrase)
            confirm('Have you backed up this exact passphrase independently? Losing it loses state access')
            self.env['TOFU_STATE_ENCRYPTION_PASSPHRASE'] = value
        # A supplied passphrase is never inferred from remote secret names or regenerated.
        args.state_mode = 'apply'
        args.accept_cost = True
        args.approve_environment_changes = True
        if ledger['expected'] is None:
            args.state_completion_file = self.root / 'ready.json'
        record['phase'] = 'applying'
        local.save(self.record_path, record)
        self.invoke(args)
        if not required <= self.names():
            raise Error('Secret installation incomplete; rerun same command to resume retained credentials')
        with local.lock(self.root / 'github-environments'):
            g.Environments(args, self.run).execute(operation, 'reconcile')
        self.ready()
        record['phase'] = 'complete'
        local.save(self.record_path, record)
        print('Setup complete. No deployment dispatched. Rerun this command to verify/resume.')

    def ready(self):
        receipt = self.root / 'ready.json'
        inputs = dict(reviewed_imports='{}', approve_import=False)
        if not receipt.exists():
            raise Error('Original new-backend completion receipt missing; recover it before CI')
        value = local.private_read(receipt)
        if not isinstance(value, dict) or value.get('kind') != 'bmux-live-r2-bootstrap-completion':
            raise Error('Corrupt completion receipt')
        choices_args = self.bootstrap_args()
        choices_args.state_account_id = value['backend']['account_id']
        with local.lock(self.root):
            original = p.Provision(self.choices(choices_args.state_account_id), self.root, None)
            original.completion(value['operation'], receipt)
        inputs.update(bootstrap_completion=receipt.read_text(), bootstrap_completion_sha256=local.digest(receipt))
        path = self.root / 'configure-inputs.json'
        if not path.exists() or local.private_read(path) != inputs:
            local.save(path, inputs)
        print('First configure inputs saved privately at ' + str(path))
        print('When ready for FIRST configure (do not replay after state exists):')
        print('gh workflow run configure-production-infra.yml --repo ' + shlex.quote(self.args.repo)
              + ' --ref master --json < ' + shlex.quote(str(path)))
        print('After first configure succeeds, subsequent runs must omit bootstrap inputs.')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', default='BSteffaniak/bmux')
    parser.add_argument('--approval-reviewer', default='BSteffaniak')
    parser.add_argument('--bucket', default='bmux-opentofu-state')
    parser.add_argument('--state-dir', type=Path, default=Path.home() / '.local/state/bmux/live-bootstrap')
    parser.add_argument('--publisher-key-file', type=Path, default=Path.home() / '.config/bmux/live/publisher-key')
    parser.add_argument('--recover-creation-audit', help='Explicit recovery: original successful Cloudflare audit UUID')
    parser.add_argument('--creation-time', help='Exact original audit event RFC3339 timestamp')
    args = parser.parse_args()
    if bool(args.recover_creation_audit) != bool(args.creation_time):
        parser.error('Recovery requires both --recover-creation-audit and --creation-time')
    if (not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', args.repo)
            or not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9-]{0,38}', args.approval_reviewer)
            or not re.fullmatch(r'[a-z0-9][a-z0-9-]{1,61}[a-z0-9]', args.bucket)
            or not args.state_dir.is_absolute() or not args.publisher_key_file.is_absolute()):
        parser.error('Invalid repository/reviewer/bucket or nonabsolute private paths')
    if not sys.stdin.isatty():
        parser.error('Interactive terminal required; use advanced bootstrap for automation')
    try:
        os.umask(0o077)
        if subprocess.run(['gh', 'auth', 'status', '--hostname', 'github.com'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False).returncode:
            subprocess.run(['gh', 'auth', 'login', '--hostname', 'github.com', '--web'], check=True)
        local.private_dir(args.state_dir)
        local.private_dir(args.publisher_key_file.parent)
        Setup(args).execute()
        return 0
    except (Error, local.InstallError, g.Error, r.RecoveryError) as error:
        print('ERROR: ' + str(error), file=sys.stderr)
    except (OSError, ValueError, TypeError, KeyError, AttributeError, subprocess.SubprocessError):
        print('ERROR: Setup failed; diagnostics suppressed. Retain private state and reconcile before retry.', file=sys.stderr)
    except (EOFError, KeyboardInterrupt):
        print('Setup interrupted; rerun same command. Ambiguous mutations require reconciliation.', file=sys.stderr)
    return 1


if __name__ == '__main__':
    sys.exit(main())
