#!/usr/bin/env python3
"""Read-only Cloudflare discovery; emit explicitly reviewed OpenTofu imports only.

Inputs: CLOUDFLARE_API_TOKEN, CLOUDFLARE_ACCOUNT_ID, CLOUDFLARE_ZONE_ID,
LIVE_HOSTNAME, LIVE_PAGES_PROJECT, LIVE_ENVIRONMENT. --managed is a JSON array
from the selected backend's state list; --imports is a reviewed address -> ID
object. Approval attests the exact resources are not owned by another state.
No provider or state mutation is performed, including when approval is given.
"""
import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import stat
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

BASE = 'https://api.cloudflare.com/client/v4/'
MAX_BYTES = 1024 * 1024
MAX_PAGES = 20
STATE_ADDRESSES = ('cloudflare_r2_bucket.state',)
ADDRESSES = (
    'cloudflare_pages_project.live', 'cloudflare_pages_domain.live',
    'cloudflare_dns_record.live', 'cloudflare_worker.live',
    'cloudflare_worker_version.live', 'cloudflare_workers_deployment.live',
    'cloudflare_workers_route.live',
)


class ImportErrorSafe(Exception):
    """Only fixed local messages, never remote response or credential text."""


def checked(value, pattern, message):
    if not isinstance(value, str) or re.fullmatch(pattern, value) is None:
        raise ImportErrorSafe(message)
    return value


def configuration(env):
    account = checked(env.get('CLOUDFLARE_ACCOUNT_ID'), r'[a-f0-9]{32}', 'Invalid account identity')
    zone = checked(env.get('CLOUDFLARE_ZONE_ID'), r'[a-f0-9]{32}', 'Invalid zone identity')
    hostname = checked(env.get('LIVE_HOSTNAME'), r'(?=.{1,253}$)(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z]{2,63}', 'Invalid hostname')
    pages = checked(env.get('LIVE_PAGES_PROJECT'), r'[a-z0-9][a-z0-9-]{0,57}', 'Invalid Pages project name')
    environment = checked(env.get('LIVE_ENVIRONMENT'), r'staging|production', 'Invalid environment')
    token = checked(env.get('CLOUDFLARE_API_TOKEN'), r'[A-Za-z0-9_.-]{1,512}', 'Missing or invalid API token')
    bucket = checked(env.get('LIVE_STATE_BUCKET'), r'[a-z0-9][a-z0-9-]{1,61}[a-z0-9]', 'Invalid R2 state bucket')
    return dict(account=account, zone=zone, hostname=hostname, pages=pages,
                worker='bmux-live-' + environment, token=token, bucket=bucket)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise ImportErrorSafe('Provider redirect rejected')


