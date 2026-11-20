#!/usr/bin/env python3
"""Check retained leaf argument contracts against the pre-cutover git object."""
from pathlib import Path
import difflib
import re
import subprocess

ROOT = Path(__file__).resolve().parents[2]
PATH = 'packages/cli-schema/src/lib.rs'
BASE = '3320b28b58b6'

def contracts(src):
    items = {}
    for family in ('ConfigCommand', 'ConfigProfilesCommand', 'ConfigScopeCommand',
                   'ServerAutostartCommand', 'SlotCommand', 'SlotPrintFormat',
                   'SlotOutputFormat', 'SlotShellKind', 'SlotInstallMode',
                   'SandboxCommand', 'SandboxEnvModeArg', 'SandboxStatusArg', 'SandboxSourceArg'):
        items[family] = re.search(r'pub enum '+family+r' \{\n(.*?)^\}', src, re.S|re.M)[1]
    # server is a mixed namespace: snapshot and remote cuts are not regressions.
    server = re.search(r'pub enum ServerCommand \{\n(.*?)^\}', src, re.S|re.M)[1]
    for name in ('Start','Status','WhoamiPrincipal','Stop','Autostart'):
        items['server '+name] = re.search(r'^    '+name+r'(?: \{\n.*?^    \},|,)', server, re.S|re.M)[0]
    return items

if __name__ == '__main__':
    baseline = contracts(subprocess.check_output(['git','show',BASE+':'+PATH], cwd=ROOT,text=True))
    current = contracts((ROOT/PATH).read_text())
    for key in baseline:
        if baseline[key] != current[key]:
            raise SystemExit(''.join(difflib.unified_diff(baseline[key].splitlines(True),current[key].splitlines(True),BASE+' '+key,'working '+key)))
    print(f'{len(baseline)} retained schemas match {BASE}')
