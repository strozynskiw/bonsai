import assert from 'node:assert/strict';

// Explicit selection is frozen at preflight, never inferred from missing runs.
export function selectedHarnesses(value = 'bonsai,dsh') {
  const harnesses = value.split(',');
  assert.ok(harnesses.length > 0 && new Set(harnesses).size === harnesses.length);
  assert.ok(harnesses.every(harness => ['bonsai', 'dsh'].includes(harness)), 'unknown harness');
  return harnesses;
}
