"""Explicit, read-only provider verification for an interrupted new-bucket operation.

The original provisioner and GitHub adapter remain implementation-bound. Recovery
never changes their binding, expected identity, operation ID, or resource plan.
"""
from datetime import datetime, timezone
import json
import re
import urllib.error
import urllib.parse
import urllib.request
import uuid


class RecoveryError(Exception):
    pass


def instant(value):
    if not isinstance(value, str):
        raise RecoveryError('Missing creation/audit timestamp')
    try:
        result = datetime.fromisoformat(value.replace('Z', '+00:00'))
        if result.tzinfo is None:
            raise ValueError('timezone missing')
        return result.astimezone(timezone.utc)
    except ValueError:
        raise RecoveryError('Invalid creation/audit timestamp') from None


class Reader:
    """Only GET, bounded bodies, no redirects, safe status/error codes, no raw diagnostics."""
    def __init__(self, token, no_redirect):
        if not token or any(c.isspace() for c in token):
            raise RecoveryError('Provisioning token required for read-only verification')
        self.token = token
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), no_redirect)

    def get(self, path, stage):
        request = urllib.request.Request('https://api.cloudflare.com/client/v4' + path,
                                         headers={'Authorization': 'Bearer ' + self.token})
        try:
            with self.opener.open(request, timeout=30) as response:
                raw = response.read(1048577)
                if len(raw) > 1048576:
                    raise RecoveryError(stage + ': response exceeds bound')
                data = json.loads(raw)
            if not isinstance(data, dict) or data.get('success') is not True:
                raise RecoveryError(stage + ': Cloudflare rejected verification; no local reconciliation')
            return data
        except urllib.error.HTTPError as error:
            # Never print bodies/messages, tokens, query strings, or raw exceptions.
            codes = []
            try:
                data = json.loads(error.read(65537))
                codes = [v['code'] for v in data.get('errors', []) if isinstance(v, dict) and type(v.get('code')) is int][:8]
            except (ValueError, TypeError, AttributeError, OSError):
                pass
            raise RecoveryError(stage + ': HTTP ' + str(error.code) + '; Cloudflare codes '
                                + json.dumps(codes) + '; check scoped permissions (Account Settings Read for audit); response suppressed') from None
        except (OSError, urllib.error.URLError, ValueError):
            raise RecoveryError(stage + ': transport/response failed; no local reconciliation; response suppressed') from None


def evidence(reader, choices, audit_id, audit_time, started):
    account, bucket = choices['account_id'], choices['bucket']
    endpoint = '/accounts/' + account
    data = reader.get(endpoint + '/audit_logs?' + urllib.parse.urlencode(dict(id=audit_id, per_page=2)), 'Verify creation audit')
    rows = data.get('result')
    if not isinstance(rows, list) or len(rows) != 1:
        raise RecoveryError('Exact unique audit record required; truncated/missing inventory rejected')
    audit = rows[0]
    if not isinstance(audit, dict):
        raise RecoveryError('Unsupported audit record')
    action, resource = audit.get('action'), audit.get('resource')
    if (audit.get('id') != audit_id or not isinstance(action, dict) or not isinstance(resource, dict)
            or action.get('result') is not True or action.get('type') not in ('create', 'CreateBucket')
            or resource.get('type') != 'r2.bucket' or resource.get('id') != bucket
            or audit.get('interface') != 'API' or audit.get('owner', {}).get('id') != account):
        raise RecoveryError('Audit identity/action/account/outcome mismatch; no reconciliation')
    when = instant(audit.get('when'))
    supplied = instant(audit_time)
    if when != supplied:
        raise RecoveryError('Audit timestamp differs from supplied --creation-time: API=' + when.isoformat()
                            + '; supplied=' + supplied.isoformat()
                            + '. Use the exact API timestamp for this verified audit UUID; no reconciliation performed')
    if not 0 <= (when - instant(started)).total_seconds() <= 300:
        raise RecoveryError('Audit timestamp outside original operation creation window: API=' + when.isoformat()
                            + '; original checkpoint=' + instant(started).isoformat()
                            + '. Preserve original checkpoint; no reconciliation performed')
    data = reader.get(endpoint + '/r2/buckets/' + urllib.parse.quote(bucket, safe=''), 'Verify live bucket identity')
    row = data.get('result')
    if (not isinstance(row, dict) or row.get('name') != bucket or row.get('storage_class') != 'Standard'
            or abs((instant(row.get('creation_date')) - when).total_seconds()) > 5):
        raise RecoveryError('Live bucket generation differs from successful creation audit')
    return dict(audit_id=audit_id, audit_time=when.isoformat(), operation_started=started,
                bucket_identity={k: row[k] for k in ('name', 'creation_date', 'storage_class')})


