import test from 'node:test';
import assert from 'node:assert/strict';
import { createDirectoryAccess } from '../desktop/src/directory-access.js';

const intent = { schema_version: 1, intent_id: 'a'.repeat(32), operation: 'set_settings',
  disposition: 'committed', config_state: 'committed', validation: 'not_run', science_running: false };
const completed = { schema_version: 1, operation_id: 'b'.repeat(32), operation: 'set_settings_destructive',
  disposition: 'completed', config_state: 'after', runtime_state: 'stopped', recovery_state: 'not_needed' };
function fixture(choice = 'host', response = intent) {
  const cfg = { proxy_port: 18991, sandbox_port: 8990, reuse_system_ssh: true, allow_science_host_home: false };
  const calls = []; let choices = 0, grants = 0;
  const ensure = createDirectoryAccess({ getConfigState: () => cfg,
    choose: async () => { choices++; return choice; },
    call: async (...args) => { calls.push(args); if (response instanceof Error) throw response; return response; },
    onGranted: () => { grants++; } });
  return { cfg, calls, ensure, counts: () => ({ choices, grants }) };
}
test('explicit host consent persists only runtime settings and only after an exact commit', async () => {
  for (const response of [intent, completed, { ...intent, disposition: 'no_change' }]) {
    const f = fixture('host', response);
    assert.equal(await f.ensure(), true);
    assert.deepEqual(f.calls, [['set_settings', { cfg: { proxy_port: 18991, sandbox_port: 8990,
      reuse_system_ssh: true, allow_science_host_home: true } }]]);
    assert.equal(f.cfg.allow_science_host_home, true);
    assert.equal(await f.ensure(), true);
    assert.deepEqual(f.counts(), { choices: 1, grants: 1 });
  }
});
test('cancel and Escape do not grant, save, or proceed', async () => {
  for (const choice of ['cancel', '', undefined]) {
    const f = fixture(choice === undefined ? '' : choice);
    assert.equal(await f.ensure(), false);
    assert.equal(f.cfg.allow_science_host_home, false);
    assert.deepEqual(f.calls, []);
    assert.equal(f.counts().grants, 0);
  }
});
test('isolated choice is session-only and never writes consent', async () => {
  const f = fixture('isolated');
  assert.equal(await f.ensure(), true);
  assert.equal(await f.ensure(), true);
  assert.equal(f.cfg.allow_science_host_home, false);
  assert.deepEqual(f.calls, []);
  assert.equal(f.counts().choices, 1);
  await f.ensure({ force: true });
  assert.equal(f.counts().choices, 2);
});
test('invalid, failed, or partial save never publishes host consent', async () => {
  for (const response of [null, {}, new Error('failed'), { ...intent, science_running: true },
    { ...intent, operation: 'set_active_profile' }, { ...completed, runtime_state: 'unknown' },
    { ...completed, config_state: 'before' }, { ...completed, recovery_state: 'required' }]) {
    const f = fixture('host', response);
    await assert.rejects(f.ensure());
    assert.equal(f.cfg.allow_science_host_home, false);
    assert.equal(f.counts().grants, 0);
  }
});
test('saved explicit consent skips dialog; revocation asks again', async () => {
  const f = fixture();
  f.cfg.allow_science_host_home = true;
  assert.equal(await f.ensure(), true);
  assert.equal(f.counts().choices, 0);
  f.cfg.allow_science_host_home = false;
  await f.ensure();
  assert.equal(f.counts().choices, 1);
});

test('preview settings receipt supports the same strict directory consent protocol', async () => {
  const previous = globalThis.window;
  globalThis.window = { location: { search: '?profile_preview=1' } };
  try {
    const { mockInvoke } = await import('../desktop/src/preview-adapter.js');
    const cfg = await mockInvoke('get_config');
    cfg.allow_science_host_home = false;
    let granted = false;
    const ensure = createDirectoryAccess({ call: mockInvoke, getConfigState: () => cfg,
      choose: async () => 'host', onGranted: () => { granted = true; } });
    assert.equal(await ensure(), true);
    assert.equal(granted, true);
    assert.equal((await mockInvoke('get_config')).allow_science_host_home, true);
  } finally {
    if (previous === undefined) delete globalThis.window;
    else globalThis.window = previous;
  }
});
