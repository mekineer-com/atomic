import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const workflow = readFileSync(new URL('../.github/workflows/windows-server.yml', import.meta.url), 'utf8');
const pattern = workflow.match(/SOURCE_REF -cnotmatch '([^']+)'/)[1];
const reference = new RegExp(pattern);
for (const value of ['v0.1.12', 'v12.34.56', 'c688c848ab83f6a1fb9c051887c6364765b8ed03']) {
  assert(reference.test(value), value);
}
for (const value of ['buildfix', 'HEAD', 'c688c848', 'v0.1.12-dev', 'v0.1.12; whoami', 'A'.repeat(40), '']) {
  assert(!reference.test(value), value);
}
assert(workflow.includes("$sha -cne $env:SOURCE_REF"));
assert(workflow.includes("$env:SOURCE_REF -eq 'v0.1.12'"));
console.log('Windows source reference checks passed');
