# Science runtime 合同

本文描述跨版本的 executable 选择、HOME/data、身份和恢复合同。具体 Science 版本、hash、embedded identity、事故与 E2E 结果只进入[日期化调研](../audits/2026-07-30-v084-architecture-reconnaissance.md)或 evidence。

## 设计定位与沙箱边界

CSSwitch 是 Science 第三方模式的 **运行编排器和模型协议适配器**，不是 Science
产品本体。目标是在不读取真实 Claude 账号状态、不重建 Science 领域系统的前提
下，让第三方 provider 在自身能力范围内尽可能保留 Science 原生体验。

CSSwitch 长期拥有的稳定合同只有：

1. executable 选择、来源和 runtime identity；
2. 官方主目录浏览、独立 config/auth/data-dir、端口与禁止将真实账号作为第三方状态的布局；
3. 隔离 data-dir 内的本地虚拟登录投影；
4. loopback Gateway、path secret、provider launch plan 与 model protocol 兼容；
5. launch receipt、listener、data-dir 和进程身份对齐；
6. protected projection、pending cleanup 和精确补偿；
7. 显式、可关闭、局部失败的 Skill/SSH/Codex 等窄 bridge。

Science 继续拥有 UI、project/session、permission、artifact、memory、
environment/kernel、Agent、Skill 执行、generic MCP/Plugin 和官方服务客户端。
这并不免除 CSSwitch 的兼容责任：如果 Science 原生工作流依赖模型请求，
Gateway 必须在 provider 声明的能力内保留 stream、tools、`tool_choice`、
reasoning、structured output、vision、错误与停止语义，或给出明确、可定位的
降级。

这里的 sandbox 是身份与数据隔离，不是“全部断网”，也不能因为多种流量经过同一
Gateway 进程就把它们都解释成 model routing：

- model inference 必须进入 CSSwitch Gateway；
- 显式 bridge 只走自己的 loopback/stdio/IPC 合同；
- connector、文献、云、generic remote MCP 与 remote compute 等 Science 原生
  外部语义由 Science、用户和外部服务管理；当前非 loopback HTTPS 仍可能因进程级
  `HTTPS_PROXY` 以 raw `CONNECT` 穿过 Gateway，CSSwitch 只拥有 tunnel target
  policy、deadline 和 transport diagnostics，不拥有 MCP/connector/云等上层语义；
- 非 HTTPS、显式 proxy bypass 或不读取 proxy environment 的 client 不能由上述
  合同推出；CSSwitch 不注入其凭证、不冒充 entitlement，也不能无说明地改变现有
  transport；
- 真实 Claude OAuth/token、真实账号数据库和未经用户选择的外部凭证不得复制或
  投影成第三方身份；浏览主目录不等于把整棵主目录授予模型读写。

### 本机 Home 浏览与独立状态

默认（含缺字段的旧版升级）保持隔离 HOME。设置中的明确授权复选框映射到
`Config.allow_science_host_home`，只有 JSON 布尔 `true` 才启用；默认 false 不序列化，
不改变未授权旧配置的事务 fingerprint。变更该授权走已有串行 `set_settings` 事务，
启用或撤销都会先停止现有 Science / Gateway；停止失败不提交新值。撤销不回滚
官方程序已经执行的操作。cold start、recovery 和 auto start 使用同一已保存值，
共同经过 `configure_science_home_access`；未授权时既不准备 Home 配置也不设置
host-home 标志，不从旧配置或环境变量推断同意。

前端 `directory-access.js` 在交互启动前提示目录访问方式，工作台提供常驻权限入口。
它仅在 `set_settings` 回执确认提交后继续 host-home 启动；取消不保存、不启动，
隔离选择只在本次面板会话内记忆。授权沿用既有配置字段，不迁移为默认同意。
授权后用户在 Science 自己的目录选择器选择工作文件夹；HOME 外路径仍受 Science
独立 granted-root 检查，本入口不会伪造授权，也不修复官方目录连接器登录。


