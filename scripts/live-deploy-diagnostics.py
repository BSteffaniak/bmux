#!/usr/bin/env python3
"""Print bounded, redacted structured OpenTofu errors; never dump provider logs."""
import json
import os
from pathlib import Path
import re
import sys


def diagnostics(text, environ):
    secrets = sorted({value for key, value in environ.items()
                      if len(value) >= 4 and any(part in key.upper() for part in
                          ('TOKEN', 'SECRET', 'PASSWORD', 'PASSPHRASE', 'KEY', 'TF_VAR_'))},
                     key=len, reverse=True)
    output = []
    for line in text.splitlines():
        try:
            record = json.loads(line)
        except (ValueError, RecursionError):
            continue
        if not isinstance(record, dict):
            continue
        diagnostic = record.get('diagnostic')
        if not isinstance(diagnostic, dict) or diagnostic.get('severity') != 'error':
            continue
        fields = [diagnostic.get('summary', ''), diagnostic.get('detail', '')]
        message = '\n'.join(value for value in fields if isinstance(value, str))
        for secret in secrets:
            message = message.replace(secret, '[redacted]')
        message = re.sub(r'https?://\S+', '[redacted URL]', message)
        message = re.sub(r'"[^"\n]*"|\x1b\[[0-9;]*[A-Za-z]', '[redacted]', message)
        message = re.sub(r'(?i)(bearer\s+|(?:token|password|secret|key)\s*[=:]\s*)\S+',
                         r'\1[redacted]', message)
        # Prevent workflow-command/control-character injection from provider text.
        message = ''.join(c for c in message if c == '\n' or c.isprintable())
        output.append('  ' + message[:3000].replace('\n', '\n  '))
        if len(output) >= 5:
            break
    return '\n'.join(output) or '  No structured diagnostic available; raw provider output remains private.'


if __name__ == '__main__':
    with Path(sys.argv[1]).open(errors='replace') as source:
        text = source.read(1048576)
    print(diagnostics(text, os.environ), file=sys.stderr)
