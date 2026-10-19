import assert from 'node:assert/strict';
import test from 'node:test';
import {selectedHarnesses} from './harnesses.mjs';

test('selection defaults to both and permits explicit single harness', () => {
  assert.deepEqual(selectedHarnesses(), ['bonsai', 'dsh']);
  assert.deepEqual(selectedHarnesses('bonsai'), ['bonsai']);
  assert.deepEqual(selectedHarnesses('dsh'), ['dsh']);
});
test('selection rejects empty, duplicate and unknown harnesses', () => {
  for (const value of ['', 'bonsai,bonsai', 'other', 'bonsai,']) {
    assert.throws(() => selectedHarnesses(value));
  }
});