经用户明确接受官方 Home 行为后，Desktop cold start / recovery 由
`runtime/science/home_layout.rs::prepare_science_host_home` 准备隔离配置，再由
`ScienceHostAdapter::spawn_launch` 启用 daemon 的真实 host `HOME`。Science 原生
`host-home` / `host-browse` 因而可浏览本机主目录；读写授权仍归 Science，不添加
全 HOME grant，不关闭生产 sandbox，也不改浏览器页面或另建文件选择器。

配置 owner 只安全读取隔离 data-dir 的 `config.toml`，保留其他偏好，显式固定
`data_dir`、`paths.auth_dir` 和 `paths.conda_home`；已有认证/Conda override 若指向
其他位置则拒绝，不静默覆盖；内容未变但权限为只读时仍原子归一到 `0600`。
原子写入后向 launch allowlist 传递配置摘要，脚本核对
属主、权限、无符号链接和摘要，使用显式 `--config` / `--data-dir`，不加载真实 Home
的默认 Science 配置来建立第三方身份。配置准备或身份校验失败时不启动 Science。

同一 owner 在隔离 HOME 的 `.csswitch-science-tools/security` 写入固定私有 shim，
以 `zsh -f` 禁止加载用户 shell 初始化文件，通过受核对的内容身份放入 Science PATH；
只有该子命令恢复隔离 HOME，沿用旧
CSSwitch 专属钥匙串，避免主目录切换使既有第三方登录不可解。CSSwitch 不读取或
输出真实 Keychain / OAuth 内容。其他继承环境仍清空，provider secret 仍只到 Gateway。

此模式不是“Science 永不接触真实 Home”：官方 executable 在加载指定配置前的
历史目录检查/迁移，以及 HOME 派生的官方资源行为，按用户授权由 Science 自身
执行；CSSwitch 不扫描、复制或自行迁移真实 Science 数据。原来已健康的 daemon
不强制重启，需用户停止并重新启动后才使用新 Home。无 host-home 标志的手工脚本
和 `--version` / `status` / `url` / stop 控制探针仍保持隔离 HOME，不扩大诊断权限。

生产接线、配置保存/拒绝和参数防漂移分别由 `home_layout_tests`、
`runtime::launch_env` 与 `test/test_launch_science_env_allowlist.sh` 做聚焦检查；
这些检查不建立真实文件读写、provider 或当前运行实例的验证结果。

凭证边界的 process environment 合同：Tauri → launch/stop script 与
launch script → Science 均使用显式 allowlist（`runtime/launch_env.rs` +
`scripts/launch-virtual-sandbox.sh` 的 `env -i`）；stop/launch 控制面注入
`CSSWITCH_HOST_HOME`，Gateway base 注入绝对 host `HOME`（Codex 等主机态路径）；
provider secret 只进入 Gateway。Science `--version`、`status` 与 `url` 共用唯一
`runtime/science/control_runner.rs`：先 `env_clear`，只恢复 `base_process_env` 与隔离
`HOME`，再以私有进程组、绝对 deadline、有限 stdout/stderr、kill-group 和 direct-child
`wait` 执行。version 使用 15 秒期限和 1 KiB 输出合同；status/url 使用 5 秒期限和
64 KiB 输出合同。sentinel、直接 child 挂起及父进程退出后代继续存活的回归见
`runtime::science::tests`；launch/stop allowlist 的 stub 回归见 `runtime::launch_env`
测试和 `test/test_launch_science_env_allowlist.sh`。完整所有权与 bridge 边界见下方能力依赖正文。

完整 ownership、运行路径、bridge 准入和拆分前冻结项见
[Claude Science 能力依赖](science-capability-dependencies.md)；逐能力当前决策只在
[产品与 Claude Science 能力地图](../features/product-science-capability-map.md)
维护。

## 当前源码 owner

