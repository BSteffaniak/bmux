"""Independent source coverage for the CLI ownership cutover (no Cargo invocation)."""
import argparse
import collections
import json
from pathlib import Path
import re
import subprocess
import tomllib

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
REV = '3320b28b58b6276e5c656c5206cd1571982d1370'
TABLE = 'packages/cli/src/runtime/built_in_commands.rs'
SCHEMA = 'packages/cli-schema/src/lib.rs'
API = 'packages/plugin/src/host_runtime.rs'
FIXTURE = HERE / 'paths.json'


def git(*args):
    return subprocess.check_output(['git', *args], cwd=ROOT).decode()


def inventory(baseline=False):
    def read(path):
        return git('show', f'{REV}:{path}') if baseline else (ROOT / path).read_text()

    paths = {}

    def entry(path):
        return paths.setdefault(path, {})

    # The baseline derive macro defines nested command trees (including env and
    # access) that are not in 1:1 correspondence with the handler table.
    enums = dict(re.findall(r'pub enum (\w*Command) \{\n(.*?)^\}', read(SCHEMA), re.M | re.S))

    def visit(name, prefix):
        for block in re.split(r'(?=^    [A-Z]\w*[,{( ])', enums[name], flags=re.M)[1:]:
            variant = re.match(r'    (\w+)', block)[1]
            if variant == 'External':
                continue
            segment = re.sub(r'(?<!^)(?=[A-Z])', '-', variant).lower()
            path = (prefix + ' ' + segment).strip()
            entry(path)['schema'] = f'{name}::{variant}'
            nested = re.search(r'#\[command\(subcommand\)\]\s*command: (\w+Command)', block)
            if nested:
                visit(nested[1], path)

    visit('Command', '')
    table = read(TABLE)
    blocks = table.split('BuiltInExecutionCommand::new(')[1:]
    for block in blocks:
        handler = re.match(r'\s*BuiltInHandlerId::(\w+),', block)[1]
        literals = re.findall(r'&\[([^\]]*)\]', block.split('\n    ]')[0])
        for literal in literals:
            path = ' '.join(re.findall(r'"([^"\n]+)"', literal))
            if not path or 'core' in entry(path):
                raise ValueError(f'unrecognized/duplicate execution-table row: {handler} {literal}')
            entry(path)['core'] = handler

    manifests = (git('ls-tree', '-r', '--name-only', REV, 'plugins').splitlines() if baseline
                 else (str(p.relative_to(ROOT)) for p in (ROOT / 'plugins').glob('*/plugin.toml')))
    plugins = {}
    for file in sorted(p for p in manifests if re.fullmatch(r'plugins/[^/]+/plugin\.toml', p)):
        manifest = tomllib.loads(read(file)); owner = manifest['id']
        plugins[owner] = {k: manifest[k] for k in ('owns_namespaces', 'owns_paths') if manifest.get(k)}
        for cmd in manifest.get('commands', []):
            for words in [cmd.get('path') or [cmd['name']], *cmd.get('aliases', [])]:
                path = ' '.join(words)
                entry(path).setdefault('declarations', []).append(f"{owner}:{cmd['name']}")

    # Map actual bridge destinations, not just the presence of an SDK import.
    def proxy(path, owner):
        entry(path).setdefault('called_by_proxy', []).append(owner)

    connections = read('plugins/connections-plugin/src/client_cli.rs') if baseline or (ROOT / 'plugins/connections-plugin/src/client_cli.rs').exists() else ''
    if '.core_cli_command_run_path' in connections:
        for dest in sorted(set(re.findall(r'forward\(\s*context,\s*"([^"]+)"', connections))):
            proxy(dest, 'bmux.connections:connect')
    cluster = read('plugins/cluster-plugin/src/membership.rs')
    if '.core_cli_command_run_path' in cluster:
        for literal in re.findall(r'HealthProbe::\w+ => vec!\[([^\]]+)\]', cluster):
            proxy(' '.join(re.findall(r'"([^"]+)"', literal)), 'bmux.cluster:health-probe')
    plugin_cli = read('plugins/plugin-cli-plugin/src/lib.rs')
    pane = read('plugins/cluster-plugin/src/pane.rs')
    if 'vec!["remote".into(), operation.into()]' in pane:
        for op in re.findall(r'HealthProbe::\w+ => "([^"]+)"', pane):
            proxy('remote ' + op, 'bmux.cluster:health-probe')
    if 'const CORE_COMPAT_COMMANDS:' in plugin_cli:
        table = plugin_cli.split('const CORE_COMPAT_COMMANDS:', 1)[1].split('];', 1)[0]
        for alias, literal in re.findall(r'\(\s*"([^"]+)",\s*&\[([^\]]+)\]', table):
            proxy(' '.join(re.findall(r'"([^"]+)"', literal)), 'bmux.plugin_cli:' + alias)
    elif 'core_proxy_command_path(context.command.as_str())' in plugin_cli:
        for path, row in paths.items():
            for decl in row.get('declarations', []):
                if decl.startswith('bmux.plugin_cli:') and not path.startswith('plugin '):
                    proxy(path, decl)
    if not baseline:
        # Fail closed when the bridge moves to another Rust source file: its
        # destinations need an explicit mapping before the guard can pass.
        known = {'plugins/connections-plugin/src/client_cli.rs', 'plugins/plugin-cli-plugin/src/lib.rs'}
        bridge = re.compile(r'(?:\.|::)core_cli_command_run_path\s*\(|"--core-builtins-only"|"run_path"|CORE_CLI_COMMAND_RUN_PATH_OPERATION_V1')
        for source in (ROOT / 'plugins').rglob('*.rs'):
            if str(source.relative_to(ROOT)) not in known and bridge.search(source.read_text()):
                raise SystemExit(f'unmapped core bridge reference: {source.relative_to(ROOT)}')
    methods = sorted(re.findall(r'^\s*(?:async\s+)?fn (\w+)\b', read(API).split('pub trait HostRuntimeApi', 1)[1].split('\n}', 1)[0], re.M))
    return {'revision': REV, 'host_api': methods, 'claims': plugins, 'paths': dict(sorted(paths.items()))}


