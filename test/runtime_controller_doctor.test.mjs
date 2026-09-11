import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { runInNewContext } from "node:vm";

let invokeHandler = async () => {
  throw new Error("invoke handler not installed");
};

globalThis.window = {
  location: { search: "" },
  __TAURI__: {
    core: {
      invoke: async (command, args) => await invokeHandler(command, args),
    },
  },
};

const { createRuntimeController } = await import(
  "../desktop/src/runtime-controller.js?d0-doctor-intent-split"
);

function makeController(messages, busyStates = []) {
  return createRuntimeController({
    els: {},
    getConfigState: () => ({ active_id: "", profiles: [] }),
    getSkillPage: () => null,
    isBusy: () => false,
    isActivationInFlight: () => false,
    getMode: () => "proxy",
    getOfficialRuntimeState: () => "gray",
    setBusy: (on, operation) => busyStates.push([on, operation]),
    setMsg: (message, kind) => messages.push([message, kind]),
    setBrowserFallback() {},
    startOneClickFeedback() {},
    startDoctorFeedback() {},
    isCodexSource: () => false,
    renderList() {},
    runtimeCommandErrorText: (error) => String(error),
    syncOpenBrowserControl() {},
    setLight() {},
    setStatusText() {},
    setStatusRecoveryMsg() {},
    proxyRecoveryMessage: () => "",
  });
}

test("read-only Doctor intent invokes only run_doctor_read_only and renders typed status", async () => {
  const calls = [];
  const messages = [];
  const busyStates = [];
  invokeHandler = async (command, args) => {
    calls.push([command, args]);
    return {
      schema_version: 1,
      intent: "read_only_diagnostics",
      status: "passed",
      message: "typed pass without legacy report-text sentinel",
    };
  };

  await makeController(messages, busyStates).runDoctorReadOnly();

  assert.deepEqual(calls, [["run_doctor_read_only", undefined]]);
  assert.deepEqual(messages.at(-1), ["typed pass without legacy report-text sentinel", "ok"]);
  assert.deepEqual(busyStates, [
    [true, { kind: "doctorReadOnly" }],
    [false, undefined],
  ]);
});

test("explicit Skill route repair invokes only repair_skill_route and renders typed attention", async () => {
  const calls = [];
  const messages = [];
  invokeHandler = async (command, args) => {
    calls.push([command, args]);
    return {
      schema_version: 1,
      intent: "repair_skill_route",
      status: "warning",
      message: "route marker invalidated; host repair not completed",
    };
  };

  await makeController(messages).repairSkillRoute();

  assert.deepEqual(calls, [["repair_skill_route", undefined]]);
  assert.deepEqual(messages.at(-1), ["route marker invalidated; host repair not completed", "err"]);
});

test("API-only status page exposes read-only diagnostics, not skill mutations", async () => {
  const html = await readFile(new URL("../desktop/src/index.html", import.meta.url), "utf8");
  assert.match(html, /id="doctorBtn"[^>]*>运行只读自检</);
  assert.doesNotMatch(html, /id="repairSkillRouteBtn"/);
  assert.match(html, /只读/);
  assert.doesNotMatch(html, /id="codexLoginBtn"/);
});


async function feedbackFixture() {
  const source = await readFile(new URL("../desktop/src/main.js", import.meta.url), "utf8");
  const listeners = {};
  let modalOpen = false;
  const panel = { hidden: false, contains: () => false };
  const msg = { parentElement: panel, textContent: "长自检报告", className: "msg" };
  const els = { msg, browserFallback: { hidden: true } };
  const document = {
    activeElement: null,
    querySelector: () => modalOpen ? {} : null,
    addEventListener: (type, handler) => { listeners[type] = handler; },
  };
  const close = { addEventListener: (type, handler) => { listeners[type] = handler; } };
  const functions = source.slice(source.indexOf("function setMsg("), source.indexOf("function setBrowserFallback("));
  const api = runInNewContext(functions + "; wireFeedbackDismissal(); ({setMsg, dismissFeedback});", {
    els, document, $: () => close,
  });
  return { panel, msg, listeners, api, modal: (value) => { modalOpen = value; } };
}

test("feedback close hides long reports without erasing them; new results reopen", async () => {
  const {panel, msg, listeners, api} = await feedbackFixture();
  listeners.click();
  assert.equal(panel.hidden, true);
  assert.equal(msg.textContent, "长自检报告");
  api.setMsg("新的只读自检结果", "ok");
  assert.equal(panel.hidden, false);
  assert.equal(msg.textContent, "新的只读自检结果");
});

test("Escape dismisses feedback but belongs to an open confirmation dialog", async () => {
  const {panel, listeners, modal} = await feedbackFixture();
  let prevented = 0;
  const escape = {key: "Escape", preventDefault: () => { prevented++; }};
  modal(true);
  listeners.keydown(escape);
  assert.equal(panel.hidden, false);
  assert.equal(prevented, 0);
  modal(false);
  listeners.keydown({...escape, isComposing:true});
  assert.equal(panel.hidden, false);
  listeners.keydown(escape);
  assert.equal(panel.hidden, true);
  assert.equal(prevented, 1);
});

test("long-report close control stays separate from scrollable content and busy locks", async () => {
  const html = await readFile(new URL("../desktop/src/index.html", import.meta.url), "utf8");
  const css = await readFile(new URL("../desktop/src/styles.css", import.meta.url), "utf8");
  const main = await readFile(new URL("../desktop/src/main.js", import.meta.url), "utf8");
  assert.match(html, /id="feedbackCloseBtn"[^>]*aria-label="关闭操作反馈"/);
  assert.match(html, /id="feedbackCloseBtn"[^>]*>[\s\S]*?<span>关闭<\/span><kbd>Esc<\/kbd><\/button>/);
  assert.match(css, /\.feedback-close \{[^}]*min-height:\s*36px/);
  assert.ok(html.indexOf('id="feedbackCloseBtn"') < html.indexOf('id="msg"'));
  assert.match(css, /\.feedback \.msg \{[^}]*overflow:\s*auto/);
  assert.doesNotMatch(main.slice(main.indexOf("function setBusy("), main.indexOf("function syncActivationControls(")), /feedbackCloseBtn/);
});
