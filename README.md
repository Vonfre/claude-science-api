<div align="center">

# 研舟 · SciPort

**用自己的模型 API，打开熟悉的科研工作空间。**

[中文](README.md) · [English](README.en.md)

[下载 v0.9.0](https://github.com/Vonfre/claude-science-api/releases/tag/v0.9.0) · [反馈问题](https://github.com/Vonfre/claude-science-api/issues) · [项目文档](docs/README.md)

</div>

研舟是面向 Claude Science 的本地 API 连接与启动工具。这一版本专注于第三方模型 API：管理连接、选择模型、启动隔离 Science，以及查看本地服务状态。它不是 Claude Science 本体，也不是 Anthropic 官方产品。

![研舟工作台](docs/assets/sciport-workbench.png)

*截图使用虚拟数据，仅展示界面，不代表真实服务连接状态。*

## 你可以做什么

- **一个工作台完成启动**：启动、打开、停止 Science，查看当前连接和待应用状态。
- **管理多个模型连接**：搜索配置、编辑模型、批量删除连接或清除密钥。
- **使用兼容 API**：支持 Anthropic Messages、OpenAI Chat Completions 和 OpenAI Responses 三类协议；服务商是否完整兼容，仍需实际验证。
- **自由填写模型 ID**：默认模型必填；质量、快速和 Fable 模型可分别配置，留空时继承默认模型。
- **区分状态层级**：本地网关、Science 健康检查、上游网络可达性分别显示，不把“网络可达”当作“密钥可用”。
- **保持界面简洁**：工作台与设置两个入口，深浅主题，自检与日志按需展开。

## 下载与安装

本次提供 **macOS Apple Silicon（M 系列芯片，arm64）** 安装包。未提供或验证 Intel、Windows、Linux 安装包。

1. 从 [v0.9.0 Release](https://github.com/Vonfre/claude-science-api/releases/tag/v0.9.0) 下载 `SciPort_0.9.0_aarch64.dmg`。
2. 确认已通过官方渠道安装 Claude Science。研舟不捆绑、不下载、不自动升级 Science。
3. 打开 DMG，将 **SciPort.app** 拖到“应用程序”。
4. 打开研舟，添加 API 连接。

**签名说明：**本版使用本地 ad-hoc 签名，不具备 Apple Developer ID 签名或 Apple 公证。macOS 可能拦截首次打开。请先核对下载来源和 Release 的 SHA-256；确认信任后，按照系统“隐私与安全性”提供的打开选项处理。不要关闭系统整体安全保护。

### 从 CSSwitch 升级

先退出旧版 CSSwitch，避免两个相同应用标识的程序同时运行。新版安装文件名为 `SciPort.app`，但应用标识和 `~/.csswitch` 配置目录保持兼容，不会因为改名自动迁移或清空配置。可保留旧 App 的离线备份用于回退，但不要同时启动。

此版本只公开 API 功能。旧账号或扩展相关数据不会因为界面移除而自动删除；旧的 Science 汉化浏览器扩展需自行停用。回退时退出新版，再恢复备份的旧 App；不要在未备份的情况下手动修改配置文件。

## 第一次连接

1. 点击 **添加 API**，选择服务商或自定义协议。
2. 填写名称、API 地址、供应商提供的精确模型 ID 和 API Key。
3. 点击 **创建**，将该连接 **设为当前**。
4. 点击 **启动 Claude Science**。仅切换列表中的当前选择不会打断已有会话，启动后才应用。
5. 根据网关与 Science 的健康检查结果判断是否就绪；需要时展开 **诊断与日志**。

不要把密钥、完整配置文件或含隐私内容的日志粘贴到公开 Issue。模型名称必须以供应商实际提供为准，模板推荐不等于当前账号一定可用。

## 关于官方连接器报错

如果 Science 显示：

> Directory connectors unavailable
>
> Your claude.ai session has expired. Sign in again to restore your directory connectors.

**研舟不会把这条提示当作已修复。**第三方模型连接与官方 claude.ai 会话是不同的授权链路。本版不提供官方账号登录，不同步浏览器 Cookie，也不伪造连接器授权；模型可以调用，不代表官方目录连接器可用。需要官方账号能力时，请使用官方提供的登录与连接器流程。

本版同时移除了 Science 中文切换、Codex 账号登录入口、官方模式切换和 Skill/MCP 管理界面。这里的“中文切换移除”指 Science 汉化功能；研舟自身仍使用中文界面。

## 本地数据与权限

- API Key 保存在本机 `~/.csswitch/config.json` 中，文件权限为 `0600`。**不是钥匙串加密存储**，同一用户权限下的程序仍可能读取它。
- 第三方 Science 使用隔离的运行目录；研舟不以读取官方账号凭据作为 API 配置流程的一部分。
- 本地网关使用回环地址。端口可在设置中修改，`8765` 保留给官方 Science；两个自定义端口不得相同。
- 系统 SSH 复用默认关闭。主动启用意味着授权隔离 Science 使用已有 SSH 配置与身份；研舟不复制整个 `.ssh`，也不启动 SSH 服务端。
- 批量删除连接不删除 Science 项目；清除密钥会使相关连接需要重新填写密钥。

![深色设置界面](docs/assets/sciport-settings-dark.png)

## 从源码构建

需要 macOS、Xcode Command Line Tools、Node.js/npm，以及带 Apple Silicon 目标的 Rust 工具链。

```bash
git clone https://github.com/Vonfre/claude-science-api.git
cd claude-science-api
git checkout v0.9.0
npm ci --prefix desktop
npm run tauri --prefix desktop -- build --bundles app
```

输出：`desktop/src-tauri/target/release/bundle/macos/SciPort.app`。构建过程会从同一源码构建 Gateway sidecar；不要手动拷入旧版 sidecar。

开发模式：

```bash
npm run tauri --prefix desktop -- dev
```

聚焦前端检查：

```bash
bash test/run-frontend.sh
```

完整源码门禁及隔离要求见[自动测试](docs/operations/testing.md)。源码测试、安装包验证、真实 Science、真实供应商调用和 Apple 公证是不同证据层。**本版不承诺所有供应商、官方连接器或科研任务均已实测通过。**

## 文档与反馈

- [功能与界面合同](docs/features/ui-information-architecture.md)
- [开发与构建](docs/operations/development.md)
- [发布流程](docs/operations/release.md)
- [更新记录](CHANGELOG.md)
- [提交问题](https://github.com/Vonfre/claude-science-api/issues)：请附版本、系统/芯片、复现步骤和脱敏错误信息。

## 致谢与许可证

本项目基于 [CSSwitch](https://github.com/SuperJJ007/CSswitch) 改造，保留上游版权声明与 [MIT 许可证](LICENSE.upstream)。本仓库原有 [Apache-2.0 许可证](LICENSE) 同样保留；新增贡献按仓库许可证提供，上游代码仍保留其 MIT 许可与归属。参见 [NOTICE](NOTICE)。Claude 和 Anthropic 相关名称归其各自权利人所有；本项目与 Anthropic 无隶属或官方背书关系。
