# Zed CN

[![Upstream: Zed](https://img.shields.io/badge/upstream-Zed-084CCF)](https://github.com/zed-industries/zed)
[![Build Windows release](https://github.com/rxp200/zed-cn/actions/workflows/build-windows-release.yml/badge.svg)](https://github.com/rxp200/zed-cn/actions/workflows/build-windows-release.yml)

Zed CN 是基于 [Zed](https://github.com/zed-industries/zed) 的**中文特化个人修改加强版**。项目保留 Zed 高性能、多人协作、原生 GPU 加速等核心能力，并针对简体中文界面、国内网络环境、远程开发、本地模型和个人工作流持续进行适配与增强。

> [!CAUTION]
> 本项目是非官方社区分支，与 Zed Industries, Inc. 没有隶属、赞助或背书关系。`Zed` 名称及相关标识归其权利人所有。需要官方版本、官方支持或最稳定的原始体验时，请访问 [zed.dev](https://zed.dev/) 或 [zed-industries/zed](https://github.com/zed-industries/zed)。

Zed 原项目是一款由 [Atom](https://github.com/atom/atom) 和 [Tree-sitter](https://github.com/tree-sitter/tree-sitter) 作者打造的高性能多人协作代码编辑器。

## 本分支的主要增强

- **简体中文界面**：直接汉化菜单、设置、Git、终端、调试器、Agent 等大量用户可见文本，并提供中文命令名称。
- **AI 翻译**：支持悬停文档及选区/光标词翻译，可选择语言模型，并提供持久化缓存。
- **本地模型兼容**：改进 LM Studio、llama.cpp 及 OpenAI 兼容接口的模型识别、工具能力和编辑预测支持。
- **远程开发优化**：改善国内网络环境下的 Remote Server 下载与 SSH 上传、超时、重试、手动重连及终端端口转发体验。
- **编辑与性能改进**：针对超长行提供性能保护，并包含设置搜索、路径补全、Git 历史筛选等个人工作流增强。
- **Windows 适配**：维护个人使用的 Windows 安装包构建流程及若干 Windows 可靠性修复。

汉化和增强会随上游变化持续维护，但不能保证所有界面在任何时刻都已完整翻译，也不能保证个人构建具备官方发行版的签名、支持和更新服务。

## 安装

### Zed CN

Windows 构建可从本仓库的 [Releases](https://github.com/rxp200/zed-cn/releases) 获取。个人发布的安装包目前可能没有受信任的代码签名，因此 Windows SmartScreen 可能显示警告；请在核对发布来源和校验值后自行决定是否安装。

macOS 和 Linux 暂未提供稳定的 Zed CN 预编译包，可按照下方开发文档从源码构建。

### 官方 Zed

如果你不需要本分支的中文特化与增强，macOS、Linux 和 Windows 用户可以从 [Zed 官方下载页](https://zed.dev/download) 下载，或使用对应平台的软件包管理器安装（[macOS](https://zed.dev/docs/installation#macos) / [Linux](https://zed.dev/docs/linux#installing-via-a-package-manager) / [Windows](https://zed.dev/docs/windows#package-managers)）。

目前尚不提供 Web 版本（[上游跟踪讨论](https://github.com/zed-industries/zed/discussions/26195)）。

## 从源码构建

- [在 macOS 上构建 Zed](./docs/src/development/macos.md)
- [在 Linux 上构建 Zed](./docs/src/development/linux.md)
- [在 Windows 上构建 Zed](./docs/src/development/windows.md)

本仓库仍以 Zed 的上游构建文档为基础；遇到分支特有问题时，请优先在本仓库反馈。

## 贡献

本项目欢迎与中文体验、国内网络环境、远程开发和上述增强有关的问题反馈及改进。仓库内的 [CONTRIBUTING.md](./CONTRIBUTING.md) 主要继承自上游，目前仍包含 Zed 官方的贡献流程；向本仓库提交改动前，请同时说明改动针对 Zed CN 还是适合提交给上游。

通用功能、跨地区问题或适合所有 Zed 用户的修复，建议优先向 [Zed 上游项目](https://github.com/zed-industries/zed) 提交。

## 上游与兼容性

本项目会跟随 Zed 上游开发，并在发布安装包时尽量基于对应的官方 Stable 源码。部分远程开发增强可能要求客户端与 Remote Server 版本或实现相匹配，请以具体 Release 说明为准。

官方账号、云端协作、托管模型、扩展商店、自动更新及其他 `zed.dev` 在线服务仍由 Zed 官方提供并受其条款约束，本项目不运营这些服务。

## 许可证与归属

本仓库继承 Zed 的开源许可结构：源代码主要使用 **GPL-3.0-or-later**，明确标注的组件使用 **Apache-2.0**。完整条款见 [LICENSE-GPL](./LICENSE-GPL) 和 [LICENSE-APACHE](./LICENSE-APACHE)；第三方依赖和资源仍受各自许可证约束。

本项目保留上游项目的版权与许可证声明。分支中的个人修改不改变原始代码、名称、商标及第三方内容的权利归属。再分发二进制文件前，请自行确认许可证、第三方声明、商标、代码签名及在线服务条款等要求。

本 README 中对上游 Zed 的介绍仅用于说明项目来源，不代表本项目是 Zed 官方发行版。

仓库中保留的上游文档、法律文本、服务说明、贡献指南、维护者名单或链接描述的是 Zed 官方项目及其服务，除非文件明确注明为 Zed CN 内容；它们不代表相关组织或人员参与维护、赞助或支持 Zed CN。
