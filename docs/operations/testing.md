# 自动测试与证据判定

## 固定 Python 依赖准备（macOS Apple Silicon）

源码门禁接受 Apple Xcode 或 Command Line Tools 的 Python 3.9；启动器和进程映像
必须属于同一精确工具链配对，仍校验打开的文件描述符、内容摘要与执行身份，不接受
任意 PATH 中的 Python。构建工具同样不从 ambient PATH 查找：兼容原有 Node 安装，
并接受 Homebrew Node 24.10.0 的精确 executable；Rust 接受同目录的 stable 或 1.95
arm64 Cargo / rustc 配对。实际选中的工具仍逐文件绑定摘要和身份。依赖使用独立、
版本化缓存，不修改日常 Python 环境。

首次准备（先在临时目录下载锁定 wheel，再用标准库验证并展开）：

```bash
WHEELS="$(mktemp -d /private/tmp/csw.XXXXXX)"
/usr/bin/python3 -m pip --isolated download --only-binary=:all: --no-deps \
  --require-hashes -r quality/source-gate-python.lock --dest "$WHEELS"
/usr/bin/python3 -I scripts/prepare-source-gate-python.py --wheel-dir "$WHEELS"
```

缓存位于当前系统账户的 `Library/Caches/SciPort/source-gate/python3.9-wheels-v1/site-packages`。
安装器验证每个锁定 wheel 的 SHA-256，拒绝路径逃逸、符号链接、重复文件和覆盖已有
缓存。门禁另行固定原始 wheel RECORD 摘要，并逐文件核对内容、大小、所有者与模块
来源；没有放宽为“能 import 就通过”。已有缓存无需重复安装；依赖变更必须更新锁、
RECORD 固定摘要、缓存版本与测试，不能原地替换未复核依赖。Rust 使用已缓存的官方
crates.io 依赖，保持离线与 lockfile / 依赖内容清单校验，不读取用户 Cargo 配置，
也不强制特定镜像。此锁的 native wheel
只适用于 CPython 3.9 / macOS arm64。

## 权威 source gate

当前唯一完整 source/unit 入口是固定的 `GATE-SOURCE`：

```bash
GATE_ROOT="$(mktemp -d /private/tmp/csg.XXXXXX)"
chmod 700 "$GATE_ROOT"
bash test/run_all.sh --output-root "$GATE_ROOT"
```

`GATE_ROOT` 必须是绝对、canonical、当前用户拥有、空的 `0700` 目录，且不能位于
仓库内。`test/run_all.sh` 只是兼容命令名，只接受上述 `--output-root` 参数并转发到
隔离 Python CLI；无参数调用和旧 `--require-release-ready` 均返回 usage error。
macOS 上还必须给 suite 创建的 Unix socket 保留路径预算，因此固定使用
`/private/tmp/csg.XXXXXX` 这类短根目录。CLI 的 stderr 只输出固定、脱敏的
`source-gate` 错误码；例如 `output-root-path-too-long` 表示应换用短根目录，
不代表测试已经执行。

Gate 只接受 clean、non-shallow 的 exact `HEAD`，固定顺序执行
`quality/release-gates.v1.json` 中 `GATE-SOURCE.required_suite_ids` 的 16 个 suite。
命令、测试 identity、允许环境、timeout、无 retry 与聚合证据均由
`quality/test-catalog.v1.json` 和 trusted run-evidence 合同绑定。公共 CLI 不能选择
子集。

完整 gate 会串行执行 Rust 编译、Python、前端和质量套件，通常需要数分钟，并且在
最终 completion seal 前没有逐 suite 的公共进度输出。包含 loopback listener 和
子进程生命周期测试；如果托管执行沙箱禁止本地 bind 或进程观察，应在明确允许这些
操作的隔离执行环境中原命令重跑，并把首次结果记为 `ENV-BLOCKED`，不得改测试、
跳 suite 或把环境失败记作产品失败。

质量 metadata 的动态测试入口发现只枚举 Git 已跟踪文件和未被 ignore 的未跟踪
文件；已跟踪文件即使后来匹配 ignore 仍会被检查。这样会继续 fail-closed 捕获新增
源码测试，同时不会递归读取 `.sandbox/`、构建缓存等明确忽略的本地运行时数据。
正式 metadata、impact 和 source gate 仍应在 clean exact-HEAD worktree 执行；
不得为了门禁删除用户的 ignored runtime 数据。

报告至少记录命令、退出码、exact `HEAD`、输出目录、最终 completion seal / aggregate
判定与 16 个 suite 状态。只有递归验证后的 PASS seal 才建立
`RUN-EVIDENCE-GREEN` 与 `SOURCE-GREEN`；stdout 摘要或某个组件通过都不是权威。
固定 suite / entrypoint identity、允许环境、timeout、retry、result 与 seal 的机器合同
由 `quality/test-catalog.v1.json`、`quality/release-gates.v1.json` 及其 schema 维护；
本文是当前人工执行与判定入口。

## 聚焦诊断

`test/run-offline.sh`、`test/run-loopback.sh`、`test/run-scripts.sh`、
`test/run-rust.sh` 和 `test/run-frontend.sh` 仍可用于定位相应组件问题，但它们不是
当前完整 gate，也不能单独建立 `SOURCE-GREEN`。旧 `S0_LAYER`、
`current-env clean` 与 `release-ready green` 只用于解释历史 evidence，不是当前
候选的结果词汇。

文档治理的定向入口是：

```bash
python3 -m unittest test.test_document_governance -v
```

它登记在 `quality/test-catalog.v1.json` 的既有 `SUITE-PY-OFFLINE`，由完整
`GATE-SOURCE` 执行；覆盖范围与不能外推的证据层以
[文档治理合同](document-lifecycle.md)为准。

## 自动化没有证明的层

`GATE-SOURCE` PASS 仍不自动证明：

- `.app` / DMG 从目标 commit 构建且内容正确；
- 临时安装副本或 installed runtime 可用；
- 当前 Claude Science 版本兼容；
- 外部 Skill 的自然语言路由、领域功能或重启持久化；
- 特定真实 provider / SSH server 可用；
- Developer ID 签名、notarization、Gatekeeper 或公开 release 附件一致。

这些层分别使用[真机验收](real-machine-acceptance.md)、[发布流程](release.md)和 dated evidence。

## 报告词汇

- `PASS`：目标 gate / suite 已执行、身份绑定且满足判据；
- `失败`：已执行但不满足；
- `PREFLIGHT / ENV-BLOCKED`：当前环境或候选前置不满足，不能视为通过；
- `NEEDS-REAL-MACHINE`：必须在指定真机 / artifact 上执行；
- `未执行`：没有取得该层证据；
- `需人工判断`：机器结果不足以自动确定。

mock / loopback、built artifact、installed copy、runtime、live provider 与发布附件必须分栏记录。
