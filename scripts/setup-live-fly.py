#!/usr/bin/env python3
"""Guide missing Fly configuration installation; never create resources or deploy.

Secrets travel only through private prompts/environment, HTTPS headers and gh stdin.
No credentials or resume files are written. Existing repository values win.
"""
import argparse
import getpass
import hashlib
import json
import os
import re
import selectors
import subprocess
import time
import urllib.request
import warnings

VARIABLES = {'LIVE_FLY_APP', 'LIVE_FLY_ORG', 'LIVE_FLY_REGION'}
SECRETS = {'FLY_API_TOKEN', 'LIVE_METRICS_KEY_SHA256'}
MAX_RESPONSE = 65536
TIMEOUT = 30
# Public contract: superfly/fly-go schema.graphql (Query and FlyPlatform).
DISCOVERY = '''query {
  organizations(first: 100) { nodes { slug } pageInfo { hasNextPage } }
  platform { regions { code deprecated } }
}'''
AVAILABILITY = 'query($name: String!) { appNameAvailable(name: $name) }'


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise ValueError('Fly redirect rejected')


def graphql(token, query, variables=None):
    request = urllib.request.Request(
        'https://api.fly.io/graphql', method='POST',
        data=json.dumps({'query': query, 'variables': variables or {}}).encode(),
        headers={'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'})
    # Never forward the private header to a redirected endpoint.
    with urllib.request.build_opener(NoRedirect()).open(request, timeout=TIMEOUT) as response:
        raw = response.read(MAX_RESPONSE + 1)
    if len(raw) > MAX_RESPONSE:
        raise ValueError('Oversized Fly response')
    result = json.loads(raw)
    if not isinstance(result, dict) or result.get('errors') or not isinstance(result.get('data'), dict):
        raise ValueError('Fly query rejected')
    return result['data']


def run_gh(command, *, input, env, check, capture_output, timeout):
    """Bound subprocess output while draining it, not after unbounded capture."""
    del check, capture_output  # all callers require checked, private capture
    output = bytearray()
    deadline = time.monotonic() + timeout
    with subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                          stderr=subprocess.DEVNULL, env=env) as process:
        try:
            with selectors.DefaultSelector() as selector:
                os.set_blocking(process.stdout.fileno(), False)
                selector.register(process.stdout, selectors.EVENT_READ)
                pending = memoryview(input or b'')
                if pending:
                    os.set_blocking(process.stdin.fileno(), False)
                    selector.register(process.stdin, selectors.EVENT_WRITE)
                else:
                    process.stdin.close()
                while selector.get_map():
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise subprocess.TimeoutExpired('gh', timeout)
                    for key, _ in selector.select(remaining):
                        if key.fileobj is process.stdin:
                            count = os.write(process.stdin.fileno(), pending[:4096])
                            pending = pending[count:]
                            if not pending:
                                selector.unregister(process.stdin)
                                process.stdin.close()
                        else:
                            chunk = os.read(process.stdout.fileno(), 4096)
                            if not chunk:
                                selector.unregister(process.stdout)
                            else:
                                output.extend(chunk)
                                if len(output) > MAX_RESPONSE:
                                    raise ValueError('Oversized GitHub output')
            status = process.wait(timeout=max(0.001, deadline - time.monotonic()))
            if status:
                raise subprocess.CalledProcessError(status, command)
            return subprocess.CompletedProcess(command, status, stdout=bytes(output))
        except BaseException:
            process.kill()
            process.wait()
            raise


def gh(kind, action, repo, name=None, value=None):
    command = ['gh', kind, action]
    if name:
        command.append(name)
    command += ['--repo', repo]
    if kind == 'secret':
        command += ['--env', 'Production']
    if action == 'list':
        command += ['--json', 'name,value' if kind == 'variable' else 'name']
    # Do not pass the discovery token to gh, even when supplied in the environment.
    env = {k: v for k, v in os.environ.items() if k not in SECRETS}
    result = run_gh(command, input=value.encode() if value is not None else None,
                    env=env, check=True, capture_output=True, timeout=TIMEOUT)
    if action == 'set':
        return None
    if len(result.stdout) > MAX_RESPONSE:
        raise ValueError('Oversized GitHub inventory')
    rows = json.loads(result.stdout)
    if not isinstance(rows, list) or len(rows) > 1000:
        raise ValueError('Invalid GitHub inventory')
    found = {}
    for row in rows:
        if not isinstance(row, dict) or not isinstance(row.get('name'), str) or row['name'] in found:
            raise ValueError('Invalid GitHub inventory')
        value = row.get('value') if kind == 'variable' else True
        if kind == 'variable' and not isinstance(value, str):
            raise ValueError('Invalid GitHub variable')
        found[row['name']] = value
    return found


def private(prompt):
    # getpass's non-TTY fallback can echo; fail rather than accept that fallback.
    with warnings.catch_warnings():
        warnings.simplefilter('error', getpass.GetPassWarning)
        value = getpass.getpass(prompt)
    if not value or len(value) > 16384 or any(ord(c) < 32 or ord(c) == 127 for c in value):
        raise ValueError('Invalid private value')
    return value


def valid_name(value):
    return isinstance(value, str) and re.fullmatch(r'[a-z0-9][a-z0-9-]{0,62}', value)


