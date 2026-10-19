#!/usr/bin/env node
// Never print provider prose, source snippets, variable values, or raw logs.
import { readFileSync } from 'node:fs';
const categories = [
  [/cannot be destroyed|prevent_destroy|prevent.destroy/i, 'Destructive replacement blocked by prevent_destroy; preserve state and inspect provider import'],
  [/resource not accessible|403|forbidden|permission/i, 'GitHub/provider permission denied'],
  [/401|unauthorized|bad credentials/i, 'Provider authentication rejected'],
  [/cannot import non-existent|404|not found/i, 'Import target or provider endpoint not found'],
  [/acquiring the state lock|lock info|conditionalrequestconflict/i, 'Canonical state lock unavailable; do not force-unlock'],
  [/invalid index/i, 'Terraform expression uses an invalid index'],
  [/invalid.*(id|import)|unexpected.*(id|import)/i, 'Provider rejected import identity'],
  [/unsupported argument|unsupported attribute|invalid.*argument/i, 'Terraform/provider schema mismatch'],
  [/encryption|decrypt|passphrase/i, 'State or plan encryption configuration failed'],
  [/checksum|inconsistent.*lock|dependency lock/i, 'Provider lockfile or checksum mismatch'],
  [/timeout|timed out|connection|503|502|429|rate limit/i, 'Provider connectivity or rate-limit failure'],
];
let errors = 0;
for (const line of readFileSync(process.argv[2], 'utf8').split('\n')) {
  let event;
  try { event = JSON.parse(line); } catch { continue; }
  const diagnostic = event.diagnostic;
  if (diagnostic?.severity !== 'error') continue;
  errors++;
  const text = `${diagnostic.summary ?? ''}\n${diagnostic.detail ?? ''}`;
  console.error(categories.find(([pattern]) => pattern.test(text))?.[1] ?? 'Unclassified Terraform diagnostic; private log retained only on runner');
  const range = diagnostic.range;
  if (range && /^[a-zA-Z0-9_./-]+\.tf(?:\.json)?$/.test(range.filename) && Number.isInteger(range.start?.line))
    console.error(`Source: ${range.filename}:${range.start.line}`);
}
console.error(`Terraform error diagnostics: ${errors}. No apply attempted; do not reset canonical state.`);
