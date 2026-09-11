# Science runtime 规则

- Science executable、持久 data-dir、版本 runtime 资源、组织数据和监听进程是不同事实。
- `SCIENCE_BIN` 之外的新启动只解析并验证 CSSwitch 私有 selection 中固定的 active 内容寻址 snapshot；selection 文件尚不存在时才按“安全 updater → 已安装 App”做一次 bootstrap。文件存在却缺少 active（包括仅有 pending）必须 fail closed，不能重进 bootstrap。两种来源都先提交 0500 snapshot，并始终复用 CSSwitch 隔离 data-dir。
- `SCIENCE_BIN` 仅是显式开发 override；无效时 fail closed。历史缓存绝不能隐式回退。
- 只允许读取真实 `~/.claude-science` 下固定 executable；通过属主、权限、Mach-O、embedded identifier / Team ID 校验后，将同一次安全打开读取的字节原子提交到 CSSwitch 私有、只读、SHA-256 内容寻址 snapshot。已安装 App executable 同样先形成 snapshot，并把 snapshot 绑定到版本探测前后的完整 source fingerprint（device、inode、size、mtime、mode、SHA-256），不能作为每次启动时重新探查的可变 active path。bootstrap 检测到 updater 候选但校验失败时不得静默回退旧 App；后台检查失败不得替换 active。这些本地 metadata 不是官方来源的密码学证明。不得扫描、复制或读取其他账号、组织、配置、runtime 资源或凭证。CSSwitch 不下载或升级 Science，并保持 `--no-auto-update`。
- 后台 scheduler 可以每小时唤醒，但必须在探查前以私有 writer lock 持久 claim 本次时间，使并发进程或探查中崩溃都不能在 24 小时内再次探查。新内容只写成 pending，且只有首次创建新 pending 时发送一次 UI 事件；已有 pending 只维持被动展示，不得每小时重复提醒。pending 在用户精确接受或拒绝前不得被后续检查清除，等待选择期间不再探查。接受或拒绝都必须在 selection 发布前后复核 active snapshot，写后失败用 exact CAS 回滚。用户接受后只改变下次 cold start 的 active；成功拒绝的 SHA-256 进入有界集合且不得因后续拒绝被遗忘，集合已满时失败关闭，因此同一内容不重复提示。两种动作均不得强制重启健康 daemon。
- Science 与 CSSwitch Gateway 均绑定 loopback；引入或暗示 `0.0.0.0` 需要单独的安全和产品决策。
- 端口占用或 `status` 成功不能单独证明 runtime 身份；需结合 executable、选择时文件指纹、data-dir、监听 PID 和受管启动身份。选择后 executable 被替换必须 fail closed。
- 停止成功必须以 sandbox 端口真实关闭为准，不能只信任 Science CLI 的 0 退出码。CLI 假成功时只允许终止停止前后均保持唯一监听且 canonical executable 精确匹配的 PID；身份变化时不得发送信号。
- 已健康 daemon 不因版本探测或可选功能漂移而强制重启。
- 外部 Skill route / connector 配置失败只降级该可选功能，不阻断普通 Science 启动。
- 系统 SSH 默认关闭；一旦用户启用，真实 config 与 packaged wrapper 的安全校验属于 fail-closed 启动条件，不能当作 warning 略过。

- 用户已明确接受官方 Home 行为时，daemon 可使用真实 host HOME，但必须先安全固定隔离 config/auth/data 路径并核验启动配置；模型的目录读写仍由 Science 授权。官方启动自身的历史目录检查/迁移按该授权执行，Agent 的诊断、控制探针和自动测试仍遵守隔离 HOME 与禁止读取真实凭证的规则。具体 owner 合同见 `docs/architecture/science-runtime.md`。
