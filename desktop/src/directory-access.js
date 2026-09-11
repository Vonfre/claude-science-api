import { parseConfigMutationResponse } from "./runtime-mutation-protocol.js";

// A session-only isolated choice is not host-home consent. Only a successful
// typed settings commit can grant the latter; Escape/cancel never launches.
export function createDirectoryAccess({ call, getConfigState, choose, onGranted }) {
  let isolatedThisSession = false;
  return async function ensureDirectoryAccess({ force = false } = {}) {
    const cfg = getConfigState();
    if (!force && (cfg.allow_science_host_home === true || isolatedThisSession)) return true;
    const choice = await choose();
    if (choice === "isolated") {
      isolatedThisSession = true;
      return true;
    }
    if (choice !== "host") return false;
    const result = parseConfigMutationResponse(await call("set_settings", { cfg: {
      proxy_port: cfg.proxy_port, sandbox_port: cfg.sandbox_port,
      reuse_system_ssh: cfg.reuse_system_ssh === true, allow_science_host_home: true,
    } }));
    const accepted = (result?.operation === "set_settings"
      && ["committed", "no_change"].includes(result.disposition)
      && result.config_state === "committed" && typeof result.intent_id === "string"
      && result.validation === "not_run" && result.science_running === false)
      || (result?.operation === "set_settings_destructive" && result.disposition === "completed"
        && result.config_state === "after" && ["stopped", "preserved"].includes(result.runtime_state)
        && result.recovery_state === "not_needed" && typeof result.operation_id === "string");
    if (!accepted) throw new Error("目录授权保存结果不完整；未继续启动。");
    cfg.allow_science_host_home = true;
    isolatedThisSession = false;
    onGranted();
    return true;
  };
}
