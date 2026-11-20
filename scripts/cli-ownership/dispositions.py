"""Project only explicitly migrated paths out of frozen transcripts; never re-bless output."""
import base64
import functools
import gzip
import json
import re
from pathlib import Path

AUDIT = json.loads(Path(__file__).with_name('command-dispositions.json').read_text())['paths']
MIGRATED = json.loads(Path(__file__).with_name('migrated.json').read_text())
# Namespace ownership follows the last installed child. Top-level `attach`
# deliberately stays out: a missing sessions plugin must not hide that command.
NAMESPACES = {'bmux.connections': ['access', 'auth', 'kiosk', 'remote', 'remote complete', 'remote trust'],
              'bmux.playbook': ['playbook'], 'bmux.sessions': ['session'],
              'bmux.performance': ['perf'], 'bmux.plugin_cli': ['diagnostics', 'logs profiles']}
LOCAL = {'bmux.clients', 'bmux.sessions'}


def absent_paths(mode):
    return {p for owner, leaves in MIGRATED.items()
            if mode in ('bare', 'disabled') or (mode == 'local-splits' and owner not in LOCAL)
            for p in leaves + NAMESPACES.get(owner, [])}


def record(code, stdout='', stderr=''):
    return [code, *(base64.b64encode(s.encode()).decode() for s in (stdout, stderr))]


def projected_help(data, parent, absent):
    # Only filter declared children, only in the Commands block. Alignment
    # changes are permitted there; ordering, summaries and all other bytes stay checked.
    before, marker, tail = data.partition(b'\nCommands:\n')
    if not marker:
        return data
    commands, gap, rest = tail.partition(b'\n\n')
    lines = []
    for line in commands.split(b'\n'):
        m = re.fullmatch(rb'  ([\w-]+)(?:  +(.+))?', line)
        if not m:
            lines.append(line)
        elif (parent + ' ' + m[1].decode()).strip() not in absent:
            lines.append(b'  ' + m[1] + b'  ' + (m[2] or b''))
    return before + marker + b'\n'.join(lines) + gap + rest


# These commands are now bundled with Windows because the tabs backend requires
# the Workspaces service for admission (packages/cli/Cargo.toml). Each is the
# same descriptor as the historically installed default-profile owner. Compare
# to its *frozen* installed transcript, not to another candidate invocation.
WORKSPACE_CASES = frozenset('help workspace ' + leaf for leaf in (
    'activate-tab', 'kill', 'last', 'list', 'move-tab', 'new', 'next', 'previous', 'rename', 'switch'))


@functools.cache
def installed_reference(profile='default'):
    with gzip.open(Path(__file__).parent / f'transcripts/{profile}.json.gz') as stream:
        return json.load(stream)['cases']


EXTRA_OPTIONS = {
    'help logs level': [('      --json', 'Print output as JSON', '--target')],
    'help plugin doctor': [('      --summary-only', 'Print the summary without individual findings', '--log-level'),
                           ('      --severity <severity>', 'Filter findings by error, warning, or info', '--log-level'),
                           ('      --code <code>', 'Filter findings by diagnostic code substring', '--log-level')],
    'help plugin list': [('      --compact', 'Print one concise line per plugin', '--target'),
                         ('      --enabled-only', 'List only enabled plugins', '--runtime'),
                         ('      --capability <capability>', 'Filter by required or provided capability', '--log-level')],
}


def insert_options(reference, options):
    lines = base64.b64decode(reference[1]).decode().splitlines(True)
    for spelling, summary, before in options:
        index, = [i for i, line in enumerate(lines) if line.lstrip().startswith(before + ' ')]
        lines.insert(index, f'{spelling:<36}{summary}\n')
    return record(0, ''.join(lines))


