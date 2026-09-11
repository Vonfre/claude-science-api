import test from 'node:test';
import assert from 'node:assert/strict';
import { createAppUpdateController } from '../desktop/src/app-update-controller.js';
import { updateManifest } from '../scripts/write-app-update-manifest.mjs';
const available = { status: 'available', current_version: '0.9.1', version: '0.9.2' };
const node = () => ({ textContent: '', hidden: false, disabled: false, addEventListener() {} });
function fixture({ answer = available, confirmed = true, failInstall = false, busy = false } = {}) {
  const calls = [], busyStates = [], schedules = [];
  const status = node(), checkButton = node(), installButton = node(), notice = node();
  const controller = createAppUpdateController({ status, checkButton, installButton, notice,
    isBusy: () => busy, setBusy: value => busyStates.push(value), confirm: async () => confirmed,
    showSettings() {}, schedule: (callback, delay) => { schedules.push({ callback, delay }); return 123; },
    call: async (...args) => {
      calls.push(args);
      if (args[0] === 'install_app_update') { if (failInstall) throw new Error('private detail'); return; }
      if (answer instanceof Error) throw answer;
      return answer;
    } });
  return { controller, calls, status, checkButton, installButton, notice, busyStates, schedules };
}
test('automatic checks are startup + six hourly; discovery never installs', async () => {
  const f = fixture();
  assert.equal(f.controller.start(), 123);
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(f.calls, [['check_app_update']]);
  assert.equal(f.schedules[0].delay, 6 * 60 * 60 * 1000);
  assert.equal(f.notice.hidden, false);
  assert.match(f.notice.textContent, /0.9.2/);
});
test('manual check presents available, current and unconfigured accurately', async () => {
  for (const [answer, text, hidden] of [[available, /0.9.2/, false],
    [{ status: 'current', current_version: '0.9.1' }, /最新稳定版/, true],
    [{ status: 'not_configured', current_version: '0.9.1' }, /未配置/, true]]) {
    const f = fixture({ answer }); await f.controller.check();
    assert.match(f.status.textContent, text);
    assert.equal(f.installButton.hidden, hidden);
    assert.equal(f.checkButton.disabled, false);
  }
});
test('consented install binds exact version and holds UI busy until completion', async () => {
  const f = fixture(); await f.controller.check(); await f.controller.install();
  assert.deepEqual(f.calls[1], ['install_app_update', { expectedVersion: '0.9.2' }]);
  assert.deepEqual(f.busyStates, [true, false]);
  assert.match(f.status.textContent, /正在重启/);
});
test('cancel, busy UI, and absent candidates never invoke installation', async () => {
  const f = fixture({ confirmed: false }); await f.controller.check(); await f.controller.install();
  assert.equal(f.calls.length, 1); assert.deepEqual(f.busyStates, [true, false]);
  const b = fixture({ busy: true }); await b.controller.check(); await b.controller.install();
  assert.deepEqual(b.calls, []);
  const empty = fixture(); await empty.controller.install(); assert.deepEqual(empty.calls, []);
});
test('network and install failures are sanitized, retryable, and release UI busy', async () => {
  const f = fixture({ answer: new Error('https://private.invalid/token') });
  await f.controller.check(); assert.match(f.status.textContent, /暂时无法/);
  assert.doesNotMatch(f.status.textContent, /private|token/);
  assert.equal(f.checkButton.disabled, false);
  const i = fixture({ failInstall: true }); await i.controller.check(); await i.controller.install();
  assert.match(i.status.textContent, /更新未完成/);
  assert.doesNotMatch(i.status.textContent, /private/);
  assert.equal(i.installButton.disabled, false);
  assert.deepEqual(i.busyStates, [true, false]);
});
test('overlapping checks are coalesced', async () => {
  const f = fixture(); await Promise.all([f.controller.check(), f.controller.check()]);
  assert.equal(f.calls.length, 1);
});
test('preview does not schedule or perform real update checks', () => {
  const f = fixture(); assert.equal(f.controller.start({ automatic: false }), null);
  assert.deepEqual(f.calls, []); assert.deepEqual(f.schedules, []);
});
test('manifest targets only fixed stable versioned signed Apple Silicon archive', () => {
  const m = updateManifest('0.9.2', 'c2lnbmF0dXJl\n', 'Release notes');
  assert.deepEqual(Object.keys(m.platforms), ['darwin-aarch64']);
  assert.equal(m.platforms['darwin-aarch64'].url, 'https://github.com/Vonfre/claude-science-api/releases/download/v0.9.2/SciPort_0.9.2_aarch64.app.tar.gz');
  for (const v of ['0.9.2-beta.1', '../bad', 'v0.9.2', '01.9.2']) assert.throws(() => updateManifest(v, 'YWJj'));
  for (const signature of ['', ' ', 'not a signature']) assert.throws(() => updateManifest('0.9.2', signature));
});
