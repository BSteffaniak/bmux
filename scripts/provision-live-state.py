#!/usr/bin/env python3
"""Explicit R2 bucket bootstrap. Default is provider-free; credentials never enter the ledger."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import sys
import stat
import uuid


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


local = load('live_local', 'live-install.py')
bootstrap = load('live_bootstrap', 'bootstrap-live-secrets.py')
Error = local.InstallError
def implementation_identity():
    values = {}
    for path in (Path(__file__).absolute(), Path(__file__).absolute().with_name('bootstrap-live-secrets.py')):
        info = path.lstat()
        if not stat.S_ISREG(info.st_mode) or info.st_mode & 0o022:
            raise Error('Unsafe provisioning implementation asset')
        values[path.name] = local.digest(path)
    return local.identity(values)


def choices(args, environ=None):
    env = os.environ if environ is None else environ
    if any(env.get(k) for k in ('LIVE_STATE_ROLE_ARN', 'LIVE_STATE_ACCESS_KEY_ID', 'LIVE_STATE_SECRET_ACCESS_KEY')):
        raise Error('Legacy AWS backend configured; manual state migration required before R2 bootstrap')
    bucket = args.bucket or env.get('LIVE_STATE_BUCKET') or 'bmux-opentofu-state'
    account = args.account_id or env.get('CLOUDFLARE_ACCOUNT_ID')
    region = args.region or env.get('LIVE_STATE_REGION') or 'auto'
    for flag, name, value in ((args.bucket, 'LIVE_STATE_BUCKET', bucket),
                              (args.account_id, 'CLOUDFLARE_ACCOUNT_ID', account),
                              (args.region, 'LIVE_STATE_REGION', region)):
        if flag and env.get(name) and env[name] != value:
            raise Error('Configured backend identity differs; refusing competing backend')
    if not re.fullmatch(r'[a-z0-9][a-z0-9-]{1,61}[a-z0-9]', bucket) or region != 'auto':
        raise Error('R2 requires a valid bucket and region auto; legacy AWS configuration requires manual migration')
    if not isinstance(account, str) or not re.fullmatch(r'[a-f0-9]{32}', account):
        raise Error('Provide exact Cloudflare account identity')
    if not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', args.repo):
        raise Error('Provide OWNER/REPO')
    return dict(repo=args.repo, bucket=bucket, region=region, account_id=account)


def public(c, operation=None, phase='dryrun', resources=None):
    return dict(schema=2, operation=operation, phase=phase,
                backend=dict(bucket=c['bucket'], region='auto', key='bmux/live/production/terraform.tfstate',
                             endpoint=f'https://{c["account_id"]}.r2.cloudflarestorage.com',
                             use_lockfile=True, encrypt=True), resources=resources or [])


def snapshot(cf, c):
    # Listing avoids conflating 404, authorization errors, or malformed API replies.
    rows = []
    cursor = None
    for _ in range(100):
        path = f'/accounts/{c["account_id"]}/r2/buckets?per_page=1000'
        if cursor:
            path += '&cursor=' + bootstrap.urllib.parse.quote(cursor, safe='')
        data = cf.request('GET', path)
        result = data.get('result')
        if not isinstance(result, dict) or not isinstance(result.get('buckets'), list):
            raise Error('Unknown/corrupt R2 bucket inventory')
        rows.extend(result['buckets'])
        info = data.get('result_info', {})
        if not isinstance(info, dict):
            raise Error('Unknown R2 pagination metadata')
        result_cursor = result.get('cursor')
        info_cursor = info.get('cursor')
        if result_cursor and info_cursor and result_cursor != info_cursor:
            raise Error('Conflicting R2 pagination cursors')
        next_cursor = info_cursor or result_cursor
        if not next_cursor:
            break
        if not isinstance(next_cursor, str) or next_cursor == cursor or len(next_cursor) > 4096:
            raise Error('Invalid R2 inventory cursor')
        cursor = next_cursor
    else:
        raise Error('R2 bucket inventory exceeds bound')
    matches = [r for r in rows if isinstance(r, dict) and r.get('name') == c['bucket']]
    if len(matches) > 1 or any(not isinstance(r, dict) or not isinstance(r.get('name'), str) for r in rows):
        raise Error('Unknown/corrupt R2 bucket inventory')
    if not matches:
        return None
    row = matches[0]
    if not isinstance(row.get('creation_date'), str) or row.get('storage_class') != 'Standard':
        raise Error('Unsupported R2 bucket metadata; inspect ownership before reuse')
    return dict(name=row['name'], creation_date=row['creation_date'], storage_class=row['storage_class'])


class Provision:
    def __init__(self, c, root, cf):
        self.c, self.root, self.cf = c, root, cf
        self.path = root / 'operation.json'
        self.binding = local.identity(dict(choices=c, module=implementation_identity()))
        self.ledger = local.private_read(self.path) if self.path.exists() else None
        if self.ledger is not None:
            x = self.ledger
            if (not isinstance(x, dict) or set(x) != {'schema', 'operation', 'binding', 'phase', 'expected', 'observed', 'resources'}
                    or x['schema'] != 2 or x['binding'] != self.binding
                    or not isinstance(x['operation'], str) or not re.fullmatch(r'[a-f0-9]{32}', x['operation'])
                    or x['phase'] not in ('planned', 'applying', 'complete')):
                raise Error('Legacy/corrupt provisioning ledger or changed authority; manual migration required')
            expected = x['expected']
            if expected is not None and (not isinstance(expected, dict) or set(expected) != {'name', 'creation_date', 'storage_class'}
                    or expected['name'] != c['bucket'] or not isinstance(expected['creation_date'], str)
                    or expected['storage_class'] != 'Standard'):
                raise Error('Corrupt expected bucket identity')
            observed = x['observed']
            if observed is not None and (not isinstance(observed, dict) or set(observed) != {'name', 'creation_date', 'storage_class'} or observed['name'] != c['bucket'] or not isinstance(observed['creation_date'], str) or observed['storage_class'] != 'Standard'):
                raise Error('Corrupt observed bucket identity')
            if x['phase'] == 'complete' and observed is None:
                raise Error('Completed operation has no observed bucket identity')
            if x['resources'] != self.resources(expected):
                raise Error('Corrupt resource review')

    @staticmethod
    def resources(expected):
        return [{'address': 'cloudflare_r2_bucket.state', 'actions': ['no-op' if expected else 'create'], 'import': expected is not None}]

    def result(self):
        return public(self.c, self.ledger['operation'], self.ledger['phase'], self.ledger['resources'])

    def exact(self, operation):
        if not self.ledger or operation != self.ledger['operation'] or self.binding != local.identity(dict(choices=self.c, module=implementation_identity())):
            raise Error('Approval must name exact saved operation and unchanged module/configuration')

    def save(self, phase):
        self.ledger['phase'] = phase
        local.save(self.path, self.ledger)

    def plan(self, approve_import=False):
        if self.ledger:
            raise Error('Existing operation is authoritative; review/reconcile it, never replace')
        expected = snapshot(self.cf, self.c)
        if expected and not approve_import:
            raise Error('Existing bucket requires explicit --approve-import ownership acceptance')
        self.ledger = dict(schema=2, operation=uuid.uuid4().hex, binding=self.binding, phase='planned',
                           expected=expected, observed=None, resources=self.resources(expected))
        self.save('planned')
        return self.result()

    def apply(self, operation, accept_cost):
        self.exact(operation)
        if not accept_cost or self.ledger['phase'] != 'planned':
            raise Error('Apply requires planned operation and explicit --accept-cost; interrupted apply requires reconcile')
        if snapshot(self.cf, self.c) != self.ledger['expected']:
            raise Error('Bucket identity changed since review; refusing mutation')
        self.save('applying')  # Durable before any provider mutation.
        if self.ledger['expected'] is None:
            data = self.cf.request('POST', f'/accounts/{self.c["account_id"]}/r2/buckets',
                                   dict(name=self.c['bucket'], storageClass='Standard'))
            row = data.get('result')
            if not isinstance(row, dict) or row.get('name') != self.c['bucket']:
                raise Error('Unexpected creation response; retain operation and inspect provider')
            expected = snapshot(self.cf, self.c)
            if expected is None or expected.get('creation_date') != row.get('creation_date'):
                raise Error('Created bucket identity unconfirmed; manual reconciliation required')
            # Preserve reviewed resources; creation identity is not a new plan.
            self.ledger['observed'] = expected
        else:
            self.ledger['observed'] = self.ledger['expected']
        self.save('complete')
        return self.result()

    def completion(self, operation, path):
        self.exact(operation)
        if self.ledger['phase'] != 'complete' or self.ledger['expected'] is not None:
            raise Error('Completion admission requires exact confirmed new-bucket operation, never adopted bucket')
        if not path.is_absolute() or path.exists() or path.is_symlink():
            raise Error('Completion output must be a new explicit private absolute file')
        local.private_dir(path.parent)
        backend = self.result()['backend']
        backend = dict(backend, account_id=self.c['account_id'])
        value = dict(schema=1, kind='bmux-live-r2-bootstrap-completion', operation=operation,
                     backend=backend, created=True, bucket_identity=self.ledger['observed'])
        value['binding'] = local.identity(value)
        # No overwrite, including a concurrent writer. The explicit descriptor
        # admits only a reviewed empty-backend bootstrap, not ordinary restores.
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, 'w') as stream:
            json.dump(value, stream, sort_keys=True)
            stream.write('\n')
            stream.flush()
            os.fsync(stream.fileno())
        directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
        return value

    def reconcile(self, operation):
        self.exact(operation)
        if self.ledger['phase'] != 'applying' or self.ledger['expected'] is None:
            raise Error('Unknown interrupted creation authority; inspect provider and manually recover, never adopt by name')
        if snapshot(self.cf, self.c) != self.ledger['expected']:
            raise Error('Bucket identity differs; manual reconciliation required')
        self.ledger['observed'] = self.ledger['expected']
        self.save('complete')
        return self.result()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', required=True)
    parser.add_argument('--account-id')
    parser.add_argument('--bucket')
    parser.add_argument('--region')
    parser.add_argument('--state-dir', type=Path)
    group = parser.add_mutually_exclusive_group()
    group.add_argument('--plan', action='store_true')
    group.add_argument('--review')
    group.add_argument('--apply')
    group.add_argument('--reconcile')
    parser.add_argument('--approve-import', action='store_true')
    parser.add_argument('--accept-cost', action='store_true')
    parser.add_argument('--completion-file', type=Path)
    args = parser.parse_args()
    try:
        c = choices(args)
        if args.completion_file and (args.plan or not any((args.review, args.apply, args.reconcile))):
            raise Error('Completion file requires exact complete operation')
        if not any((args.plan, args.review, args.apply, args.reconcile)):
            print(json.dumps(public(c), sort_keys=True))
            return 0
        if not args.state_dir or not args.state_dir.is_absolute():
            raise Error('Provide explicit private absolute --state-dir')
        with local.lock(args.state_dir):
            p = Provision(c, args.state_dir, None)
            if args.review:
                p.exact(args.review)
                if p.ledger['phase'] not in ('planned', 'complete'):
                    raise Error('Interrupted operation requires reconcile')
                result = p.result()
            else:
                p.cf = bootstrap.Cloudflare(os.environ)
                if args.plan:
                    result = p.plan(args.approve_import)
                elif args.apply:
                    result = p.apply(args.apply, args.accept_cost)
                else:
                    result = p.reconcile(args.reconcile)
            if args.completion_file:
                operation = args.review or args.apply or args.reconcile
                p.completion(operation, args.completion_file)
            print(json.dumps(result, sort_keys=True))
        return 0
    except (Error, bootstrap.BootstrapError) as error:
        print('ERROR: ' + str(error), file=sys.stderr)
    except (OSError, ValueError, KeyError, TypeError, AttributeError):
        print('ERROR: Invalid private/provider state; output suppressed; inspect before retry', file=sys.stderr)
    return 1


if __name__ == '__main__':
    sys.exit(main())