`desktop/src-tauri/src/runtime/science.rs` 是保持历史 module surface 的同模块
façade；生产实现按独立维护原因分布为：

| 边界 | 当前源码 owner |
|---|---|
| 基础类型、常量、receipt/runtime 合同 | `runtime/science/contracts.rs` |
| `--version` / `status` / `url` 的 allowlisted bounded command runner | `runtime/science/control_runner.rs` |
| executable 选择、安全读取、snapshot 与版本识别 | `runtime/science/executable.rs` |
| predecessor / candidate observation、normalized diff、adoption decision 与 milestone ledger | `runtime/science/adoption.rs` |
| 固定 active、pending update、每日节流与用户选择 | `runtime/science/selection.rs` |
| live process/listener identity 与 runtime state | `runtime/science/runtime_state.rs` |
| managed launch、receipt 与启动后身份提交 | `runtime/science/managed_launch.rs` |
| preflight、probe、reuse、URL 与 stop lifecycle | `runtime/science/lifecycle.rs` |
| typed host launch/stop 投影、environment exposure 与 staged identity verification | `runtime/science/host_adapter.rs::ScienceHostAdapter` |
| 历史测试身份 | `runtime/science/tests.rs`；仍保持 `runtime::science::tests::*` |

这些片段仍编译在原 `runtime::science` 模块内；拆分没有建立新的状态 owner，也没有
扩大 executable 来源、environment allowlist、凭证读取、listener 控制或 stop
权限。Science transaction 与 protected projection 继续由
`runtime/sandbox_session/` 拥有，Gateway 进程编排继续由
`runtime/proxy_lifecycle/` 拥有。

当前 source 已关闭此前 Science control/probe 继承 ambient environment，以及
`status/url` 可无限等待或被后代输出 pipe 拖住的边界；该结论不建立 exact artifact、
installed、真实 Science/live、签名或 release 证据。

## CSSwitch → Science 生产控制链

这条链只描述 CSSwitch 自有的运行编排；Science 内部的 project、Agent、
artifact、memory、environment 和 official entitlement 仍由 Science 或外部服务拥有。

| 阶段 | production caller | 当前 owner | 输出 / 失败边界 |
|---|---|---|---|
| runtime 预检 | `desktop/src/runtime-controller.js::oneClick` → registered `science_runtime_preflight` | `commands/runtime/status.rs` | 正常路径只验证 fixed active；首次无 selection 才 bootstrap。返回 `installed_ready` / `cached_choice_required` / missing / error，不启动进程 |
| update 检查 / 选择 | setup scheduler → `check_science_runtime_update`；前端 → `science_runtime_update_status/action` | `runtime/science/selection.rs` | 每 24 小时最多探查一次并发布 pending；接受只切换下次 cold start active，不重启健康 runtime |
| 启动 IPC | `desktop/src/runtime-controller.js::oneClick` → `runOneClick` → registered `one_click_login` | `commands/runtime/one_click.rs` | 锁外 auth preflight 与 backend failure 投影；不直接启动 OS 进程 |
| 入口决策 | command → `runtime/sandbox_session/one_click.rs::one_click_login_entry` | `runtime/sandbox_session/one_click.rs` | recovery / healthy reopen / cold 三路分派，以 typed journal 与 generation 拒绝漂移 |
| cold coordinator | cold branch → `run_cold_one_click` | `one_click/cold.rs` | 顺序拥有 prior stop、authority、Gateway、phase dispatch、route 与 finalize |
| Science phase | coordinator → `run_managed_science_launch_phase` | `one_click/cold/science_phase.rs` | 启动、health、listener/runtime identity、managed receipt、DB reverify/restart；失败回传 typed phase result |
| host 边界 | Science phase / recovery / stop caller → `ScienceHostAdapter` | `runtime/science/host_adapter.rs` | 生成 allowlisted argv/env，投影 script acceptance、health、identity、receipt 与 stop outcome；不重建 host identity |
| OS 进程 | adapter → bundled `scripts/launch-virtual-sandbox.sh` / `stop-science-sandbox.sh` → exact `claude-science` | scripts 与已选 runtime 的窄 host contract | `env -i`、隔离 HOME/data-dir、loopback port 与 exact listener/PID；任一身份不可证即 fail closed |
| 读模型 | frontend polling → registered `status` | `commands/runtime/status.rs` | 只投影轻量 HTTP health 与已有 metadata，不升级为 strong runtime identity |
| 显式停止 | frontend `stop_all` / `quit_app` / mode/settings teardown | `commands/runtime/lifecycle.rs` + `ScienceHostAdapter` | owner claim → 锁外 stop/wait → generation + full identity CAS；陈旧结果不得清 replacement runtime |
| native exit | Tauri `RunEvent::Exit*` → `cleanup_for_exit_with` → `execute_process_local_science_stop_with` | `desktop/src-tauri/src/lib.rs` 编排 terminal policy；`commands/runtime/lifecycle.rs` 拥有 Science stop publication | owner claim → 锁外 stop/wait → generation + full identity CAS；best-effort 忽略 stop failure，仍关闭 Gateway |