def recover(p, choices, root, operation, audit_id, audit_time, reader, confirm):
    """Verify twice; durably journal evidence before acknowledging the original creation."""
    if not re.fullmatch(r'[a-f0-9]{32}', operation or ''):
        raise RecoveryError('Recovery requires exact original operation ID')
    try:
        if str(uuid.UUID(audit_id)) != audit_id:
            raise ValueError('noncanonical audit id')
    except (ValueError, TypeError, AttributeError):
        raise RecoveryError('Recovery requires canonical audit record UUID') from None
    instant(audit_time)
    local = p.local
    with local.lock(root):
        obj = p.Provision(choices, root, None)  # Original implementation/choice validation remains mandatory.
        obj.exact(operation)
        if obj.ledger['expected'] is not None or obj.ledger['resources'] != [dict(address='cloudflare_r2_bucket.state', actions=['create'], **{'import': False})]:
            raise RecoveryError('Recovery only supports the original reviewed new-bucket creation')
        journal_path = root / ('creation-recovery-' + operation + '.json')
        if journal_path.is_symlink():
            raise RecoveryError('Recovery journal must not be a symlink')
        journal = local.private_read(journal_path) if journal_path.exists() else None
        if obj.ledger['phase'] == 'complete':
            if (not isinstance(journal, dict) or journal.get('operation') != operation
                    or journal.get('binding') != obj.binding or journal.get('choices') != choices
                    or journal.get('audit_id') != audit_id or instant(journal.get('audit_time')) != instant(audit_time)
                    or journal.get('bucket_identity') != obj.ledger['observed']):
                raise RecoveryError('Completed operation has no matching recovery evidence')
            started = journal['operation_started']
        elif obj.ledger['phase'] == 'applying' and obj.ledger['observed'] is None:
            started = datetime.fromtimestamp(obj.path.stat().st_mtime, timezone.utc).isoformat()
        else:
            raise RecoveryError('Exact interrupted new-bucket creation required')
        observed = evidence(reader, choices, audit_id, audit_time, started)
        expected = dict(schema=1, operation=operation, binding=obj.binding, choices=choices, **observed)
        if journal is not None and journal != expected:
            raise RecoveryError('Conflicting durable recovery evidence; never replace')
        print(json.dumps(dict(recovery=expected, original_phase=obj.ledger['phase']), indent=2))
        if obj.ledger['phase'] == 'complete':
            return  # Verified replay: no mutation, no repeated confirmation.
        confirm('Confirm this audit record is the original setup creation; reconcile locally without recreating the bucket?')
        if evidence(reader, choices, audit_id, audit_time, started) != observed:
            raise RecoveryError('Bucket/audit changed during recovery review')
        # Retain original authority and timestamp before advancing phase; repeatable after crash.
        if journal is None:
            local.save(journal_path, expected)
        original_path = root / ('before-creation-recovery-' + operation + '.json')
        if original_path.exists():
            if local.private_read(original_path) != obj.ledger:
                raise RecoveryError('Original recovery checkpoint differs; no overwrite')
        else:
            local.save(original_path, obj.ledger)
        obj.ledger['observed'] = observed['bucket_identity']
        obj.save('complete')
        print('Original bucket creation reconciled. No Cloudflare writes performed.')
