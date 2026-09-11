# 发布流程

发布是逐层建立证据，不是一次 `build` 或一次 `gh release`。Agent 的授权禁止项见[发布规则](../../.agents/rules/release.md)。

## SciPort v0.9.1 打包修复

v0.9.1 在编译前新增前端与 Release 说明渲染回归检查，使用 `${RELEASE_TAG}` 明确中文标点前的变量边界。该聚焦检查不替代完整源码门禁，也不宣称真实 Science 或 provider 已验证。以下 v0.9.0 例外仅记录历史。

## SciPort v0.9.0 的维护者指定发布方式

2026-09-11，维护者明确要求停止本地测试，先推送源码，由 GitHub 构建并发布安装包，再自行实际测试。本次是对下述标准源码门禁流程的一次明确例外，不改变后续版本的默认门禁，也不将之前失败的检查记为通过。README、Release 说明和 BUILD_INFO 必须保留未完成验证的披露。

[GitHub 构建流程](../../.github/workflows/release.yml) 在版本 tag 上从同一 checkout 构建 desktop 与 Gateway，调用[打包脚本](../../scripts/package-sciport-release.sh)进行 ad-hoc 签名、版本/架构/DMG 白名单及字节一致性检查后发布。v0.9.1 工作流运行上述聚焦检查，但不运行应用、Science 或真实 provider；不保存 Actions 构建缓存与中间 artifact。公开同名 Release 不会被重跑覆盖。

## 1. 固定发布输入

- 目标版本、分支与 exact commit；
- 目标架构与预期 app / DMG 文件名；
- package.json / lock、Cargo.toml / lock 与 Tauri 配置中的版本一致；
- README、CHANGELOG、升级说明和 known limitations 已准备；
- 工作树干净，受保护 worktree 不参与发布。

把 source commit 记入该版本的 `docs/evidence/releases/<version>.md`。

## 2. 源码门禁

```bash
GATE_ROOT="$(mktemp -d /private/tmp/csg.XXXXXX)"
chmod 700 "$GATE_ROOT"
bash test/run_all.sh --output-root "$GATE_ROOT"
git diff --check
```

该命令必须绑定 clean、non-shallow 的 exact `HEAD`，并取得完整 16-suite PASS
completion seal。preflight、环境或 suite 阻断时应在满足同一候选约束的环境复跑；
不能把局部测试、stdout 摘要或旧 `current-env clean` / `release-ready green`
词汇改写成当前 `SOURCE-GREEN`。

source gate 通过后可发布 immutable `SourceCandidateRecord`，但它仍只属于 source 层。真正的 release candidate 必须引用该 record，并另外绑定 release-profile run、当前公开 base 与所需 release gates；artifact/public evidence 只能在随后的 `ReleaseEvidenceV1` 建立。三层 candidate SHA、previous release identity 或 digest 任一不一致都 fail closed。

## 3. 构建 artifact

```bash
cd desktop
npm ci
npm run tauri build
cd ..
```

Codex OAuth 与 Gateway 不要求 Apple Developer 身份、Developer ID、Team ID 或正式签名。需要公开分发时，维护者可以在独立发布流水线中选择 Developer ID / notarization；不得把该可选分发步骤写成源码构建或 Codex 登录前置。

从目标 commit 构建后核对：

- `.app`、DMG 与 `CFBundleShortVersionString`；
- `Contents/MacOS/desktop` 与 `Contents/MacOS/csswitch-gateway`；
- `Contents/Resources/scripts`；
- 不存在旧 Python `Resources/proxy` runtime；
- gateway executable identity、Tauri externalBin / resources 和注册命令与源码一致。

计算最终 DMG 的大小和 SHA-256，之后任何重建都视为新 artifact，需重跑后续层。

DMG 必须从一次性创建的空 staging 目录生成，只复制本次 clean build 的正式 app；禁止把持久化 `target/release/bundle/macos` 或其他可能含历史产物的目录整体作为 DMG 输入。

## 4. 临时安装与 runtime

只读挂载 DMG，把 app 复制到隔离位置或使用独立 bundle ID；未经授权不覆盖 `/Applications/CSSwitch.app`。

使用临时 HOME / data-dir、动态端口、假凭证验证：

- Gateway ownership、启动 / 停止；
- 固定路径且通过本地身份 / 文件安全校验的 updater runtime 优先于 installed App、无效 `SCIENCE_BIN` fail-closed、隔离 cache one-shot；
- Science start / reopen / recovery / url / stop 的强 runtime identity，并确认高频 UI status 只报告 HTTP health 与已记 metadata；
- 外部 Skill route、install / attach / load / restart / uninstall / detach；
- 外部 Skill bridge 失败只 warning；系统 SSH 默认关闭不影响启动，但 opt-in 后缺失 / 不安全 config 或 wrapper 必须 fail closed。

真实 provider、真实账号和真实 SSH server 只在单独授权后验证，并与 loopback 结果分开写。

## 5. 分发检查

分别执行并记录：

- `hdiutil verify` 通过，并只读挂载最终 DMG；
- 排除 `.DS_Store`、`.VolumeIcon.icns` 等预期镜像元数据后，根目录精确白名单只能包含一个正式 app 和一个 Applications 链接；
- app 名称、bundle ID、版本、架构与发布输入完全一致，Applications 链接精确指向 `/Applications`；
- `.app` 数量必须等于 1；任何 Test、Acceptance、历史 app 或其他非白名单载荷都阻断发布。

至少记录最终附件 SHA-256、包内 Desktop/Gateway hash、版本和安装/runtime 结果。上述根目录白名单必须同时对本地候选和从公开 release 重新下载的附件执行。如果维护者选择公开 macOS 分发签名，再单独记录 Developer ID、notarization、stapled ticket 与 Gatekeeper 结果；本项目不提供或强制一个固定 Team ID，也不把这些分发证据当作 Codex 功能本身的前置。

## 6. 发布与回读

在明确授权后创建 tag / push / GitHub Release / 上传附件。发布后重新查询：

- tag peeled commit 与目标 commit；
- release 非 draft / prerelease 状态与发布时间；
- 最终附件名称、大小和 digest；
- 重新下载或用独立同字节 artifact 计算 hash；
- README 下载入口、CHANGELOG、升级说明和 known limitations 一致。

只有公开页面和最终附件都回读一致，才写“已发布”。未取得的 installed runtime、live provider、签名或公证层必须明确写“未建立”。

## 7. 收尾

- 将版本结果写入 `docs/evidence/releases/`；
- 刷新 `.agents/context/current-release.md`、verified-state 与 known-issues；
- 再检查所有 worktree，确认没有误改用户工作区；
- commit、push、tag、release 和清理分支分别报告，不合并授权。
