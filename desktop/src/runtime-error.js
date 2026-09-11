import { formatConfigMutationCommandError, parseConfigMutationCommandError } from "./runtime-mutation-protocol.js";

// Ordinary API mutations keep their structured recovery messages without loading account UI.
export function runtimeCommandErrorText(error) {
  try {
    const parsed = parseConfigMutationCommandError(error);
    if (parsed) return formatConfigMutationCommandError(parsed);
  } catch (protocolError) { return protocolError.message; }
  if (typeof error === "string") return error;
  if (typeof error?.message === "string") return error.message;
  return "后端返回了无法识别的错误，请运行只读自检。";
}