Gateway 在 Science phase 之前由 `runtime/proxy_lifecycle/` 建立并提供 typed
launch receipt；Science 启动后的 provider/model 请求再进入 packaged Rust Gateway。
因此“Science 健康”、“Gateway 健康”、“provider 能力通过”和“Science-native
功能可用”是四个不能相互补绿的结论。

## 分离六个事实

1. **executable**：实际执行的 `claude-science` 文件；
2. **persistent data-dir**：`~/.csswitch/sandbox/home/.claude-science`；
3. **isolated HOME**：CSSwitch 第三方模式传入的 HOME；当前部署让它与 data-dir 根共址，但这不是 Science 通用 `data_dir` 语义；
4. **fixed user-level state**：Science 固定写入 `~/.claude-science` 的 config、认证与 shared package environment；在第三方模式中该 `~` 指隔离 HOME；
5. **environments / kernels / runtime resources**：starter/task environments、session kernels 与 `<data-dir>/runtime/<version>/`；
6. **live identity**：canonical executable、data-dir、监听 PID、端口和受管启动记录的组合。

官方合同中，data directory 保存 per-org conversation、artifact、delegation 和 workspace；认证 token 与 shared package environment 固定在 `~/.claude-science`，不随 `data_dir` 移动。CSSwitch 用隔离 HOME 把两类状态一起隔离，但文档和恢复逻辑仍须区分所有权。

starter Conda Python/R 环境、跨项目复用的 named task environment、session 内 Python/R kernel 与 inline package 都是不同生命周期。CSSwitch 不拥有这些环境，也不从 opaque root 推断功能可用；Node 的上游所有权/scope 仍为 `UNKNOWN`。

## active runtime 与新启动

1. 如果设置了 `SCIENCE_BIN`，它必须是绝对、非 symlink、可执行且能安全读取版本的开发 override；无效时 fail closed，不读取 selection 或继续猜其他 binary。
2. 普通启动只读取私有 `selection.v1.json` 的 active source、version、SHA-256 与 size，从固定 snapshot 目录派生唯一 path，并复核 canonical path、完整内容指纹、权限和来源要求。文件一旦存在就必须包含 active；缺少 active、仅有 pending、无法解析或 snapshot 不可证都 fail closed，不能重新进入 bootstrap 或探查 updater、App、cache。
3. selection 尚不存在时允许一次 bootstrap：先检查固定的 `~/.claude-science/bin/claude-science`；其路径、属主、权限、大小、Mach-O 与 embedded identifier / Team ID 全部通过后，安全读取并提交 0500 内容寻址 snapshot。没有 updater 时再检查 `/Applications/Claude Science.app` executable，并同样先形成 snapshot。snapshot 在 selection writer lock 之外以私有临时文件、fsync、内容寻址 hard-link 和目录 fsync 独立发布；崩溃遗留的同 inode 临时链接会在复用前清理，随后强制复核 owner、0500、单链接、size/hash。active selection 再由私有 writer lock / identity-bytes CAS 原子发布，且提交前后都以 exact snapshot identity 失败关闭。
4. 只有 bootstrap 来源都不可用、`<CSSwitch data-dir>/bin/claude-science` 可执行且版本可确认时，preflight 才返回 `cached_choice_required`；用户可授权 `cached_once`。cache 授权只在本次启动内存中生效，不写成 active。

