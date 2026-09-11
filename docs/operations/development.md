# 开发与维护

仓库 `.cargo/config.toml` 关闭 dev/test 调试符号与增量缓存，以控制隔离多 crate 验证的磁盘占用；保留默认断言、溢出检查和 release 优化设置。需要调试符号时，可在个人开发调用中用 Cargo 环境变量覆盖，不将该覆盖当作正式门禁证据。

本文说明当前源码树的开发入口。安全、Git / worktree 和证据措辞分别以 [`.agents/rules/`](../../.agents/rules/) 为准。

## 环境

- macOS Apple Silicon（当前桌面发布目标）；
- Node.js / npm（Tauri 前端与构建）；
- Rust / Cargo（desktop backend 与 Rust gateway）；
- Python 3（测试驱动与 mock 使用，**不是** CSSwitch runtime proxy 依赖）；
- Claude Science App（只在隔离 runtime / 真机验收时需要）。

## 本地启动

```bash
cd desktop
npm install
npm run tauri dev
```

## 快速 WIP、候选与 source closure

先按[代码作者规则](../../.agents/rules/code-authoring.md)确认 owner、边界和最高风险：`F3 > F2 > F1 > F0`。同一有界 WIP 先完成，再运行完整 diff 和有明确信号的 owning check；`F0` / `F1` 默认不启动正式独立审查或完整 gate。稳定的 `F2` / `F3` candidate 先冻结边界，再按[独立审查规则](../../.agents/rules/reviewing.md)集中审查；文件、进程或 network 副作用的目标、授权、时序、提交点、timeout、cleanup、补偿或失败报告变化一律按 `F3` 处理。

candidate 不自动等于 source closure。只有明确需要声明 exact candidate 的 source closure、修改 quality kernel / gate authority，或影响无法由聚焦检查可信界定时，才在 clean exact-`HEAD` 候选上运行唯一完整 gate：

```bash
GATE_ROOT="$(mktemp -d /private/tmp/csg.XXXXXX)"
chmod 700 "$GATE_ROOT"
bash test/run_all.sh --output-root "$GATE_ROOT"
```

固定 16-suite 选择、输出目录约束和判定边界见[测试文档](testing.md)。无参数调用和
旧 `--require-release-ready` 已不再是有效入口。任何 WIP 或 focused check 的 PASS 都不建立
`SOURCE-GREEN`；没有运行完整 gate 时报告 `SOURCE-GREEN: NOT-RUN`。

## 组件级检查

五个 Rust manifest 都是独立入口；从仓库根选择目标，不把单个 crate 结果外推到其他 crate：

| 组件 | manifest | 聚焦命令形态 |
| --- | --- | --- |
| Skill 安装核心 | `desktop/skill-package/Cargo.toml` | `cargo <fmt / clippy / test> --manifest-path desktop/skill-package/Cargo.toml` |
| Codex network 库 | `desktop/codex-network/Cargo.toml` | `cargo <fmt / clippy / test> --manifest-path desktop/codex-network/Cargo.toml` |
| 共享 provider contract | `desktop/provider-contracts/Cargo.toml` | `cargo <fmt / clippy / test> --manifest-path desktop/provider-contracts/Cargo.toml` |
| Gateway | `desktop/gateway/Cargo.toml` | `cargo <fmt / clippy / test> --manifest-path desktop/gateway/Cargo.toml` |
| Tauri desktop | `desktop/src-tauri/Cargo.toml` | `cargo <fmt / clippy / test> --manifest-path desktop/src-tauri/Cargo.toml` |

`fmt` 使用 `cargo fmt --check --manifest-path <manifest>`；`clippy` 使用 `cargo clippy --manifest-path <manifest> --all-targets -- -D warnings`；`test` 使用 `cargo test --manifest-path <manifest>`。Python 与前端的常用聚焦入口仍为：

```bash
python3 -m unittest discover -s test -p 'test_*.py' -v
node --check desktop/src/main.js
```

组件命令和单个 `test/run-*.sh` 只适合聚焦诊断；当前完整门禁是上述
`GATE-SOURCE`，不能由组件结果拼成 `SOURCE-GREEN`。

## 远端协作与 CI 当前状态

当前仓库提供 [macOS Release 构建](../../.github/workflows/release.yml)：推送版本 tag 后构建 Apple Silicon 安装包并发布到目标 GitHub 仓库。该流程只做构建及包完整性检查，不运行测试套件，也不建立 `SOURCE-GREEN`。没有已配置的 required check。PR 模板用于复核本地
exact base / head、风险、变更面和实际执行的分层检查；它不把本地结果变成远端执行证据。

当前决定暂不采用 CI：尚无 macOS runner 耗时或稳定性的实际证据，也未获得启用 required
check 的授权。在 workflow 和远端执行证据实际存在前，不能写成由 CI 兜底；source closure
继续只在明确 closure 目标时使用现有本地 exact-candidate gate。

## Science 相邻功能工作法

1. 在隔离环境确认上游 runtime 事实；
2. 明确 source of truth 与所有权；
3. 先验证不增加存储 / 状态机的最短路径；
4. 跑一条完整 E2E，并分别记录 copy、discover、attach、load、trigger、功能执行与重启；
5. 最后再决定是否需要 UI、catalog、cache 或新存储。

Science 已拥有的能力不应在 CSSwitch 再造一套 installer、目录所有权或生命周期。

## 隔离 runtime 开发

- 使用临时外层 `HOME`、临时持久 data-dir、动态端口和假 `security`；
- 不使用真实 `~/.claude-science`、端口 `8765` 或 `/Applications/CSSwitch.app`；
- installed-App candidate、缓存 candidate、实际 PID、版本 runtime 与 data-dir 分别取证；
- live provider、真实账号或真实 SSH server 测试需要额外授权。

详细步骤见[真机验收](real-machine-acceptance.md)。

## 文档维护

文档类型、权威位置、默认阅读预算，以及临时 Plan / Draft Spec / Handoff 的晋升、过期与删除，统一见[文档治理合同](document-lifecycle.md)。

发布或重要 upstream runtime 变化后，应复核 architecture、功能限制、known issues 和 release evidence，而不是把新事实只留在聊天或 handoff。
