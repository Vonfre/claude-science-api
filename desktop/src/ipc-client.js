import { PREVIEW, mockInvoke } from "./preview-adapter.js";

// Tauri command 顶层参数保持 lowerCamelCase；serde payload 内部字段保持 snake_case。
// 本模块只统一 transport / event / window adapter，不解释业务 DTO。
const invoke = PREVIEW
  ? (command, args) => mockInvoke(command, args)
  : window.__TAURI__.core.invoke;

export async function call(command, args) {
  return await invoke(command, args);
}

export async function listen(eventName, handler) {
  if (PREVIEW || !window.__TAURI__.event) return null;
  return await window.__TAURI__.event.listen(eventName, handler);
}

export async function configureDesktopWindow() {
  if (PREVIEW) return;
  try {
    const appWindow = window.__TAURI__.window.getCurrentWindow();
    const LogicalSize = window.__TAURI__.dpi.LogicalSize;
    await appWindow.setMinSize(new LogicalSize(820, 600));
    await appWindow.setSize(new LogicalSize(1180, 800));
  } catch (_) {
    // 某些受限 WebView 权限下不能改窗口尺寸；界面本身仍保持响应式。
  }
}