已安装 App 是 snapshot 的来源而不是可变 active path。版本探测得到的 source fingerprint 必须与随后安全读取形成的 snapshot 内容/大小完全相同；两步间 App 被替换时 bootstrap/update check fail closed，不能把旧版本标签绑定到新字节。这样 App/updater 后续替换不会让日常启动切换 binary，也不要求为获得旧字节回读已变化的来源。

`official_updated` 只读取并快照 updater 固定路径中的单个 executable；该路径已观察到 standalone updater 与 App-seeded 两种精确 identity，当前源码只接受枚举的 exact identifier + Team ID 组合。具体字符串、hash、版本与 `source-fixed-product-pending` 结论留在日期化 audit；它们不能写成 final artifact、installed/live 或公开 release 已证明。

内容寻址 snapshot 只证明被采用 executable 的稳定字节身份；可比较的更新 provenance 由
`science-runtime-adoption/ledger.v1.json` 单独维护。每条 `ScienceExecutableObservation` 只含
固定来源、已验证版本、SHA-256、大小、embedded-identity 校验状态和可选 snapshot id，不保存
executable 路径。`ScienceUpdateAttempt` 记录 predecessor、candidate、固定枚举字段组成的
normalized diff，以及 `deferred_healthy`、`rejected` 或 `selected` 决策。安全校验在形成完整
candidate observation 前失败时，rejected 记录只保留固定 source 与 rejection code，不读取或
猜测候选内容。

selected attempt 从 `observed` 只允许按 CAS 顺序进入 `launch_committed`、再进入 `finalized`。
无 receipt provenance 的 stopped-to-started 选择只检查最新一条 selected attempt：candidate 相同的
普通重启或未完成 crash retry 复用它；latest selected candidate 已变化时必须新建 attempt，并以
latest finalized candidate 为 predecessor，因此 A→B→A 会记录新的 B→A normalized diff；
managed launch receipt schema v2 绑定 runtime source、version 与 attempt id。历史稳定文件名
`science-managed-launch.v1.json` 不改名：其中 schema v1 继续只读兼容并明确视为 provenance
unknown，新写入均为 schema v2。新提交的 `runtime_binding` 与 finalize intent 都绑定同一个 exact
attempt id；fresh probe 只从身份已验证的 V2 receipt 回填该 id，preflight 只有在 receipt、runtime 与
持久 binding 三者 id 相同时才补记 `finalized`，旧 V1 或缺失/不同 id 只能补记
`launch_committed`。带 provenance 的 fresh finalize replay 在 authority cleanup 前还必须重建同一
健康 runtime，并证明 action、binding、V2 receipt、runtime 四方 attempt id 完全相同；receipt
缺失、V1、非法、不同 attempt 或 runtime identity drift 都保留 journal。prior-stop 与 private compensation recovery identity 也携带可选
attempt id；private manifest 中 candidate/prior attempt 的同值有界索引随 V2 compensation journal
发布，避免 authority restore 后只剩 private replay 引用时对应记录被压缩。