class Cloudflare:
    def __init__(self, token, opener=None, clock=time.monotonic):
        self.token = token
        # Do not inherit proxy settings that could receive the bearer token.
        self.opener = opener or urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
        self.clock = clock
        self.deadline = clock() + 30

    def page(self, endpoint, page, cursor=None):
        remaining = self.deadline - self.clock()
        if remaining <= 0:
            raise ImportErrorSafe('Provider discovery deadline exceeded')
        path = endpoint.split('?')[0]
        buckets = path.endswith('/r2/buckets')
        bucket_detail = bool(re.fullmatch(r'accounts/[a-f0-9]{32}/r2/buckets/[a-z0-9-]+', path))
        unpaged = bucket_detail or path.endswith(('/workers/scripts', '/workers/routes', '/lock', '/lifecycle'))
        # Pages rejects per_page=100. Use its supported size, also as a
        # conservative numbered-page size for DNS and Workers inventories.
        size = 10
        params = {} if unpaged else {'per_page': size}
        if buckets:
            if cursor is not None:
                params['cursor'] = cursor
        elif not unpaged:
            params['page'] = page
        separator = '&' if '?' in endpoint else '?'
        url = BASE + endpoint + (separator + urllib.parse.urlencode(params) if params else '')
        request = urllib.request.Request(url, headers={'Authorization': 'Bearer ' + self.token}, method='GET')
        try:
            with self.opener.open(request, timeout=min(5, remaining)) as response:
                if response.status != 200 or response.geturl() != url:
                    raise ImportErrorSafe('Unexpected provider response')
                data = bytearray()
                while True:
                    if self.clock() >= self.deadline:
                        raise ImportErrorSafe('Provider discovery deadline exceeded')
                    block = response.read(min(65536, MAX_BYTES + 1 - len(data)))
                    data.extend(block)
                    if len(data) > MAX_BYTES:
                        raise ImportErrorSafe('Provider response exceeds bound')
                    if not block:
                        break
                answer = json.loads(data)
        except ImportErrorSafe:
            raise
        except urllib.error.HTTPError as error:
            # Only local endpoint category and numeric status: never echo the
            # URL, bearer token, provider body, or untrusted exception text.
            category = ('R2 inventory' if buckets else 'complete collection' if unpaged else 'numbered inventory')
            raise ImportErrorSafe(f'{category} request rejected (HTTP {error.code})') from None
        except (OSError, ValueError, RecursionError, http.client.HTTPException, urllib.error.URLError):
            raise ImportErrorSafe('Provider discovery failed') from None
        if not isinstance(answer, dict) or answer.get('success') is not True:
            raise ImportErrorSafe('Provider discovery rejected')
        items = answer.get('result')
        if bucket_detail:
            if not isinstance(items, dict):
                raise ImportErrorSafe('Invalid R2 bucket detail')
            items = [items]
        elif '/r2/buckets' in url and isinstance(items, dict):
            if items.get('cursor'):
                raise ImportErrorSafe('R2 inventory truncated; reconcile manually')
            items = items.get('rules') if url.split('?')[0].endswith(('/lock', '/lifecycle')) else items.get('buckets')
        if not isinstance(items, list) or len(items) > 100 or any(not isinstance(item, dict) for item in items):
            raise ImportErrorSafe('Unsupported provider list response')
        info = answer.get('result_info')
        if unpaged:
            if info is not None:
                raise ImportErrorSafe('Unexpected pagination on complete provider collection')
            return items, 1
        if buckets:
            # R2 omits result_info entirely on its terminal short page.
            if info is None:
                if len(items) >= size:
                    raise ImportErrorSafe('R2 pagination completeness unavailable')
                return items, None
            if not isinstance(info, dict) or info.get('per_page') != size:
                raise ImportErrorSafe('Invalid R2 cursor metadata')
            next_cursor = info.get('cursor')
            if next_cursor is not None and (not isinstance(next_cursor, str) or len(next_cursor) > 4096):
                raise ImportErrorSafe('Invalid R2 cursor')
            if len(items) > size:
                raise ImportErrorSafe('Oversized R2 page')
            if items and not next_cursor:
                raise ImportErrorSafe('R2 pagination completeness unavailable')
            return items, next_cursor or None
        if info is None:
            if len(items) >= size:
                raise ImportErrorSafe('Provider pagination completeness unavailable')
            return items, 1
        if not isinstance(info, dict):
            raise ImportErrorSafe('Invalid pagination metadata')
        pages = info.get('total_pages')
        current = info.get('page')
        if pages is None:
            count = info.get('total_count')
            per_page = info.get('per_page')
            if type(count) is not int or count < 0 or type(per_page) is not int or per_page != size:
                raise ImportErrorSafe('Unsupported provider pagination counts')
            pages = max(1, (count + size - 1) // size)
        if pages == 0 and type(pages) is int and current == page == 1 and not items:
            return items, 1
        if type(pages) is not int or type(current) is not int or current != page or not 1 <= pages <= MAX_PAGES or page > pages:
            raise ImportErrorSafe('Unsupported provider pagination')
        return items, pages

    def listing(self, endpoint):
        result, total = self.page(endpoint, 1)
        if endpoint.split('?')[0].endswith('/r2/buckets'):
            seen = set()
            for page in range(2, MAX_PAGES + 1):
                if total is None:
                    return result
                if total in seen:
                    raise ImportErrorSafe('Repeated R2 pagination cursor')
                seen.add(total)
                items, total = self.page(endpoint, page, cursor=total)
                result.extend(items)
            if total is not None:
                raise ImportErrorSafe('R2 pagination exceeds bound')
            return result
        for page in range(2, total + 1):
            items, observed = self.page(endpoint, page)
            if observed != total:
                raise ImportErrorSafe('Provider pagination changed during discovery')
            result.extend(items)
        return result


def unique(items, field, expected):
    matches = [item for item in items if item.get(field) == expected]
    if len(matches) > 1:
        raise ImportErrorSafe('Multiple matching provider identities; reconcile manually')
    return matches[0] if matches else None


def resource_id(item):
    return checked(item.get('id'), r'[a-f0-9]{32}', 'Unsupported provider resource identity')


def discover(api, config, managed):
    account, zone = config['account'], config['zone']
    projects = api.listing(f'accounts/{account}/pages/projects')
    project = unique(projects, 'name', config['pages'])
    found = {}
    bucket = unique(api.listing(f'accounts/{account}/r2/buckets'), 'name', config['bucket'])
    if bucket is None:
        raise ImportErrorSafe('Selected R2 backend bucket missing; finish explicit bootstrap first')
    # List responses omit storage_class; fetch the selected bucket's detail
    # rather than treating omitted fields as defaults.
    details = api.listing(f'accounts/{account}/r2/buckets/{config["bucket"]}')
    if len(details) != 1 or details[0].get('name') != bucket['name'] or details[0].get('creation_date') != bucket.get('creation_date'):
        raise ImportErrorSafe('Selected R2 backend bucket identity changed')
    bucket = details[0]
    if bucket.get('storage_class') != 'Standard' or not isinstance(bucket.get('creation_date'), str):
        raise ImportErrorSafe('Unsupported R2 backend bucket identity')
    found[STATE_ADDRESSES[0]] = account + '/' + config['bucket'] + '/default'
    # These two provider resources cannot be imported. Never overwrite a shared
    # bucket policy while adopting the bucket into this stack.
    for suffix, address in (('lock', 'cloudflare_r2_bucket_lock.state'),
                            ('lifecycle', 'cloudflare_r2_bucket_lifecycle.state')):
        rules = api.listing(f'accounts/{account}/r2/buckets/{config["bucket"]}/{suffix}')
        # A pristine Cloudflare multipart-abort baseline is preserved verbatim
        # in state.tf. This is not permission to replace custom lifecycle policy;
        # every other unmanaged rule set still requires ownership reconciliation.
        default_abort = [{
            'id': 'Default Multipart Abort Rule', 'enabled': True, 'conditions': {},
            'abortMultipartUploadsTransition': {'condition': {'type': 'Age', 'maxAge': 604800}},
        }]
        preserved_baseline = suffix == 'lifecycle' and rules == default_abort
        if rules and address not in managed and not preserved_baseline:
            raise ImportErrorSafe('Existing unmanaged R2 policy requires explicit ownership reconciliation')
    if project is not None:
        found[ADDRESSES[0]] = account + '/' + config['pages']
    # Prove the hostname is not already attached to a different Pages project.
    domain_matches = []
    for item in projects:
        name = checked(item.get('name'), r'[a-z0-9][a-z0-9-]{0,57}', 'Unsupported Pages project identity')
        domain = unique(api.listing(f'accounts/{account}/pages/projects/{name}/domains'), 'name', config['hostname'])
        if domain is not None:
            domain_matches.append(name)
    if len(domain_matches) > 1 or domain_matches and domain_matches != [config['pages']]:
        raise ImportErrorSafe('Hostname belongs to another Pages project; reconcile manually')
    if domain_matches:
        found[ADDRESSES[1]] = account + '/' + config['pages'] + '/' + config['hostname']
    query = urllib.parse.urlencode({'name': config['hostname']})
    # Mail and verification records legitimately share the hostname. Only
    # address/alias records compete with the Pages routing record; never import
    # or rewrite the unrelated MX/TXT policy.
    dns_records = api.listing(f'zones/{zone}/dns_records?{query}')
    dns = unique([item for item in dns_records if item.get('type') in ('A', 'AAAA', 'CNAME')],
                 'name', config['hostname'])
    if dns is not None:
        if dns.get('type') != 'CNAME' or dns.get('content') != config['pages'] + '.pages.dev':
            raise ImportErrorSafe('Hostname DNS points elsewhere; reconcile manually')
        found[ADDRESSES[2]] = zone + '/' + resource_id(dns)
    worker = unique(api.listing(f'accounts/{account}/workers/workers'), 'name', config['worker'])
    classic = unique(api.listing(f'accounts/{account}/workers/scripts'), 'id', config['worker'])
    if classic is not None and worker is None:
        raise ImportErrorSafe('Existing classic Worker ownership requires manual reconciliation')
    if worker is not None:
        # Never guess Worker/version/deployment/DO migration ownership from a name.
        if not set(ADDRESSES[3:6]).issubset(managed):
            raise ImportErrorSafe('Existing Worker/version/Durable Object ownership requires manual reconciliation')
    route = unique(api.listing(f'zones/{zone}/workers/routes'), 'pattern', config['hostname'] + '/api/live/*')
    if route is not None:
        if route.get('script') != config['worker'] or worker is None:
            raise ImportErrorSafe('Existing route ownership requires manual reconciliation')
        found[ADDRESSES[6]] = zone + '/' + resource_id(route)
    return {address: provider_id for address, provider_id in sorted(found.items()) if address not in managed}


def read_json(path):
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(fd, 'rb') as stream:
            info = os.fstat(stream.fileno())
            if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_size > MAX_BYTES:
                raise ImportErrorSafe('Input must be a bounded owned regular JSON file')
            raw = stream.read(MAX_BYTES + 1)
            if len(raw) > MAX_BYTES:
                raise ImportErrorSafe('Input JSON exceeds bound')
            def object_pairs(pairs):
                result = {}
                for key, value in pairs:
                    if key in result:
                        raise ImportErrorSafe('Duplicate input JSON key')
                    result[key] = value
                return result
            return json.loads(raw, object_pairs_hook=object_pairs)
    except ImportErrorSafe:
        raise
    except (OSError, ValueError, RecursionError):
        raise ImportErrorSafe('Unable to read input JSON') from None


def managed_addresses(value):
    if not isinstance(value, list) or len(value) > 1000 or any(not isinstance(address, str) or len(address) > 512 for address in value):
        raise ImportErrorSafe('Managed addresses must be a bounded JSON string array')
    if len(set(value)) != len(value):
        raise ImportErrorSafe('Duplicate managed address')
    return set(value)


def reviewed_imports(value):
    if not isinstance(value, dict) or set(value) - set(ADDRESSES + STATE_ADDRESSES):
        raise ImportErrorSafe('Unsupported reviewed import addresses')
    for provider_id in value.values():
        checked(provider_id, r'[a-z0-9.-]+(?:/[a-z0-9.-]+){1,2}', 'Invalid reviewed provider identity')
    return value


def paths(artifact, output):
    artifact = Path(artifact)
    output = Path(output).absolute()
    try:
        if not artifact.is_absolute() or artifact.resolve(strict=True) != artifact or not artifact.is_dir():
            raise ImportErrorSafe('Artifact must be an absolute non-symlink module directory')
        main = artifact / 'main.tf'
        if main.is_symlink() or not main.is_file() or main.stat().st_size > MAX_BYTES:
            raise ImportErrorSafe('Artifact module is invalid')
        text = main.read_text()
        if '5.17.0' not in text or any('"' + address.split('.')[0] + '" "live"' not in text for address in ADDRESSES):
            raise ImportErrorSafe('Artifact is not the supported Cloudflare 5.17 live module')
        state = artifact / 'state.tf'
        if (state.is_symlink() or not state.is_file() or state.stat().st_size > MAX_BYTES
                or any('"' + kind + '" "state"' not in state.read_text()
                       for kind in ('cloudflare_r2_bucket', 'cloudflare_r2_bucket_lock', 'cloudflare_r2_bucket_lifecycle'))):
            raise ImportErrorSafe('Artifact lacks the supported R2 state authority')
        parent = output.parent
        info = parent.lstat()
        if parent.resolve(strict=True) != parent or not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise ImportErrorSafe('Output directory must be owned, non-symlink, mode 0700')
        if not output.name.endswith('.tf.json') or output.exists() or output.is_symlink():
            raise ImportErrorSafe('Output must be a new .tf.json file')
    except ImportErrorSafe:
        raise
    except (OSError, ValueError):
        raise ImportErrorSafe('Invalid artifact or output path') from None
    return output


def generate(output, found, reviewed, approved):
    if found and (not approved or reviewed != found):
        raise ImportErrorSafe('Imports require --approve-import and an exact --imports mapping; attest no other state owns these identities')
    if not found and reviewed:
        raise ImportErrorSafe('Reviewed mapping differs from discovered unmanaged identities')
    document = {'import': [{'to': address, 'id': provider_id} for address, provider_id in sorted(found.items())]}
    try:
        fd = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, 'w') as stream:
            json.dump(document, stream, sort_keys=True)
            stream.write('\n')
            stream.flush()
            os.fsync(stream.fileno())
    except OSError:
        raise ImportErrorSafe('Unable to create private import file') from None


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--artifact', required=True)
    parser.add_argument('--output', required=True)
    parser.add_argument('--managed', required=True)
    parser.add_argument('--imports')
    parser.add_argument('--approve-import', action='store_true')
    args = parser.parse_args(argv)
    try:
        config = configuration(os.environ)
        output = paths(args.artifact, args.output)
        managed = managed_addresses(read_json(args.managed))
        reviewed = reviewed_imports(read_json(args.imports)) if args.imports else {}
        found = discover(Cloudflare(config['token']), config, managed)
        encoded = json.dumps(found, sort_keys=True, separators=(',', ':')).encode()
        print(json.dumps({'imports': found, 'fingerprint': hashlib.sha256(encoded).hexdigest()}, sort_keys=True))
        generate(output, found, reviewed, args.approve_import)
    except ImportErrorSafe as error:
        print('Live import discovery: ' + str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
