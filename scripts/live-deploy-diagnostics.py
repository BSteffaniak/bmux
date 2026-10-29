#!/usr/bin/env python3
"""Print bounded, redacted structured OpenTofu errors; never dump provider logs."""
import json
import os
from pathlib import Path
import re
import sys


# Fly embeds a JSON body inside a string-valued OpenTofu diagnostic. Never
# print that body: it may echo the submitted config (including env secrets).
# Translate recognized API error text into fixed, actionable local descriptions.
FLY_REASONS = (
    (r'(?:not authorized|unauthorized|authentication|access denied|permission denied)',
     'authorization: verify the deployment credential can update this app'),
    (r'(?:lease.*(?:held|expired|invalid|required|mismatch)|(?:held|expired|invalid|required).*lease)',
     'lease conflict: inspect machine ownership; do not force-release an active lease'),
    (r'(?:image.*(?:not found|not accessible|unauthorized|invalid|pull|manifest)|invalid image|(?:failed|unable|cannot).*pull)',
     'image rejected: verify the uploaded digest and Fly registry access'),
    (r'(?:volume.*(?:not found|attached|host|region|mount|in use)|mount.*(?:volume|invalid|duplicate))',
     'volume placement rejected: compare the existing mount, host and region; preserve the authority volume'),
    (r'(?:insufficient|not enough|no available|host.*full|capacity)',
     'capacity unavailable: inspect host capacity; do not replace the authority machine automatically'),
    (r'(?:region.*(?:invalid|unsupported|not found|mismatch)|cannot.*(?:region|migrat))',
     'region rejected: compare the requested region with the existing machine and volume'),
    (r'(?:guest|memory_mb|cpu_kind|cpus).*(?:invalid|must|unsupported|not allowed)',
     'guest sizing rejected: check the requested CPU and memory combination'),
    (r'(?:service|internal_port|port|autostop|auto_stop|autostart|auto_start).*(?:invalid|must|unsupported|not allowed)',
     'service configuration rejected: check ports and autostart/autostop settings'),
    (r'(?:restart).*(?:invalid|must|unsupported|not allowed)',
     'restart policy rejected: check the machine restart configuration'),
    (r'(?:machine.*(?:not found|destroyed|being replaced|not stopped)|instance.*(?:mismatch|changed|invalid))',
     'machine state conflict: inspect the existing machine and refresh the reviewed plan'),
)


def fly_error_context(detail):
    """Only known provider envelope + error fields, never config or arbitrary JSON."""
    match = re.search(r'fly machines: (\d{3}) [^\n]*?\(request_id: ([A-Za-z0-9_-]{1,100}), body: ', detail)
    if not match:
        return ''
    try:
        body, _ = json.JSONDecoder().raw_decode(detail[match.end():].lstrip())
    except (ValueError, RecursionError):
        return '\nFly API error body unavailable or malformed; raw body withheld'
    messages = []

    def collect(value, depth=0):
        if depth > 4:
            return
        if isinstance(value, str):
            messages.append(value[:4096])
        elif isinstance(value, list):
            for item in value[:16]:
                collect(item, depth + 1)
        elif isinstance(value, dict):
            for key in ('error', 'errors', 'message', 'detail'):
                if key in value:
                    collect(value[key], depth + 1)

    collect(body)
    reasons = [reason for pattern, reason in FLY_REASONS
               if any(re.search(pattern, message, re.IGNORECASE) for message in messages)]
    if not reasons:
        reasons = ['unclassified rejection: use the Fly request ID for support; raw body withheld']
    return '\nFly API classification (heuristic):\n' + '\n'.join(reasons[:3])


def diagnostics(text, environ):
    return diagnostic_lines(text.splitlines(), environ)


def diagnostic_lines(lines, environ):
    secrets = sorted({value for key, value in environ.items()
                      if len(value) >= 4 and any(part in key.upper() for part in
                          ('TOKEN', 'SECRET', 'PASSWORD', 'PASSPHRASE', 'KEY', 'TF_VAR_'))},
                     key=len, reverse=True)
    output = []
    for line in lines:
        if len(line) > 65536:
            continue
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
        context = fly_error_context(message)
        # Withhold the entire body, including malformed/unterminated strings and
        # trailing echoed configuration. Only the fixed classifier escapes it.
        message = re.sub(r'(fly machines:[^\n]*?body:).*', r'\1 [withheld]',
                         message, flags=re.DOTALL)
        if diagnostic.get('summary') == 'Missing required argument':
            # Schema identifiers, not supplied values: preserve this fixed error
            # shape while all other quoted provider text stays private.
            message = re.sub(r'The argument "([a-z][a-z0-9_]{0,63})" is required, but no definition was found\.',
                             r'The argument \1 is required, but no definition was found.', message)
        message = re.sub(r'https?://\S+', '[redacted URL]', message)
        message = re.sub(r'"(?:\\.|[^"\\\n])*"|\x1b\[[0-9;]*[A-Za-z]', '[redacted]', message)
        message = re.sub(r'(?i)(bearer\s+|(?:token|password|secret|key)\s*[=:]\s*)\S+',
                         r'\1[redacted]', message)
        # Prevent workflow-command/control-character injection from provider text.
        message = ''.join(c for c in message if c == '\n' or c.isprintable())
        message += context
        output.append('  ' + message[:3000].replace('\n', '\n  '))
        if len(output) >= 5:
            break
    return '\n'.join(output) or '  No structured diagnostic available; raw provider output remains private.'


def bounded_lines(source):
    # Stream the whole operation log: a useful failure can follow megabytes of
    # successful apply events. Discard oversized records without buffering them.
    while line := source.readline(65537):
        if len(line) > 65536:
            while not line.endswith('\n'):
                line = source.readline(65537)
                if not line:
                    break
            continue
        yield line


if __name__ == '__main__':
    with Path(sys.argv[1]).open(errors='replace') as source:
        print(diagnostic_lines(bounded_lines(source), os.environ), file=sys.stderr)
