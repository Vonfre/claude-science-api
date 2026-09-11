import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import {
  RUNTIME_STATUS_LABELS,
  runtimeStatusLabel,
  aggregateRuntimeStatus,
  normalizeRuntimeLight,
} from "../desktop/src/runtime-status-state.js";

test("Codex 无独立 upstream 时只聚合适用层", () => {
  assert.equal(aggregateRuntimeStatus({ proxy: "green", sandbox: "green", upstream: "gray" }), "green");
  assert.equal(aggregateRuntimeStatus({ proxy: "amber", sandbox: "amber", upstream: "gray" }), "amber");
});

test("官方模式不把打开请求当作健康证明", () => {
  assert.equal(aggregateRuntimeStatus({}, { mode: "official", officialState: "gray" }), "gray");
});

test("未知状态保持中性，明确失败才变红", () => {
  assert.equal(normalizeRuntimeLight("not-a-status"), "unknown");
  assert.equal(aggregateRuntimeStatus({ proxy: "green", sandbox: "green" }), "gray");
  assert.equal(aggregateRuntimeStatus({ proxy: "red", sandbox: "green", upstream: "gray" }), "red");
  assert.equal(RUNTIME_STATUS_LABELS.unknown, "状态未知");
});

test("运行反馈在独立区域内滚动且不强制跳转", () => {
  const css = readFileSync(new URL("../desktop/src/styles.css", import.meta.url), "utf8");
  const js = readFileSync(new URL("../desktop/src/main.js", import.meta.url), "utf8");
  const feedbackRule = css.match(/\.feedback\s*\{([^}]+)\}/)?.[1] || "";
  assert.match(feedbackRule, /max-height:\s*min\(380px, 50dvh\)/);
  const messageRule = css.match(/\.feedback \.msg\s*\{([^}]+)\}/)?.[1] || "";
  assert.match(messageRule, /max-height:\s*32vh/);
  assert.match(messageRule, /overflow:\s*auto/);
  const setMsg = js.slice(js.indexOf("function setMsg("), js.indexOf("function setBrowserFallback("));
  assert.doesNotMatch(setMsg, /scrollIntoView/);
});

test("运行时窗口重设与 Tauri 默认尺寸保持一致", () => {
  const js = readFileSync(new URL("../desktop/src/ipc-client.js", import.meta.url), "utf8");
  const tauri = JSON.parse(readFileSync(new URL("../desktop/src-tauri/tauri.conf.json", import.meta.url), "utf8"));
  const testTauri = JSON.parse(readFileSync(new URL("./tauri.real-machine.conf.json", import.meta.url), "utf8"));
  const mainWindow = tauri.app.windows.find((item) => item.label === "main");
  const testWindow = testTauri.app.windows.find((item) => item.label === "main");
  assert.deepEqual([mainWindow.width, mainWindow.height], [1180, 800]);
  assert.deepEqual([testWindow.width, testWindow.height], [1180, 800]);

  const configureWindow = js.slice(
    js.indexOf("export async function configureDesktopWindow()"),
  );
  assert.match(configureWindow, /setMinSize\(new LogicalSize\(820, 600\)\)/);
  assert.match(configureWindow, /setSize\(new LogicalSize\(1180, 800\)\)/);
  assert.doesNotMatch(configureWindow, /setSize\(new LogicalSize\(920, 600\)\)/);
});

test("上游网络可达不冒充 API 或 Science 运行成功", () => {
  assert.equal(runtimeStatusLabel("upstreamStateText", "green"), "网络可达 · API 未验证");
  assert.equal(runtimeStatusLabel("upstreamStateText", "amber"), "网络不可达");
  assert.equal(runtimeStatusLabel("upstreamStateText", undefined), "尚未确认");
  assert.equal(runtimeStatusLabel("proxyStateText", "amber"), "未启动或未就绪");
  assert.equal(runtimeStatusLabel("sandboxStateText", "green"), "健康检查通过");
  assert.equal(aggregateRuntimeStatus({proxy:"amber", sandbox:"amber", upstream:"green"}), "amber");
});