managed receipt 的 write/clear 属于 authority filesystem mutation。普通 launch/stop path 在同一
writer leaf 外取得共享 `AuthorityWriterGuard`；已经持有 exclusive replay/history effect lease 的
owner 只能传入 scoped、不可跨线程的 `AuthorityWriterBypass`，避免嵌套 SH 自锁，同时让 EX lifetime
继续拥有序列化责任。该 fence 与 config writer fence 是不同边界；锁序、per-target replay 和五条
transaction stop path 见[运行时状态与事务](runtime-state-transactions.md)。

ledger 位于 CSSwitch 私有 data root，不在 Science data-dir 内；目录 / 文件分别收紧为 owner-only，
读取有硬上限并使用 no-follow，更新由跨进程 writer lock、期望 identity / bytes 复核与原子替换
保护。最多保留 64 条；live receipt、durable prior-stop、active compensation retention id 以及未完成 selected attempt 不能被压缩；
receipt 文件存在但权限、类型、大小、读取竞态或 JSON 无法安全确认时，压缩直接失败关闭，不能按“无 live receipt”处理。
至少保留最近 16 条。记录只比较上述 allowlist metadata，不能读取或 diff Science 用户账号、
组织、对话、project、environment、runtime assets 或其他 opaque data。历史版本的日期化兼容性
调查仍不能替代这份通用机制，也不能把本地 embedded metadata 写成官方来源的密码学证明。

snapshot 位于 `<CSSwitch data root>/runtime-snapshots/science/`，不在 Science data-dir 内。`selection.v1.json` 与 adoption ledger 共用 CSSwitch 私有 store 和跨进程 writer lock，但分别原子发布；selection 不保存 executable path，只保存 active、可选 pending、有界且不逐次覆盖的被拒绝候选 SHA-256 集合、最近一次检查时间与临时检查 claim id。拒绝集合最多 64 条，满载时新拒绝失败关闭而不是遗忘旧选择。CSSwitch 不扫描、复制或读取真实 Science 账号、组织、配置、`conda`、`runtime` 或 `seed-assets`，不下载 Science、不调用 updater，也不覆盖 Science cache。bootstrap 检测到 updater 但本地校验失败时会显式报错，不静默回退旧 App 或 cache；后台检查失败保留 active，预探查 claim 已推进本次检查时间，下一持久间隔再试。

embedded identifier / Team ID 只作为格式与误选防护，不声称密码学证明文件来自 Anthropic。该路径沿用 CSSwitch 已有的“信任当前用户安装的本地 Science”边界；复制前后复核同一 source inode/metadata，installed App snapshot 还必须匹配版本探测所得完整 source fingerprint（device、inode、size、mtime、mode、SHA-256），即使替换文件字节相同也不能混用旧探测结果。snapshot 以完整 SHA-256 命名并进入 host-context fingerprint。启动、恢复和停止等强控制路径会重新验证 snapshot，内容变化时 fail closed；高频 UI `status` 是下文明确的轻量例外，只投影 HTTP health 与已有 metadata。`sandbox_url()` 也不是独立的强身份边界：runtime 不再 current 或 CLI URL 获取失败时，它会回退到裸 `http://127.0.0.1:<port>`；手动打开入口会先验证 listener/runtime，但冷启动与 reuse 路径依赖调用它之前已经完成的身份检查。updater 随后替换 source 不会改变已运行 daemon 的 executable 身份。为支持 CSSwitch 自身重启后的接管，恢复探测会在私有 snapshot 目录中重新验证已有的内容寻址 executable；历史 snapshot 只参与现有 daemon 的身份恢复，不改变 stopped-to-started 的新启动选择顺序。

## 启动与网络参数

新进程使用预检后的 binary 和固定 data-dir，并显式传入：

- `--host 127.0.0.1`；
- CSSwitch 选择的 UI port；
- 单独校验的 `--sandbox-port`；
- `--no-auto-update`。

Gateway 同样只监听 loopback。端口健康不等于身份健康；公共网络暴露不属于当前合同。

## 一键启动的事务快照边界

CSSwitch 是启动与路由插件，不拥有 Science 的语言环境。冷启动事务只快照可能被 CSSwitch 启动准备流程改写、且失败时必须精确补偿的受保护状态：

