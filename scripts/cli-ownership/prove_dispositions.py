"""Local execution checks for installed help dispositions, in compat's isolated environment."""
import argparse
from pathlib import Path

from compat import transcript
from dispositions import record


# These are independent expected results, not captured candidate output.
EXPECTED = {
    'remote list --json': record(0, '[]\n'),
    'remote targets --json': record(0, '[]\n'),
    'logs level --json': record(0, '{\n  "level": "error"\n}\n'),
    'plugin doctor --severity not-valid': record(1, stderr="--severity must be one of: error, warning, info (received 'not-valid')\n"),
    'plugin list --compact --enabled-only --capability no-such-capability': record(0, 'no plugins discovered\n'),
    'new-session -n x --name y': record(2, stderr="error: the argument '--name <NAME>' cannot be used multiple times\n\nUsage: bmux new-session [OPTIONS] [NAME]\n\nFor more information, try '--help'.\n"),
    'playbook dry-run missing-fixture': record(1, stderr='failed parsing playbook from missing-fixture: failed reading missing-fixture: No such file or directory (os error 2)\n'),
    'logs profiles show default': record(0, 'profile: default\nactive: no\nlines: (default)\nsince: (none)\nquick filter: (none)\nfilters:\n  (none)\n'),
}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('default_binary', type=Path)
    args = parser.parse_args()
    actual = transcript(args.default_binary.resolve(strict=True), 'default', map(str.split, EXPECTED))
    failures = [case for case, expected in EXPECTED.items() if actual[case] != expected]
    if failures:
        raise SystemExit('execution transcript mismatch: ' + ', '.join(failures))
    print(f'{len(EXPECTED)} execution transcripts OK')
