# ME-S

ME-S（命令名 `me-s`）是一个简单、轻量的本地 AI Agent，尤其适合需要连续工作较长时间的任务。

它可以在终端或浏览器中使用，能够操作文件、运行命令、浏览网页、查看图片，并支持多个 Agent 协作。不同界面可以同时连接到同一个运行中的 me-s，实时同步会话、输入草稿和执行状态。

ME-S 本身也是开发者与 Codex 通过 vibe coding 协作完成的项目。

> ME-S 仍处于早期阶段。功能和配置可能继续调整，请为重要项目保留独立备份。

## 特点

- **简单轻量**：安装一个可执行文件即可开始使用。
- **适合长任务**：可记录工作计划和进展，并在上下文接近上限时进行压缩。
- **多端同步**：同时提供 TUI 与 WebUI，多个界面可实时同步。
- **多 Agent 协作**：支持普通子 Agent，也支持 Manager 与 Worker 协作完成复杂工作。
- **完整的本地工具**：支持终端、文件、网页、图片等常用操作。
- **真实浏览器**：可以访问实际网页，必要时允许用户临时接管浏览器。
- **可扩展工具**：可在工作区中加入自定义 Python 工具箱。
- **跨平台**：支持 macOS、Linux 和 Windows。

## 安装

预编译文件发布在 [GitHub Releases](https://github.com/LytsingStudio/me-s/releases)。安装脚本会自动识别系统与处理器架构，并在安装前校验下载文件。

### macOS

在终端中执行：

```bash
curl --proto '=https' --tlsv1.2 -fsSL https://github.com/LytsingStudio/me-s/releases/latest/download/install.sh | sh
```

默认安装到 `/usr/local/bin/me-s`，需要时会请求管理员权限。如需安装到其他目录：

```bash
curl --proto '=https' --tlsv1.2 -fsSL https://github.com/LytsingStudio/me-s/releases/latest/download/install.sh | ME_INSTALL_DIR="$HOME/.local/bin" sh
```

### Linux

```bash
curl --proto '=https' --tlsv1.2 -fsSL https://github.com/LytsingStudio/me-s/releases/latest/download/install.sh | sh
```

默认安装到 `/usr/local/bin/me-s`。Linux 版本兼容 glibc 2.17 及以上系统。

### Windows

在 PowerShell 中执行：

```powershell
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; irm https://github.com/LytsingStudio/me-s/releases/latest/download/install.ps1 | iex
```

脚本默认安装到 `%LOCALAPPDATA%\Programs\me-s\me-s.exe`，并把该目录加入用户 `PATH`。如果当前窗口尚未识别 `me-s` 命令，请重新打开终端。

安装 me-s 不会覆盖或删除已有的 `me` 可执行文件；`me` 与 `me-s` 可以同时存在，并共同使用现有的 me 全局配置目录和工作区格式。

### 从源码构建

需要安装 Rust：

```bash
git clone https://github.com/LytsingStudio/me-s.git && cd me-s && cargo build --locked --release
```
构建产物位于 `target/release/me-s`。

## 快速开始

```bash
# 初始化全局配置
me-s init

# 在项目目录中创建工作区
cd /path/to/project
me-s create

# 查看并选择模型
me-s model list
me-s model select MODEL_NAME

# 启动 me-s
me-s
```

直接在普通目录中运行 `me-s` 时，程序也会询问是否立即创建工作区。

默认情况下，`me-s` 会同时启动 TUI 和 WebUI。WebUI 可通过 `http://127.0.0.1:38199` 访问；如果端口已被占用，程序会自动选择后续可用端口。

只启动 WebUI：

```bash
me-s --no-tui
```

为 WebUI 设置访问密码：

```bash
me-s --no-tui --webui-passkey "PASSWORD"
```

WebUI 可被局域网中的其他设备访问。访问密码不会加密 HTTP 流量；通过公网使用时，请另外配置 HTTPS 和适当的网络访问限制。

## 如何使用

### 工作区与会话

在项目目录执行 `me-s create` 后，该目录就是一个 me 工作区。每个工作区可以包含多个独立会话，并保留各自的聊天记录、工作进展和设置。

TUI 与 WebUI 可以同时使用。在一个界面中发送消息、修改输入草稿或创建会话，其他已连接界面会同步更新。

### 工作模式

创建新会话时可以选择工作模式：

| 模式 | 适合场景 |
| --- | --- |
| `main-agent` | 默认模式。Agent 直接使用全部可用工具完成任务。 |
| `manager-agent` | 适合复杂任务。Manager 负责理解、规划和判断，Worker 协助完成具体操作。 |
| `chatbot` | 只进行普通对话，不使用工具。 |

使用下面的命令查看或设置新会话默认采用的模式：

```bash
me-s orch
me-s orch main-agent
```

已经创建的会话不会因为修改默认模式而改变。

### 长任务

处理复杂工作时，Agent 可以维护目标、计划、发现和进度。即使任务持续较长时间，也能继续参考已有结论和未完成事项。

当上下文接近模型上限时，Agent 会在合适的位置整理已有内容，以便继续工作。你可以在界面中查看当前上下文用量和整理结果。

### 工具

me-s 默认提供以下能力：

- 在真实终端中运行命令和交互式程序；
- 读取、创建、修改和删除文件；
- 浏览网页并在需要时保存页面截图；
- 查看图片；
- 创建子 Agent 协助工作；
- 维护长任务的目标和计划。

如需扩展功能，可以在工作区的 `.me/tools/` 中加入兼容的 Python 工具箱。首次创建工作区时，me-s 会自动准备默认工具。

## TUI 与 WebUI

TUI 常用快捷键：

- `Tab`：切换会话、终端和其他页面。
- `Ctrl+O`：展开或收起工具详情。
- `Esc`：根据当前状态中止生成、撤回或清空输入框。
- `/model`：切换当前会话模型。
- `/effort`：切换推理强度。
- `/context`：查看上下文用量与组成。
- `/clear`：清空当前上下文。
- `/rewind`：回到之前的位置。

WebUI 提供对应的会话、模型、上下文、终端和多 Agent 管理功能，同时支持桌面与移动端布局。工具卡片可以直接点击展开。

## 常用 CLI

| 命令 | 说明 |
| --- | --- |
| `me-s` | 启动 TUI 与 WebUI。 |
| `me-s --no-tui` | 只启动 WebUI。 |
| `me-s --webui-passkey PASSWORD` | 为本次 WebUI 设置访问密码。 |
| `me-s init` | 初始化或重置全局配置。 |
| `me-s version` | 显示当前版本。 |
| `me-s update` | 直接从公开 Release 更新当前 me-s 可执行文件，不需要 `gh`，且不修改全局配置或已有的 `me`。 |
| `me-s create` | 在当前目录创建工作区。 |
| `me-s workspace reset` | 永久删除当前目录的 me 工作区数据。 |
| `me-s model list` | 列出可用模型。 |
| `me-s model select NAME [EFFORT]` | 为当前工作区选择模型。 |
| `me-s model select-default NAME` | 设置全局默认模型。 |
| `me-s model test NAME` | 测试模型连接和响应速度。 |
| `me-s model export PASSWORD` | 加密导出全部模型配置和凭据。 |
| `me-s model import FILE PASSWORD` | 导入模型配置和凭据。 |
| `me-s codex login` | 登录 Codex OAuth。 |
| `me-s codex status` | 查看 Codex OAuth 登录状态。 |
| `me-s codex logout` | 退出 Codex OAuth。 |
| `me-s orch [NAME]` | 查看或设置默认工作模式。 |

运行 `me-s --help` 可查看完整命令列表。

## 配置模型

全局模型配置文件位于：

- macOS/Linux：`~/.config/me/conf.d/models.toml`
- 设置了 `XDG_CONFIG_HOME`：`$XDG_CONFIG_HOME/me/conf.d/models.toml`
- Windows：`%APPDATA%\me\conf.d\models.toml`
- 设置了 `ME_CONFIG_HOME`：`$ME_CONFIG_HOME/conf.d/models.toml`

建议通过环境变量或独立凭据文件保存 API Key，不要把密钥直接提交到版本库。下面是一个 OpenAI-compatible 模型示例：

```toml
version = 1
default_model = "example-model"

[[models]]
name = "example-model"
provider = "openai-compatible"
reserve_output_context = true
base_url = "https://api.example.com/v1"
endpoint = "/chat/completions"
api_key_env = "EXAMPLE_API_KEY"
model = "provider-model-name"
timeout_seconds = 120

[models.capabilities]
context_window = 131072
max_output_tokens = 32768
input_modalities = ["text", "image"]
output_modalities = ["text"]
reasoning_modes = ["none", "thinking"]
reasoning_efforts = ["low", "high"]
tools = true
streaming = true

[models.parameters]
max_tokens = 32768

[models.effort_parameters.low]
reasoning_effort = "low"

[models.effort_parameters.high]
reasoning_effort = "high"
```

请根据模型服务商的文档准确填写上下文长度、最大输出、输入类型和推理强度。`reserve_output_context = true` 表示在上下文窗口中为请求的最大输出量预留空间；设为 `false` 或省略时不预留。保存后可以执行：

```bash
me-s model list
me-s model test example-model
me-s model select-default example-model
```

Codex OAuth 不需要手写预设。运行 `me-s codex login` 后，可用模型会自动出现在模型列表中。模型导出包会加密包含 Codex OAuth 凭据；导入时仅在当前设备尚未登录 Codex 时恢复，不会覆盖已有登录。

## 开发

```bash
cargo fmt --check
cargo test --locked
bun test tests/webui_markdown.test.js tests/webui_projection.test.js
```

项目的跨平台 Release 由仓库中的 `release.sh` 在本地构建并发布。

## License

ME-S 使用 [MIT License](LICENSE)。