- `encryption.key`、`.oauth-tokens/`、`active-org.json`、`.key-backups/`、`auth-owner.lock`；
- `config.toml`、`csswitch-ssh-bridge.v1.json`、`mcp/`、`.csswitch-route-state.json`；
- `orgs/`（包含组织数据库、历史和组织内 Skills，不能按缓存处理）。

`conda/`、`runtime/`、`seed-assets/`、`r-libs/`、`sbx-bind-src/` 是 Science-owned opaque roots。CSSwitch 只对这五个固定顶层入口做 no-follow 的目录、owner、权限和 device/inode 绑定校验，并在任何受保护写入前及 `serve` spawn 紧前重验；不得递归遍历、读取、复制、fsync、删除或回滚其内容。未知的其他顶层入口同样按外部状态原地保留，除非它与受保护入口冲突或根目录身份不安全。这个边界不依赖 APFS clone，也不把环境误称为可重建缓存。

这是当前 **protected projection** 合同。旧候选曾对整棵 authority 做 full-tree snapshot，因 Conda 大文件、runtime symlink、entry/逻辑容量而多次失效；该历史只证明当时的故障与修复过程，不表示当前仍递归 clone 全树。后继关系见[0.1.25 compatibility evidence](../evidence/investigations/2026-07-28-claude-science-0.1.25-compatibility.md)末尾指针。

私有快照根一旦建立，就在复制任何受保护状态之前，以 marker、精确 path/device/inode 和原子 manifest 的 `active_recovery` disposition 持久登记；完整快照仍必须在任何 OAuth、SSH bridge、MCP 或路由写入之前完成。只有成功、完整补偿或无需继续恢复的选择分支，才能通过 compare-and-swap 把同一票据转成 `cleanup_only`，随后才允许删除；因此 capture 中途崩溃不会留下未登记的敏感 orphan，config journal 即使被补偿回滚，也不能把仍需恢复的快照误删。进程若在活动事务中崩溃，下一次启动不得自动删除这份快照，也不得把可能的部分写入态当作新基线；当前策略是保留快照并要求人工恢复。旧版 `start_science` 日志没有 runtime 指纹，升级后按 `environment_uncertain`、`newer_runtime_required` 失败关闭，不得跨 runtime 自动启动。

事务在调用 `claude-science serve` 前失败时，可以精确补偿受保护状态。调用后，Science 可能已经迁移或修改自身环境，即使受保护状态已恢复，结果也必须标为 `environment_uncertain`；跨 runtime 失败不得用旧 runtime 自动重启这个已暴露环境。无法证明候选 Science 已停止时，不得在其下方恢复凭据或组织数据库，必须保留受限恢复快照并返回 `cleanup_required`。

## 运行中身份与恢复

CSSwitch 在内存中记录实际 launch binary path、来源（`explicit`、`official_updated`、`installed_app` 或 `cached_once`）、版本和选择时文件指纹。启动、复用、恢复与停止操作使用这份 runtime metadata，并在需要控制 daemon 的边界做强身份检查。URL helper 也接收该 metadata，但自身允许上述 localhost fallback，不能单独作为强身份证明。

停止不能只信任 Science CLI 的退出码：部分版本会返回成功并删除 lockfile，但 daemon 仍在监听。CSSwitch 在调用 CLI 前后都要求 sandbox port 的唯一监听 PID 与已记录 executable 精确匹配；CLI 后端口仍存活时，只向这一个前后均匹配的 PID 发送 TERM，超时后再次复核同一身份才 KILL，并以端口实际关闭作为成功条件。监听身份变化时不发送信号并返回错误。

高频 UI `status` 是例外：它只对 sandbox port 做短超时 HTTP health，并把内存中的 source / version metadata 投影到诊断结果；launch binary path 仍只保留在 `AppState`，不跨 status DTO 暴露。该路径不反复调用 `claude-science status`，不重新核对监听 PID，也不能证明当前监听者就是已记录 runtime。