def registered_commands(source):
    # Read the two registration forms used by the migrated owners. Fail closed
    # for new registration styles rather than matching arbitrary handler strings.
    declare = source.split('fn declare(', 1)[1].split('\n    }', 1)[0]
    names = set(re.findall(r'\.blocking_command\(\s*"([^"]+)"', declare))
    for literals in re.findall(r'for name in \[([^\]]*)\]\s*\{\s*d\.blocking_command\(name,', declare):
        names.update(re.findall(r'"([^"]+)"', literals))
    return names


def migrated_errors(old, new):
    policy = json.loads((HERE / 'migrated.json').read_text())
    leaves = {path for paths in policy.values() for path in paths}
    schema = {p for p, row in old['paths'].items() if 'schema' in row}
    errors = []
    for path, before in old['paths'].items():
        if path not in leaves and before.get('declarations') != new['paths'].get(path, {}).get('declarations'):
            errors.append(f'{path}: changed plugin owner or callback without a migration disposition')
    for path in schema - leaves:
        if not any(p.startswith(path + ' ') for p in schema) and not new['paths'].get(path, {}).get('schema'):
            errors.append(f'missing migrated leaf disposition: {path}')
    for owner, paths in policy.items():
        folder = ROOT / 'plugins' / (owner.removeprefix('bmux.').replace('_', '-') + '-plugin')
        manifest = tomllib.loads((folder / 'plugin.toml').read_text())
        registered = registered_commands((folder / 'src/lib.rs').read_text())
        for path in paths:
            visible = [c['name'] for c in manifest['commands'] if c.get('expose_in_cli') is True
                       and path.split() in [c.get('path'), *c.get('aliases', [])]]
            if path.split() not in manifest.get('owns_paths', []) and path.split()[0] not in manifest.get('owns_namespaces', []):
                errors.append(f'{path}: {owner} exposes this path without an ownership claim')
            row = new['paths'].get(path, {})
            if (len(visible) != 1 or visible[0] not in registered or manifest['id'] != owner
                    or row.get('core') or row.get('schema') or row.get('called_by_proxy')):
                errors.append(f'{path}: expected exposed registered {owner} leaf without core fallback; visible={visible}')
    return errors


def compare(old, new):
    errors = []
    if added := set(new['host_api']) - set(old['host_api']):
        errors.append(f'new HostRuntimeApi methods: {sorted(added)}')
    for path in sorted(old['paths'].keys() | new['paths'].keys()):
        before = old['paths'].get(path, {}); after = new['paths'].get(path, {})
        decls = after.get('declarations', [])
        covered = set(new['paths'])
        if path not in covered and not any(p.startswith(path + ' ') for p in covered):
            errors.append(f'lost path: {path}')
        if len(set(decls)) > 1:
            errors.append(f'ambiguous path: {path} {decls}')
        if after.get('called_by_proxy') and not after.get('core'):
            errors.append(f'proxy to removed core path: {path} from {after["called_by_proxy"]}')
        if before.get('core') and not after.get('core') and after.get('schema') and not any(p.startswith(path + ' ') for p in covered):
            errors.append(f'stale static leaf after dispatch removal: {path}')
    return errors


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['baseline', 'map', 'check'])
    args = parser.parse_args()
    if args.action == 'check':
        frozen = json.loads(FIXTURE.read_text())
        if frozen != inventory(True):
            raise SystemExit('frozen paths disagree with baseline git objects')
        current = inventory()
        issues = compare(frozen, current) + migrated_errors(frozen, current)
        if issues:
            raise SystemExit('\n'.join(issues))
        print('Baseline path coverage, known proxy destinations and closed host API: OK')
    else:
        print(json.dumps(inventory(args.action == 'baseline'), indent=2, sort_keys=True))