def discover(token):
    data = graphql(token, DISCOVERY)
    orgs = data['organizations']
    if orgs['pageInfo']['hasNextPage'] is not False:
        raise ValueError('Organization inventory incomplete; limit is 100')
    nodes, regions = orgs['nodes'], data['platform']['regions']
    if not isinstance(nodes, list) or not 0 < len(nodes) <= 100:
        raise ValueError('Invalid organizations')
    if not isinstance(regions, list) or not 0 < len(regions) <= 500:
        raise ValueError('Invalid regions')
    organizations, available = [], []
    for node in nodes:
        slug = node['slug']
        if not valid_name(slug) or slug in organizations:
            raise ValueError('Invalid organization')
        organizations.append(slug)
    seen = set()
    for region in regions:
        code = region['code']
        if not isinstance(code, str) or not re.fullmatch('[a-z0-9]{3}', code) or code in seen:
            raise ValueError('Invalid region')
        seen.add(code)
        if type(region['deprecated']) is not bool:
            raise ValueError('Invalid region status')
        if not region['deprecated']:
            available.append(code)
    if not available:
        raise ValueError('No available regions')
    return sorted(organizations), sorted(available)


def select(label, choices):
    for index, choice in enumerate(choices, 1):
        print(f'{index}. {choice}')
    answer = input(f'{label} (number or exact value; blank cancels): ').strip()
    if answer in choices:
        return answer
    if answer.isascii() and answer.isdigit() and len(answer) <= 3 and 1 <= int(answer) <= len(choices):
        return choices[int(answer) - 1]
    raise ValueError('Selection cancelled or invalid')


def next_step(repo):
    print('Manual next command (not dispatched; local setup authorizes no deployment spending):')
    print(f'gh workflow run live-promote.yml --repo {repo} --ref master -f migrate=true')
    print('Review the workflow preview, then approve production-approval to proceed.')


def install(repo, approve):
    secrets = gh('secret', 'list', repo)
    existing = gh('variable', 'list', repo)
    missing_secrets, missing_vars = SECRETS - secrets.keys(), VARIABLES - existing.keys()
    print('Missing Production secrets: ' + (', '.join(sorted(missing_secrets)) or 'none'))
    print('Missing repository variables: ' + (', '.join(sorted(missing_vars)) or 'none'))
    if not (missing_secrets or missing_vars):
        next_step(repo)
        return
    if not approve:
        return
    token = os.environ.get('FLY_API_TOKEN')
    if not token:
        token = private('Existing Fly token (private; discovery only if already installed): ')
    if len(token) > 16384 or any(ord(c) < 32 or ord(c) == 127 for c in token):
        raise ValueError('Invalid token')
    organizations, regions = discover(token)
    config = {name: existing[name] for name in VARIABLES if name in existing}
    if 'LIVE_FLY_ORG' not in config:
        config['LIVE_FLY_ORG'] = select('Fly organization', organizations)
    if config['LIVE_FLY_ORG'] not in organizations:
        raise ValueError('Existing organization unavailable; not overwritten')
    if 'LIVE_FLY_REGION' not in config:
        config['LIVE_FLY_REGION'] = select('Fly region', regions)
    if config['LIVE_FLY_REGION'] not in regions:
        raise ValueError('Existing region unavailable; not overwritten')
    if 'LIVE_FLY_APP' not in config:
        config['LIVE_FLY_APP'] = input('New Fly app name (blank cancels): ').strip()
    if not valid_name(config['LIVE_FLY_APP']):
        raise ValueError('Invalid app name')
    if graphql(token, AVAILABILITY, {'name': config['LIVE_FLY_APP']}).get('appNameAvailable') is not True:
        raise ValueError('App unavailable; no implicit adoption')
    values = {}
    if 'FLY_API_TOKEN' in missing_secrets:
        values['FLY_API_TOKEN'] = token
    if 'LIVE_METRICS_KEY_SHA256' in missing_secrets:
        key = private('Existing independently backed-up metrics key (private, at least 32 characters): ')
        if len(key) < 32 or key != private('Confirm existing metrics key (private): '):
            raise ValueError('Invalid metrics key')
        if input('Confirm original metrics key is backed up independently (type BACKED UP): ') != 'BACKED UP':
            raise ValueError('Metrics backup not confirmed')
        values['LIVE_METRICS_KEY_SHA256'] = hashlib.sha256(key.encode()).hexdigest()
    print('Public Fly configuration (existing explicit values preserved):')
    for name, value in sorted(config.items()):
        print(f'{name}={value}')
    print('No resources, state bootstrap, token rotation, or deployment will be performed.')
    print('Future deployments can incur Fly billing for this organization, Machines and persistent volumes.')
    if input('Authorize missing values only and acknowledge billing (type I ACKNOWLEDGE BILLING): ') != 'I ACKNOWLEDGE BILLING':
        raise ValueError('Installation cancelled')
    # Re-read before each write. If a concurrent installer changed any selected
    # public config, stop; never overwrite a newly observed secret or variable.
    for kind, pending in [('variable', {n: config[n] for n in missing_vars}), ('secret', values)]:
        for name, value in sorted(pending.items()):
            current = gh('variable', 'list', repo)
            if any(n in current and current[n] != v for n, v in config.items()):
                raise ValueError('Configuration changed during installation')
            inventory = current if kind == 'variable' else gh('secret', 'list', repo)
            if name not in inventory:
                gh(kind, 'set', repo, name, value)
    print('Missing Fly configuration installed. No deployment dispatched.')
    next_step(repo)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', default='BSteffaniak/bmux')
    parser.add_argument('--approve', action='store_true')
    args = parser.parse_args()
    if not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', args.repo):
        parser.error('Invalid repository')
    try:
        install(args.repo, args.approve)
    except (OSError, ValueError, KeyError, TypeError, AttributeError, EOFError,
            KeyboardInterrupt, getpass.GetPassWarning, subprocess.SubprocessError):
        parser.exit(2, 'Fly setup stopped; diagnostics suppressed. Partial installation is possible; rerun without --approve to inspect missing names.\n')


if __name__ == '__main__':
    main()