CSSwitch 自身重启后，只能在以下条件同时满足时接管已有 daemon：

- 监听 PID 的 canonical executable 与候选 binary 匹配；
- 候选 CLI 确认的是同一 data-dir daemon；
- 端口与受管状态一致。

单独的端口占用或 `status` 成功不足以证明身份。已健康 daemon 应复用，而不是只因 App 版本或可选 bridge 状态变化被强制重启。

## 升级合同

后台线程可以每小时唤醒；真正到期时先在私有 writer lock 下原子写入随机 claim id 与本次时间，再到锁外探查，因此并发进程或探查中崩溃都不会在 24 小时内触发第二次来源探查。已有 pending 时不再探查，也不因来源暂时缺失、当前 active 或低优先级候选自动清除，必须等待用户的精确选择。每小时唤醒不是用户提醒：只有检查首次创建新 pending 时发送一次 UI 事件；已有 pending 由启动/status 读取并保持被动展示，不重复发事件。到期检查仍按安全 updater → installed App 的顺序读取单一 executable 并形成内容 snapshot；active 已来自 updater 时不会因 updater 暂时缺失而探查或提示低优先级 installed App。与 active 内容/版本不同且未被用户拒绝的同级或更高优先级候选只写为 pending，并在 UI 提供“下次启动使用新版本”或“继续当前版本”。接受以 expected pending SHA-256 做 CAS；接受或拒绝都会在 selection 写入前后复核 active snapshot 的目录、0500/owner/nlink 与完整指纹。写后复核失败，或 selection 已 rename 可见但目录持久化确认失败，都会以刚提交的 exact identity/bytes 做 CAS 回滚；bootstrap 的首条 selection 失败时删除该条记录，已有 selection 则恢复原 active/pending。成功接受只改变下次 cold start 的 active；成功拒绝的 SHA-256 累积在有界集合中，后续拒绝不能让旧内容再次提示。两种选择都不停止或重启当前健康 daemon。

后台检查发现健康 runtime 与候选不同时写 `deferred_healthy` observation；日常 preflight / healthy reopen 不再探查更新源。正常 cold start 解析新的 active 后才建立或复用 `selected` attempt。managed receipt 提交后记录 `launch_committed`；receipt 已落盘但 milestone 尚未写入的崩溃窗口只能恢复到 `launch_committed`。one-click finalize intent 持久绑定 exact attempt id，binding/finalize CAS 成功后才记录 `finalized`；若其后 ledger 暂未收敛，下一次 preflight 也必须先证明当前 receipt 与已提交 binding 对同一 runtime 一致，不能只凭 receipt 提前 finalize。该 bookkeeping 重试不回滚已成功的 runtime transaction。CSSwitch 不迁移或覆盖组织、项目和 Skill 数据。

每次上游 App 更新后，分别验证：

1. 实际 selected binary 与 `--version`；
2. data-dir 复用且没有读取真实 HOME runtime 资产；
3. live PID、executable、runtime directory、data-dir 与端口属于同一运行；
4. start / reopen / recovery / url / stop 的强身份一致，并单独确认 UI status 只表示 HTTP health；
5. 外部 Skill route、install / attach / load / restart / uninstall / detach；
6. bridge 不兼容仍只产生 warning，普通 Agent 可工作。

一次上游版本测试不能推出通用最低版本。观察接口变化时，应只关闭受影响 bridge 并如实报告，而不是替换或降级用户 App。

## 非目标

- 不把 `@` artifact / output 当成持久 Skill 注册；
- 不把 `<data-dir>/runtime/<version>/skills` 当外部 Skill 安装目标；
- 不通过 OAuth、私有 bearer、数据库写入或 binary patch 管理 Science；
- 不为 SSH、Skill 或 provider 失败扩大 runtime 权限。