def native_help(case, mode):
    # Historical bare help is the typed core parser before migration. Default
    # sometimes printed the proxy's `[ARGS]...` instead of the callable syntax.
    bare, default = installed_reference('bare'), installed_reference()
    native = ('logs path', 'logs tail', 'logs watch', 'logs profiles delete',
              'logs profiles list', 'logs profiles rename', 'logs profiles show',
              'playbook cleanup', 'playbook diff', 'playbook dry-run',
              'playbook interactive', 'playbook run', 'playbook validate')
    if mode in ('default', 'local-splits') and case == 'help new-session':
        # Native create supports -n/--name as well as the positional NAME.
        return insert_options(installed_reference(mode)[case], [('  -n, --name <NAME>', '', '--target')])
    if mode != 'default':
        return None
    if case == 'help connect':
        # Combine historical positional/follow descriptions with the installed
        # pairing options. Neither interface alone documents both modes.
        core = base64.b64decode(bare[case][1]).decode().splitlines(True)
        plugin = base64.b64decode(default[case][1]).decode().splitlines(True)
        core[0] = plugin[0]
        pairing = [s for s in plugin if s.lstrip().startswith(('--pair ', '--name ', '--device-name '))]
        assert len(pairing) == 3
        i = next(i for i,s in enumerate(core) if s.lstrip().startswith('--log-level '))
        return record(0, ''.join(core[:i] + pairing + core[i:]))
    if case == 'help remote':
        # Canonical list and explicit targets alias reach the same config reader.
        text = base64.b64decode(default[case][1]).decode()
        return record(0, text.replace('  help            Print this message',
            '  targets         List configured connection targets\n  help            Print this message'))
    if case in EXTRA_OPTIONS:
        return insert_options(default[case], EXTRA_OPTIONS[case])
    if case in {'help ' + path for path in native} or case == 'playbook validate --help':
        return bare[case]
    if case in ('server recording --help', 'playbook from-recording --help', 'remote list --help'):
        return default['help ' + case.removesuffix(' --help')]
    if case in ('help logs', 'help logs profiles', 'help playbook'):
        text = base64.b64decode(default[case][1]).decode()
        for path in native:
            if path.rpartition(' ')[0] != case[5:]:
                continue
            old = default['help ' + path]
            old_summary, new_summary = (base64.b64decode(v[1]).decode().splitlines()[0]
                                        for v in (old, bare['help ' + path]))
            text = text.replace(old_summary + '\n', new_summary + '\n')
        return record(0, text)
    return None


def disposition(case, mode, old, actual):
    if old is None or actual is None:
        return None
    if (expected := native_help(case, mode)) is not None:
        return 'native command syntax' if actual == expected else 'violation'
    if mode == 'local-splits' and case in WORKSPACE_CASES:
        return ('installed tabs dependency' if actual == installed_reference()[case] else 'violation')
    absent = absent_paths(mode)
    path = case.removeprefix('help ')
    # Help probes are path-exact: an unrelated or invented child must never
    # become acceptable just because its parent disappeared.
    if case.startswith('help ') and path not in AUDIT:
        return None
    if any(path == p or path.startswith(p + ' ') for p in absent):
        # Uninstalled top-level commands enter external dispatch, whereas
        # `help ...` and missing children of retained namespaces fail in Clap.
        if case.startswith('help ') or path.split()[0] not in absent:
            if mode == 'disabled':
                # The bundled binary has Clap's rich diagnostics (compare the
                # frozen disabled `help workspace list` and `help server recording`).
                # Name the FIRST absent segment and its still-available parent;
                # a failure lower in the tree would mean a disabled branch leaked.
                words = path.split()
                stop = next(i for i in range(len(words)) if ' '.join(words[:i+1]) in absent)
                parent = ' '.join(words[:stop])
                usage = f'bmux {parent} [OPTIONS] <COMMAND>' if parent else 'bmux [OPTIONS] [COMMAND]'
                expected = record(2, stderr=f"error: unrecognized subcommand '{words[stop]}'\n\nUsage: {usage}\n\nFor more information, try '--help'.\n")
            else:
                expected = record(2, stderr='error: unrecognized subcommand\n')
        else:
            expected = record(1, stderr=f"bmux error: unknown command '{case}'; run 'bmux plugin list' to inspect available plugins\n")
        return 'owner absent' if actual == expected else 'violation'
    if case == 'logs profiles list --json' and mode == 'default':
        # The old proxy refused ambient execution. The installed owner must
        # return the local default profile on stdout, without an error.
        expected = json.dumps([{'active': True, 'filter_count': 0, 'name': 'default'}], indent=2) + '\n'
        return 'direct owner execution' if actual == record(0, expected) else 'violation'
    if absent and (case == '--help' or case.startswith('help ')) and old[0] == actual[0] == 0 and old[2] == actual[2]:
        parent = '' if case == '--help' else path
        a, b = [base64.b64decode(v[1]) for v in (old, actual)]
        children = {p for p in absent if p.rpartition(' ')[0] == parent}
        expected = projected_help(a, parent, children)
        if mode == 'local-splits' and case == '--help':
            # Bundled Workspaces contributes a root entry immediately before help.
            # Do not reorder the Sessions/Clients leaves around retained host leaves.
            expected = expected.replace(b'\n  help  Print this message',
                                        b'\n  workspace   \n  help  Print this message')
        if children and expected == projected_help(b, parent, set()):
            return 'namespace lists remaining children' if old != actual else None
    return None
