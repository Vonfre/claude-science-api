import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const read = name => readFileSync(new URL(`../${name}`, import.meta.url), 'utf8');
const html = read('desktop/src/index.html');
const main = read('desktop/src/main.js');
const profile = read('desktop/src/profile-controller.js');
const lib = read('desktop/src-tauri/src/lib.rs');

test('SciPort presents two destinations and preserves the working interaction IDs', () => {
  assert.match(html, /<title>研舟 · SciPort<\/title>/);
  assert.deepEqual([...html.matchAll(/data-page-target="([^"]+)"/g)].map(m => m[1]), ['switch', 'settings']);
  const ids = [...html.matchAll(/\bid="([^"]+)"/g)].map(m => m[1]);
  assert.equal(new Set(ids).size, ids.length, 'IDs must remain unique');
  for (const id of ['oneClickBtn', 'stopBtn', 'doctorBtn', 'logsBtn', 'openBrowserBtn', 'profileList',
    'newBtn', 'profileSearch', 'batchDialog', 'batchDeletePhrase', 'proxyPort', 'sandboxPort',
    'reuseSystemSsh', 'saveSettingsBtn', 'runtimeChoiceSec', 'historyRecoverySec', 'feedbackCloseBtn',
    'wizSaveBtn', 'connSaveBtn', 'metaSaveBtn', 'themeLabel']) assert.ok(ids.includes(id), id);
  assert.match(html, /<input id="wizKey" type="password"/);
  assert.match(html, /<input id="connKey" type="password"/);
});

test('localization is removed end-to-end without removing the main-window IPC guard', () => {
  for (const source of [html, main, lib, read('desktop/src-tauri/src/main.rs'), read('desktop/src/preview-adapter.js')]) {
    assert.doesNotMatch(source, /science_language|science-language|science_ui|scienceChineseToggle/);
  }
  for (const name of ['desktop/src/science-localization', 'desktop/src/science-language-controller.js',
    'desktop/src-tauri/src/science_ui.rs', 'desktop/src-tauri/src/science_language_bridge.rs']) {
    assert.equal(existsSync(new URL(`../${name}`, import.meta.url)), false, name);
  }
  assert.match(lib, /\.invoke_handler\(main_window_commands\(tauri::generate_handler!\[/);
  assert.match(lib, /allows_app_commands\(invoke.message.webview_ref\(\).label\(\)\)/);
  assert.match(lib, /label == "main"/);
});

test('official connectivity notice is an explicit capability boundary, not live session detection', () => {
  assert.match(html, /模型连接 ≠ 官方账号登录/);
  assert.match(html, /能力说明 · 非登录检测/);
  assert.match(html, /不提供 claude.ai 登录/);
  assert.doesNotMatch(html, /官方账号已登录|连接器已恢复/);
});

function renderSummary(state) {
  const nodes = Object.fromEntries(['launchProfileName', 'launchModelName', 'launchSelectionState', 'profileSelectionHint']
    .map(id => [id, { textContent: '', title: '' }]));
  const body = profile.split('function renderCurrentSummary() {')[1].split('\n}\n')[0];
  vm.runInNewContext(`function renderCurrentSummary() {${body}\n}\nrenderCurrentSummary();`, {
    getConfigState: () => state, document: { getElementById: id => nodes[id] },
  });
  return nodes;
}

test('launch selection is plain text and never claims a running service', () => {
  const selected = { id: 'a', name: '<img src=x onerror=alert(1)>', model: 'example/model' };
  const nodes = renderSummary({ profiles: [selected], active_id: 'a', applied_profile_id: 'a' });
  assert.equal(nodes.launchProfileName.textContent, selected.name);
  assert.equal(nodes.launchModelName.textContent, 'example/model');
  assert.match(nodes.launchSelectionState.textContent, /不代表服务运行中/);
  assert.equal(nodes.launchProfileName.title, selected.name);
  const pending = renderSummary({ profiles: [selected], active_id: 'a', applied_profile_id: 'b' });
  assert.match(pending.launchSelectionState.textContent, /待应用/);
  const empty = renderSummary({ profiles: [], active_id: 'missing' });
  assert.equal(empty.launchProfileName.textContent, '尚未选择 API');
  assert.equal(empty.launchSelectionState.textContent, '选择后，点击启动');
});

test('theme updates accessible labels without deleting the theme icon', () => {
  const attrs = {}, label = {}, themeBtn = { setAttribute: (key, value) => { attrs[key] = value; } };
  let persisted;
  const context = { els: { themeBtn }, $: () => label,
    document: { documentElement: { dataset: {} } }, THEME_STORAGE_KEY: 'csswitch-theme',
    window: { localStorage: { setItem: (key, value) => { persisted = [key, value]; } } } };
  const body = main.split('function applyTheme(theme, { persist = true } = {}) {')[1].split('\n}\n')[0];
  vm.createContext(context);
  vm.runInContext(`function applyTheme(theme, { persist = true } = {}) {${body}\n}\napplyTheme('dark');`, context);
  assert.equal(context.document.documentElement.dataset.theme, 'dark');
  assert.equal(label.textContent, '浅色');
  assert.equal(attrs['aria-label'], '切换浅色主题');
  assert.deepEqual(persisted, ['csswitch-theme', 'dark']);
  assert.equal(Object.hasOwn(themeBtn, 'textContent'), false);
  vm.runInContext(`applyTheme('light', {persist: false})`, context);
  assert.equal(label.textContent, '深色');
  assert.equal(attrs['aria-label'], '切换深色主题');
});

test('visual redesign retains local storage identity, reduced motion and narrow table scrolling', () => {
  const css = read('desktop/src/styles.css');
  assert.match(css, /prefers-reduced-motion: reduce/);
  assert.match(css, /\.profile-table-wrap\s*\{\s*overflow-x: auto/);
  assert.match(css, /:focus-visible/);
  assert.match(css, /\[data-theme="dark"\]/);
  assert.doesNotMatch(css, /@import|https?:\/\//);
  const config = JSON.parse(read('desktop/src-tauri/tauri.conf.json'));
  assert.equal(config.app.windows[0].title, '研舟 · SciPort');
  assert.equal(config.productName, 'SciPort');
  assert.equal(config.identifier, 'com.csswitch.menubar');
  assert.match(main, /const THEME_STORAGE_KEY = "csswitch-theme"/);
});
