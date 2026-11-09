# Zed CN

[![Upstream: Zed](https://img.shields.io/badge/upstream-Zed-084CCF)](https://github.com/zed-industries/zed)
[![Build multiplatform release](https://github.com/rxp200/zed-cn/actions/workflows/build-windows-release.yml/badge.svg)](https://github.com/rxp200/zed-cn/actions/workflows/build-windows-release.yml)

Zed CN 是基于 [Zed](https://github.com/zed-industries/zed) 的**中文特化个人修改加强版**。项目保留 Zed 高性能、多人协作、原生 GPU 加速等核心能力，并针对简体中文界面、国内网络环境、远程开发、本地模型和个人工作流持续进行适配与增强。

> [!CAUTION]
> 本项目是非官方社区分支，与 Zed Industries, Inc. 没有隶属、赞助或背书关系。`Zed` 名称及相关标识归其权利人所有。需要官方版本、官方支持或最稳定的原始体验时，请访问 [zed.dev](https://zed.dev/) 或 [zed-industries/zed](https://github.com/zed-industries/zed)。

Zed 原项目是一款由 [Atom](https://github.com/atom/atom) 和 [Tree-sitter](https://github.com/tree-sitter/tree-sitter) 作者打造的高性能多人协作代码编辑器。

## 本分支的主要增强

- **中文优先的多语言界面**：菜单、设置、Git、终端、调试器、Agent 等用户界面默认使用简体中文，并通过统一语言目录支持在设置中切换简体中文和英文；命令面板、设置及最近项目还支持中文或拼音首字母搜索。
- **AI 翻译与代码讲解**：支持悬停文档、选区或光标词翻译，以及编辑器内代码讲解、深入讲解选区、项目预扫描和 Git Diff 讲解；模型、并发和持久化缓存均可配置。
- **本地及兼容模型**：改进 LM Studio、llama.cpp、OpenAI 兼容和 Anthropic 兼容接口的模型发现、能力配置、工具调用及编辑预测支持。
- **远程开发增强**：提供官方 Zed / Zed CN Remote Server 来源选择、国内网络下载与 SSH 上传优化、连接阶段详情、超时重试、无响应恢复、Zed 专属 SSH 密钥、双向端口转发和远程终端临时文件。
- **终端与窗口工作流**：终端标签可跟随程序标题并显示输出状态；编辑器和终端标签可拖到精简独立窗口，终端还可创建保留当前滚动历史的只读冻结页面。
- **本地与远程工具**：提供多语言当前文件/选区运行、HTML 浏览器实时预览，以及本机和匹配 Zed CN Remote Server 的 CPU、内存、磁盘、网络与端口转发监控。
- **编辑与可靠性改进**：包含长行及病态 Tree-sitter 解析保护、中文设置搜索、路径补全、Git 作者历史筛选，以及 Windows 启动、终端刷新和更新重启等可靠性修复。
- **自有发布与更新信息**：自定义 Stable 构建使用带 `rN` 修订号的版本身份、校验和与静态更新清单，并可在应用内查看对应的中文发布说明及历史记录。

增强功能会随上游变化持续维护，但不能保证每个平台、第三方模型服务或远程环境都经过完整验证，也不能保证个人构建具备官方发行版的签名、支持和在线服务。部分功能会向用户选择的模型提供商发送代码或文本，请在启用前确认项目可信、提供商适合处理相关内容，并结合设置中的模型与缓存选项控制使用范围。

## 安装

### Zed CN

预编译包可从本仓库的 [Releases](https://github.com/rxp200/zed-cn/releases) 获取：

| 平台 | 架构 | Release 产物 |
| --- | --- | --- |
| Windows | x86_64、aarch64 | `Zed-<架构>.exe` |
| Linux | x86_64、aarch64 | `zed-linux-<架构>.tar.gz` |
| macOS | Apple Silicon / aarch64 | `Zed-aarch64.dmg` |

当前不提供 Intel Mac 构建。Windows 安装包未使用受信任的代码签名，macOS 应用使用临时签名且未公证，因此 SmartScreen 或 Gatekeeper 可能显示警告。请确认 Release 来自本仓库，并使用同一 Release 中的 `SHA256SUMS.txt` 核对下载文件。发布流程允许其他平台在个别构建失败时继续发布，所以请以具体 Release 的资产列表和“未生成的产物”说明为准。

### 官方 Zed

如果你不需要本分支的中文特化与增强，macOS、Linux 和 Windows 用户可以从 [Zed 官方下载页](https://zed.dev/download) 下载，或使用对应平台的软件包管理器安装（[macOS](https://zed.dev/docs/installation#macos) / [Linux](https://zed.dev/docs/linux#installing-via-a-package-manager) / [Windows](https://zed.dev/docs/windows#package-managers)）。

目前尚不提供 Web 版本（[上游跟踪讨论](https://github.com/zed-industries/zed/discussions/26195)）。

## 使用与开发文档

- [界面语言与多语言目录](./docs/src/i18n.md)
- [AI 代码讲解](./docs/src/code-explanations.md)
- [运行当前文件或选区](./docs/src/tasks.md)
- [在 macOS 上构建 Zed](./docs/src/development/macos.md)
- [在 Linux 上构建 Zed](./docs/src/development/linux.md)
- [在 Windows 上构建 Zed](./docs/src/development/windows.md)

本仓库仍以 Zed 的上游文档为基础。仓库内多数文档描述官方 Zed；只有明确标注的页面才描述 Zed CN 特有行为。遇到分支特有问题时，请优先在本仓库反馈。

## 贡献

本项目欢迎与中文体验、国内网络环境、远程开发和上述增强有关的问题反馈及改进。仓库内的 [CONTRIBUTING.md](./CONTRIBUTING.md) 主要继承自上游，目前仍包含 Zed 官方的贡献流程；向本仓库提交改动前，请同时说明改动针对 Zed CN 还是适合提交给上游。

通用功能、跨地区问题或适合所有 Zed 用户的修复，建议优先向 [Zed 上游项目](https://github.com/zed-industries/zed) 提交。

## 上游与兼容性

本项目持续跟随 Zed 上游开发。Zed CN Stable Release 保留对应官方 Stable 的版本身份和祖先关系，同时包含本分支当前公开实现，因此功能集合可能领先于同版本官方 Stable。

SSH 主机可以选择官方 Zed 或与当前 Zed CN Release 精确匹配的 Zed CN Remote Server。官方服务端保留原有连接路径，但不保证支持系统监控、远程终端临时文件、服务端托管持久终端等 Zed CN 新增能力；相同应用版本号也不代表协议能力完全相同。需要这些能力时，请在 SSH 主机的“查看服务器选项”中选择 Zed CN 来源并重新连接。下载不会在来源、版本、架构或校验失败时静默回退到其他服务端，具体限制请以相应 Release 说明为准。

Zed CN Stable 使用本项目的静态更新清单发现自定义 Release。官方账号、云端协作、托管模型、扩展商店及其他 `zed.dev` 在线服务仍由 Zed 官方提供并受其条款约束；本项目不运营这些服务，也不改变第三方模型提供商各自的数据处理规则。

## 许可证与归属

本仓库继承 Zed 的开源许可结构：源代码主要使用 **GPL-3.0-or-later**，明确标注的组件使用 **Apache-2.0**。完整条款见 [LICENSE-GPL](./LICENSE-GPL) 和 [LICENSE-APACHE](./LICENSE-APACHE)；第三方依赖和资源仍受各自许可证约束。

本项目保留上游项目的版权与许可证声明。分支中的个人修改不改变原始代码、名称、商标及第三方内容的权利归属。修改版身份、修改起始时间、Apache-2.0 修改文件和对应源码说明见 [MODIFICATIONS.md](./MODIFICATIONS.md)。每个 GitHub Release 都会给出构建二进制所使用的精确源码提交；桌面安装包同时携带 GPL、Apache、修改声明和第三方许可证报告。

本 README 中对上游 Zed 的介绍仅用于说明项目来源，不代表本项目是 Zed 官方发行版。

仓库中保留的上游文档、法律文本、服务说明、贡献指南、维护者名单或链接描述的是 Zed 官方项目及其服务，除非文件明确注明为 Zed CN 内容；它们不代表相关组织或人员参与维护、赞助或支持 Zed CN。
