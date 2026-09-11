# 系统 SSH 配置复用

状态：当前 Feature Contract

最后复核：2026-08-14（Asia/Taipei）

失效条件：Science SSH/parser surface、CSSwitch SSH production owner、packaged wrapper、授权边界或
任一 source/artifact/runtime 证据改变时，受影响段立即复核。

该功能自 v0.5.0 起提供，v0.8.1 补充了隔离 Science 的 SSH 前置校验桥接。它让隔离 Science 在用户明确授权后，按系统 OpenSSH 语义复用真实 `~/.ssh/config`；它不是 SSH server、端口转发 UI 或公网暴露功能。

## 默认与 opt-in

`reuse_system_ssh` 默认关闭。关闭时，CSSwitch 不把真实系统 SSH 配置注入隔离 Science。

启用时，CSSwitch 会在隔离 HOME 的 `.ssh/config` 创建一个 `0600` V2 普通入口文件。它不复制真实配置，精确结构是：

```text
# CSSwitch managed system SSH config bridge v2
Host <从真实 config 安全枚举的具体 aliases...>
Include "<真实 ~/.ssh/config 的绝对路径>"
```

`Host` 行让 Science 在真正执行 SSH 前取得候选 alias；`Include` 让 OpenSSH 继续按真实配置求值。该 stub 不是“只有一条 Include”，也不是对真实 config 的语义副本。

启用后，CSSwitch 在隔离环境 PATH 前放置一个窄 wrapper，最终执行：

```text
/usr/bin/ssh -F <real-home>/.ssh/config <原始参数...>
```

参数仍由调用方交给系统 `ssh`；wrapper 只固定配置文件入口，不实现 SSH 协议，也不读取或显示私钥内容。

## 授权的真实含义

这是一项行为授权，不只是“读一个 config 文件”。系统 OpenSSH 会按原生规则处理：

- `Include`
- `IdentityFile`
- `IdentityAgent`
- `ProxyCommand`
- `Match exec`

这些规则可能进一步访问其他文件、ssh-agent 或本机命令。用户启用前应理解现有 SSH 配置的信任边界。

## 不会做的事

- 不复制或 symlink 整个 `.ssh`，也不复制真实 config 内容；
- 隔离 config 不是指向真实文件的 symlink，避免 Science 写穿真实配置；
- 不把 private key、config 内容或 ssh-agent 数据传到 CSSwitch UI；
- 不启动 `sshd`，不开启 macOS Remote Login；
- 不修改防火墙或建立 `0.0.0.0` listener；
- 不把 SSH 访问与 CSSwitch inference Gateway 混成同一服务；
- 不保证某个 host、key、agent、ProxyCommand 或网络一定可用。

## 失败边界

默认关闭时，SSH 不是普通 Science 启动的前置条件。用户启用该设置时，CSSwitch 先验证真实 `~/.ssh/config`；SSH 授权状态变化会先停止仍使用旧授权的隔离 Science，再保存新设置。关闭授权会撤销 CSSwitch 管理的隔离 config；若该位置是外来文件、symlink 或特殊文件，CSSwitch 会拒绝覆盖或删除并据实报错。

兼容旧版 V1 入口：只有精确 CSSwitch V1 标记、指向当前系统 config 的精确 Include、当前用户拥有且为 `0600` 的单链接普通文件，才可进入启动事务。预检不修改文件；事务先保存 V1 before-image，现有启动脚本再生成 V2。运行态健康检查仍只接受当前 Host 列表对应的 V2，不把旧入口当作已就绪。若启动失败，只有本次事务已观察并仍能确认身份的 V2 candidate 才可恢复成原 V1；外来内容、链接、额外指令或身份变化均拒绝覆盖，无法确认归属时保留恢复状态而不宣称已回退。系统 config 和同目录其他 SSH 文件不属于此次迁移目标。恢复固定隔离 `.ssh` 目录句柄，先以不覆盖方式保留待替换对象并核验其身份，再以不覆盖方式发布 V1；并发出现的新文件不会被覆盖。被移走的对象保留在隔离目录的 `.csswitch-ssh-recovery-*` 文件中，归属异常时供人工检查，不自动删除。中断重放即使已看到 V1，也必须补做目录持久化，失败时不得清除恢复状态。

启用后的每次启动都会再次校验 config 与 packaged wrapper。config 缺失、wrapper 缺失或路径不安全时，启动 fail closed 并清理部分启动，不能以 warning 略过。

当前 packaged wrapper validator 检查：asset root/scripts/wrapper 目录链不是 symlink 且为目录；wrapper 本身不是 symlink、是普通文件、不超过 128 KiB，并至少有一个 executable bit。它当前不检查 wrapper 的精确内容/hash、owner、link count、group/world writable 或精确 mode，文档和验收不能把这些未实现检查写成已证明。只有 Science 已成功启动后的某次 SSH 命令失败，才只影响该命令。

错误报告不得打印私钥路径、config 内容、ssh-agent 数据或其他敏感信息，也不得为了诊断读取真实 private key。

## 三道动态 gate

| Gate | 要证明什么 | 当前边界 |
|---|---|---|
| 1. Science parser acceptance | 当前 Science 接受 alias inventory、`ssh_hosts` 与 V2 stub | source 已建立生成/事务链；动态 acceptance 必须绑定 exact package/runtime，当前判定从[已验证状态](../../.agents/context/verified-state.md)读取，不得从历史 Science 版本继承 |
| 2. OpenSSH invocation | Science 实际选择 wrapper；wrapper 调用 `/usr/bin/ssh -F <real config>` 并保留参数/env | wrapper source 已建立；需隔离 recorder，不连真实 server |
| 3. real server connectivity | key/agent/known_hosts、DNS/network、server 与远端命令成功 | 只在另行授权后验证；不能由 Gate 1/2 推出 |

静态/source 还应分别证明：默认关闭、保存时缺失 config 拒绝、stub/sidecar 事务、无 `.ssh` 复制、无 `sshd`/防火墙/公网 listener。`/usr/bin/ssh -G` 不能替代 Science parser，wrapper source 也不能替代 Science 实际 invocation。

任一道 gate 的成功都不能泛化为所有用户 config、key、agent、网络或 server 可用。
