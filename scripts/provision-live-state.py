#!/usr/bin/env python3
"""Explicit production S3/GitHub OIDC provisioning companion; default is provider-free."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import uuid
import selectors
import signal
import time

# Reuse the existing private ledger/locking and secret-safe process primitives.
_spec = importlib.util.spec_from_file_location('live_installer', Path(__file__).with_name('live-install.py'))
local = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(local)
Error = local.InstallError
MODULE = Path(__file__).resolve().parent.parent / 'infra/live-terraform/bootstrap'


def choices(args):
    bucket = args.bucket or os.environ.get('LIVE_STATE_BUCKET')
    if args.bucket and os.environ.get('LIVE_STATE_BUCKET') and args.bucket != os.environ['LIVE_STATE_BUCKET']:
        raise Error('Configured LIVE_STATE_BUCKET differs; refusing duplicate backend')
    region = args.region or os.environ.get('LIVE_STATE_REGION')
    if args.region and os.environ.get('LIVE_STATE_REGION') and args.region != os.environ['LIVE_STATE_REGION']:
        raise Error('Configured backend region differs')
    value = dict(repo=args.repo, bucket=bucket, region=region, role=args.role, account_id=args.account_id)
    patterns = {'repo': r'[A-Za-z0-9_.-]{1,100}/[A-Za-z0-9_.-]{1,100}',
                'bucket': r'[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]', 'region': r'[a-z]{2}-[a-z]+-\d',
                'role': r'[A-Za-z0-9+=,.@_-]{1,64}', 'account_id': r'\d{12}'}
    for key, pattern in patterns.items():
        if not isinstance(value[key], str) or not re.fullmatch(pattern, value[key]):
            raise Error('Provide valid explicit ' + key)
    if '..' in value['bucket'] or re.fullmatch(r'\d+\.\d+\.\d+\.\d+', value['bucket']):
        raise Error('Invalid S3 bucket identity')
    return value


def public(c, operation=None, phase='dryrun', summary=None):
    return dict(schema=1, operation=operation, phase=phase,
                backend=dict(bucket=c['bucket'], region=c['region'], key='bmux/live/production/terraform.tfstate',
                             encrypt=True, use_lockfile=True),
                role_arn=f'arn:aws:iam::{c["account_id"]}:role/{c["role"]}',
                github_oidc=dict(include_claim_keys=['repo', 'context', 'ref'],
                                 subject=f'repo:{c["repo"]}:environment:Production:ref:refs/heads/master'),
                resources=summary or [])


class Tools:
    def __init__(self, region):
        env = dict(os.environ)
        for key in list(env):
            if key.startswith(('TF_', 'TOFU_')) or key in ('AWS_ENDPOINT_URL', 'AWS_ENDPOINT_URL_S3'):
                del env[key]
        env.update(TF_IN_AUTOMATION='1', CHECKPOINT_DISABLE='1', AWS_REGION=region,
                   AWS_DEFAULT_REGION=region, AWS_PAGER='', AWS_CLI_AUTO_PROMPT='off')
        self.deadline = time.monotonic() + 900
        self.runner = local.Tools(Path('/'), env)
        self.aws = shutil.which('aws')
        self.tofu = shutil.which('tofu')

    def run(self, command, cwd=None, codes=(0,), timeout=900):
        deadline = min(self.deadline, time.monotonic() + timeout)
        if deadline <= time.monotonic():
            raise Error('Aggregate provisioning deadline exhausted; reconcile interrupted mutations before retry')
        buffers = {1: bytearray(), 2: bytearray()}
        process = None
        try:
            process = subprocess.Popen(command, cwd=cwd, env=self.runner.env,
                                       stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                       stderr=subprocess.PIPE, start_new_session=True)
            with selectors.DefaultSelector() as selector:
                selector.register(process.stdout, selectors.EVENT_READ, 1)
                selector.register(process.stderr, selectors.EVENT_READ, 2)
                total = 0
                while selector.get_map():
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise Error('Provider operation timed out; interrupted mutation requires reconciliation')
                    for key, _ in selector.select(min(remaining, 1)):
                        chunk = os.read(key.fileobj.fileno(), 65536)
                        if not chunk:
                            selector.unregister(key.fileobj)
                            continue
                        total += len(chunk)
                        if total > 32 * 1024 * 1024:
                            raise Error('Provider output exceeds bounded retention; operation outcome may require reconciliation')
                        buffers[key.data].extend(chunk)
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise Error('Provider deadline expired; reconcile interrupted operation')
            code = process.wait(timeout=remaining)
            if code not in codes:
                raise Error('Provider operation failed; diagnostics suppressed; verify authentication and locked state')
            return subprocess.CompletedProcess(command, code, bytes(buffers[1]), bytes(buffers[2]))
        except (OSError, subprocess.TimeoutExpired):
            raise Error('Provider executable unavailable/timed out; diagnostics suppressed') from None
        finally:
            if process is not None:
                if process.poll() is None:
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    process.wait()
                if process.stdout:
                    process.stdout.close()
                if process.stderr:
                    process.stderr.close()

    def check(self, account):
        if not self.aws or not self.tofu:
            raise Error('Install AWS CLI and pinned OpenTofu 1.10.9')
        version = json.loads(self.run([self.tofu, 'version', '-json']).stdout)
        if version.get('terraform_version') != '1.10.9':
            raise Error('Use pinned OpenTofu 1.10.9 without changing provider locks')
        identity = self.aws_json('sts', 'get-caller-identity')
        if identity.get('Account') != account or not isinstance(identity.get('Arn'), str):
            raise Error('AWS caller account differs from expected bootstrap account')
        if not isinstance(identity['Arn'], str) or not re.fullmatch(r'arn:aws:(?:iam|sts)::' + account + r':(?:user|assumed-role)/[A-Za-z0-9+=,.@_/-]{1,512}', identity['Arn']):
            raise Error('Unsupported AWS caller identity; use scoped bootstrap role or AWS SSO')
        if identity['Arn'].endswith(':root'):
            raise Error('AWS root credentials are forbidden; use scoped bootstrap role or AWS SSO')

    def aws_json(self, *args):
        value = json.loads(self.run([self.aws, *args, '--no-paginate', '--output', 'json']).stdout)
        if not isinstance(value, dict) or value.get('IsTruncated') or any(value.get(key) for key in ('NextToken', 'NextContinuationToken', 'Marker', 'ContinuationToken')):
            raise Error('Unsupported/truncated AWS inventory; narrow account resources rather than guessing absence')
        return value

    def tofu_run(self, workspace, *args, codes=(0,)):
        return self.run([self.tofu, *args], cwd=workspace, codes=codes)


def discovery(tools, c):
    """Only inventory calls; absent never inferred from a permission error."""
    account = c['account_id']
    provider = ''
    providers = tools.aws_json('iam', 'list-open-id-connect-providers').get('OpenIDConnectProviderList')
    if not isinstance(providers, list):
        raise Error('Corrupt OIDC inventory')
    expected = f'arn:aws:iam::{account}:oidc-provider/token.actions.githubusercontent.com'
    if any(item.get('Arn') == expected for item in providers):
        details = tools.aws_json('iam', 'get-open-id-connect-provider', '--open-id-connect-provider-arn', expected)
        if details.get('Url') not in ('token.actions.githubusercontent.com', 'https://token.actions.githubusercontent.com') or 'sts.amazonaws.com' not in details.get('ClientIDList', []):
            raise Error('Existing GitHub OIDC provider incompatible; reconcile shared provider explicitly')
        provider = expected
    imports = {}
    buckets = tools.aws_json('s3api', 'list-buckets').get('Buckets')
    if not isinstance(buckets, list):
        raise Error('Corrupt bucket inventory')
    if any(item.get('Name') == c['bucket'] for item in buckets):
        location = tools.aws_json('s3api', 'get-bucket-location', '--bucket', c['bucket'])
        if (location.get('LocationConstraint') or 'us-east-1') != c['region']:
            raise Error('Existing state bucket region differs')
        for resource in ('aws_s3_bucket.state', 'aws_s3_bucket_versioning.state',
                         'aws_s3_bucket_server_side_encryption_configuration.state'):
            imports[resource + '["production"]'] = c['bucket']
        public_block = tools.run([tools.aws, 's3api', 'get-public-access-block', '--bucket', c['bucket'], '--output', 'json'], codes=(0, 254))
        if public_block.returncode == 0:
            if not isinstance(json.loads(public_block.stdout).get('PublicAccessBlockConfiguration'), dict):
                raise Error('Corrupt public access block inventory')
            imports['aws_s3_bucket_public_access_block.state["production"]'] = c['bucket']
        elif b'(NoSuchPublicAccessBlockConfiguration)' not in public_block.stderr:
            raise Error('Public access block unavailable; permission failure is not absence')
        # Missing configuration gets created; unauthorized/unknown response is never absence.
        result = tools.run([tools.aws, 's3api', 'get-bucket-policy', '--bucket', c['bucket'], '--output', 'json'], codes=(0, 254))
        if result.returncode == 0:
            policy = json.loads(json.loads(result.stdout)['Policy'])
            if not isinstance(policy.get('Statement'), list):
                raise Error('Corrupt existing bucket policy')
            # Avoid overwriting an unrelated existing policy with only a TLS statement.
            statements = policy['Statement']
            safe = len(statements) == 1 and statements[0].get('Effect') == 'Deny' and statements[0].get('Principal') == '*' and statements[0].get('Action') == 's3:*' and statements[0].get('Condition') == {'Bool': {'aws:SecureTransport': 'false'}}
            if not safe:
                raise Error('Existing bucket has additional/different policy; do not overwrite it automatically')
            imports['aws_s3_bucket_policy.tls["production"]'] = c['bucket']
        elif b'(NoSuchBucketPolicy)' not in result.stderr:
            raise Error('Existing bucket policy unavailable; permission failure is not absence')
    roles = tools.aws_json('iam', 'list-roles').get('Roles')
    if not isinstance(roles, list):
        raise Error('Corrupt role inventory')
    existing = [r for r in roles if r.get('RoleName') == c['role']]
    if len(existing) > 1:
        raise Error('Ambiguous role inventory')
    if existing:
        if existing[0].get('Arn') != public(c)['role_arn']:
            raise Error('Existing role ARN/path differs; choose exact supported role')
        imports['aws_iam_role.github_production[0]'] = c['role']
        attachments = tools.aws_json('iam', 'list-attached-role-policies', '--role-name', c['role']).get('AttachedPolicies')
        if not isinstance(attachments, list):
            raise Error('Corrupt existing role policy inventory')
        target = f'arn:aws:iam::{account}:policy/bmux-live-state-production'
        if any(p.get('PolicyArn') != target for p in attachments):
            raise Error('Existing role has unrelated permissions; refuse broad production role')
        inline = tools.aws_json('iam', 'list-role-policies', '--role-name', c['role']).get('PolicyNames')
        if inline != []:
            raise Error('Existing role has inline/unknown policies; reconcile explicitly')
        if attachments:
            imports['aws_iam_role_policy_attachment.github_production[0]'] = c['role'] + '/' + target
    policies = tools.aws_json('iam', 'list-policies', '--scope', 'Local').get('Policies')
    if not isinstance(policies, list):
        raise Error('Corrupt policy inventory')
    matches = [p for p in policies if p.get('PolicyName') == 'bmux-live-state-production']
    if len(matches) > 1:
        raise Error('Ambiguous state policy inventory')
    if matches:
        expected_policy = f'arn:aws:iam::{account}:policy/bmux-live-state-production'
        if matches[0].get('Arn') != expected_policy:
            raise Error('Existing state policy path/account differs')
        # IAM policy may be shared with other consumers; do not rewrite their permissions.
        entities = tools.aws_json('iam', 'list-entities-for-policy', '--policy-arn', expected_policy)
        if entities.get('PolicyUsers') != [] or entities.get('PolicyGroups') != [] or not isinstance(entities.get('PolicyRoles'), list) or any(role.get('RoleName') != c['role'] for role in entities['PolicyRoles']):
            raise Error('Existing state policy is shared or inventory unknown; reconcile without changing other consumers')
        imports['aws_iam_policy.state["production"]'] = expected_policy
    return provider, imports


class Provision:
    def __init__(self, c, root, tools):
        self.c, self.root, self.tools = c, root, tools
        self.workspace = root / 'workspace'
        self.path = root / 'operation.json'
        self.module_id = local.identity({name: local.digest(MODULE / name) for name in ('main.tf', '.terraform.lock.hcl')})
        self.binding = local.identity(dict(choices=c, module=self.module_id))
        self.ledger = local.private_read(self.path) if self.path.exists() else None
        if self.ledger is not None:
            allowed = {'schema', 'operation', 'binding', 'phase', 'provider_arn', 'imports', 'plan_sha256', 'resources'}
            if not isinstance(self.ledger, dict) or set(self.ledger) != allowed or self.ledger['schema'] != 1 or self.ledger['binding'] != self.binding or not re.fullmatch(r'[a-f0-9]{32}', self.ledger['operation']) or self.ledger['phase'] not in ('planning', 'planned', 'applying', 'complete'):
                raise Error('Corrupt/incompatible provisioning ledger or changed authoritative choices/module')
            if self.ledger['phase'] != 'planning' and not re.fullmatch(r'[a-f0-9]{64}', self.ledger['plan_sha256']):
                raise Error('Corrupt provisioning plan identity')
            validate_imports(self.ledger['imports'], c)
            expected_provider = f'arn:aws:iam::{c["account_id"]}:oidc-provider/token.actions.githubusercontent.com'
            if self.ledger['provider_arn'] not in ('', expected_provider):
                raise Error('Corrupt OIDC provider identity')
            if not isinstance(self.ledger['resources'], list):
                raise Error('Corrupt resource review inventory')
            for item in self.ledger['resources']:
                if not isinstance(item, dict) or set(item) != {'address', 'actions', 'import'} or item['address'] not in set(import_addresses()) | {'aws_iam_openid_connect_provider.github[0]'} or not isinstance(item['import'], bool):
                    raise Error('Corrupt secret-free resource review')
                local.review_summary({'resource_changes': [{'address': item['address'], 'change': {'actions': item['actions']}}]})

    def phase(self, phase, **values):
        self.ledger.update(phase=phase, **values)
        local.save(self.path, self.ledger)

    def init(self):
        local.private_dir(self.workspace)
        for name in ('main.tf', '.terraform.lock.hcl'):
            source = MODULE / name
            destination = self.workspace / name
            if source.is_symlink() or destination.is_symlink():
                raise Error('Bootstrap module/cache symlink rejected')
            shutil.copyfile(source, destination)
            os.chmod(destination, 0o600)
        for name in ('choices.auto.tfvars.json', 'approved-imports.tf.json', 'reviewed.plan', 'terraform.tfstate', 'terraform.tfstate.backup', '.terraform', '.terraform.tfstate.lock.info'):
            if (self.workspace / name).is_symlink():
                raise Error('Bootstrap workspace state/plan symlink rejected')
        c = self.c
        local.save(self.workspace / 'choices.auto.tfvars.json', dict(region=c['region'], bucket_prefix='bmux-live',
                   environments=['production'], existing_buckets={'production': c['bucket']},
                   github_repository=c['repo'], github_role_name=c['role'], github_oidc_provider_arn=self.ledger['provider_arn']))
        local.save(self.workspace / 'approved-imports.tf.json', {'import': [
            {'to': '${' + address + '}', 'id': provider_id} for address, provider_id in self.ledger['imports'].items()]})
        self.tools.tofu_run(self.workspace, 'init', '-input=false', '-lockfile=readonly')
        for name in ('main.tf', '.terraform.lock.hcl'):
            if local.digest(self.workspace / name) != local.digest(MODULE / name):
                raise Error('Bootstrap module/lock changed during initialization')

    def plan(self, approve_import):
        if self.ledger and self.ledger['phase'] in ('applying', 'planned'):
            raise Error('Existing plan/mutation must be applied/reconciled; refusing replacement')
        self.tools.check(self.c['account_id'])
        provider, existing = discovery(self.tools, self.c)
        if self.ledger and self.ledger['phase'] == 'complete':
            return self.result()
        self.ledger = dict(schema=1, operation=uuid.uuid4().hex, binding=self.binding, phase='planning',
                           provider_arn=provider, imports={}, plan_sha256='', resources=[])
        self.phase('planning')
        self.init()
        state = self.tools.tofu_run(self.workspace, 'state', 'list').stdout.decode().splitlines()
        pending = {k: v for k, v in existing.items() if k not in state}
        if pending and not approve_import:
            raise Error('Existing resources require --approve-import: ' + json.dumps(pending, sort_keys=True))
        validate_imports(pending, self.c)
        self.phase('planning', imports=pending)
        self.init()
        path = self.workspace / 'reviewed.plan'
        self.tools.tofu_run(self.workspace, 'plan', '-input=false', '-lock-timeout=5m', '-out=' + str(path))
        os.chmod(path, 0o600)
        local.sync_file(path)
        plan = json.loads(self.tools.tofu_run(self.workspace, 'show', '-json', str(path)).stdout)
        resources = local.review_summary(plan)
        allowed = set(import_addresses()) | {'aws_iam_openid_connect_provider.github[0]'}
        if any(item['address'] not in allowed for item in resources):
            raise Error('Unexpected resource in production bootstrap plan')
        self.phase('planned', plan_sha256=local.digest(path), resources=resources)
        return self.result()

    def exact(self, operation):
        if not self.ledger or self.ledger['operation'] != operation:
            raise Error('Explicit approval must name exact saved operation ID')
        current_module = local.identity({name: local.digest(MODULE / name) for name in ('main.tf', '.terraform.lock.hcl')})
        if current_module != self.module_id:
            raise Error('Authoritative module changed during operation')

    def result(self):
        return public(self.c, self.ledger['operation'] if self.ledger else None,
                      self.ledger['phase'] if self.ledger else 'dryrun', self.ledger['resources'] if self.ledger else [])

    def review(self):
        if not self.ledger or self.ledger['phase'] not in ('planned', 'complete'):
            raise Error('No completed reviewable plan; interrupted mutations require reconciliation')
        if self.ledger['phase'] == 'planned' and local.digest(self.workspace / 'reviewed.plan') != self.ledger['plan_sha256']:
            raise Error('Saved reviewed plan content changed')
        return self.result()

    def apply(self, operation, cost):
        self.exact(operation)
        if self.ledger['phase'] == 'complete':
            return self.result()
        if not cost:
            raise Error('Explicit --accept-cost required for billable AWS provisioning')
        if self.ledger['phase'] != 'planned':
            raise Error('Prior mutation outcome ambiguous; --reconcile exact operation, never blind reapply')
        self.review()
        self.tools.check(self.c['account_id'])
        self.init()
        self.phase('applying')
        self.tools.tofu_run(self.workspace, 'apply', '-input=false', '-lock-timeout=5m', str(self.workspace / 'reviewed.plan'))
        self.phase('complete')
        return self.result()

    def reconcile(self, operation):
        self.exact(operation)
        if self.ledger['phase'] != 'applying':
            raise Error('Only interrupted apply needs reconciliation')
        self.tools.check(self.c['account_id'])
        self.init()
        result = self.tools.tofu_run(self.workspace, 'plan', '-input=false', '-lock-timeout=5m', '-detailed-exitcode', codes=(0, 2))
        if result.returncode != 0:
            raise Error('Partial provider apply differs; retain state and reconcile explicit resource ownership before any fresh operation')
        self.phase('complete')
        return self.result()


def import_addresses():
    return ['aws_s3_bucket.state["production"]', 'aws_s3_bucket_versioning.state["production"]',
            'aws_s3_bucket_server_side_encryption_configuration.state["production"]',
            'aws_s3_bucket_public_access_block.state["production"]', 'aws_s3_bucket_policy.tls["production"]',
            'aws_iam_policy.state["production"]', 'aws_iam_role.github_production[0]',
            'aws_iam_role_policy_attachment.github_production[0]']


def validate_imports(values, c):
    if not isinstance(values, dict):
        raise Error('Corrupt import inventory')
    policy = f'arn:aws:iam::{c["account_id"]}:policy/bmux-live-state-production'
    for address, value in values.items():
        if address not in import_addresses():
            raise Error('Unexpected import resource')
        expected = c['bucket'] if address.startswith('aws_s3_') else (policy if address.startswith('aws_iam_policy.') else
                   c['role'] + '/' + policy if address.startswith('aws_iam_role_policy_attachment.') else c['role'])
        if value != expected:
            raise Error('Import identity differs from exact configured backend/role/account')


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', required=True)
    parser.add_argument('--region')
    parser.add_argument('--bucket')
    parser.add_argument('--role', default='bmux-live-state-production')
    parser.add_argument('--account-id', required=True)
    parser.add_argument('--state-dir', type=Path, default=Path.home() / '.local/state/bmux/live-state-provision')
    parser.add_argument('--approve-import', action='store_true')
    parser.add_argument('--accept-cost', action='store_true')
    group = parser.add_mutually_exclusive_group()
    group.add_argument('--plan', action='store_true')
    group.add_argument('--review', action='store_true')
    group.add_argument('--apply', metavar='OPERATION_ID')
    group.add_argument('--reconcile', metavar='OPERATION_ID')
    args = parser.parse_args(argv)
    c = choices(args)
    if not args.state_dir.is_absolute():
        raise Error('State directory must be absolute')
    if not any((args.plan, args.review, args.apply, args.reconcile)):
        print(json.dumps(public(c), sort_keys=True))
        return 0
    os.umask(0o077)
    with local.lock(args.state_dir):
        provision = Provision(c, args.state_dir, Tools(c['region']))
        if args.plan:
            result = provision.plan(args.approve_import)
        elif args.review:
            result = provision.review()
        elif args.apply:
            result = provision.apply(args.apply, args.accept_cost)
        else:
            result = provision.reconcile(args.reconcile)
        print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (Error, OSError, ValueError, TypeError, KeyError) as error:
        print('ERROR: ' + (str(error) if isinstance(error, Error) else 'Invalid/corrupt local or provider state; output suppressed'), file=sys.stderr)
        sys.exit(2)
