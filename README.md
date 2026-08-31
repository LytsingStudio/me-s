# ME

ME 是一个简单、轻量的本地 AI Agent 产品，尤其适合需要连续工作较长时间的任务。一个 ME 版本由三个职责独立的程序组成：

- `me-s`：面向单个工作区的直接入口，在当前目录启动 TUI 与 WebUI；
- `me-gateway`：面向多个工作区的 Web 工作台，负责打开、恢复和关闭各个工作区；
- `me-client`：可选桌面客户端，连接远程 `me-gateway`，使用与浏览器 WebUI 相同的产品界面。

ME 可以操作文件、运行命令、浏览网页、查看图片，并支持多个 Agent 协作。不同界面可以同时观察同一个运行中的工作区，实时同步会话、输入草稿和执行状态。

ME 本身也是开发者使用 me-s，通过 vibe coding 协作开发的项目。

> ME 仍处于早期阶段。功能和配置可能继续调整，请为重要项目保留独立备份。

## 特点

- **三种使用入口**：既可在项目目录直接运行 `me-s`，也可通过 `me-gateway` 管理多个工作区，或使用 `me-client` 连接 Gateway。
- **适合长任务**：可记录工作计划和进展，并在上下文接近上限时进行压缩。
- **多端同步**：`me-s` 同时提供 TUI 与 WebUI；Gateway WebUI 与 me-client 可从一个界面访问多个工作区。
- **多 Agent 协作**：支持普通会话，以及 Manager 与 Worker 协作完成复杂工作。
- **完整的本地工具**：支持终端、文件、网页、图片等常用操作。
- **真实浏览器**：可以访问实际网页，必要时允许用户临时接管浏览器。
- **可扩展工具**：可在工作区中加入自定义 Python 工具箱。
- **跨平台**：支持 macOS、Linux 和 Windows。

## 安装

预编译版本发布在 [GitHub Releases](https://github.com/LytsingStudio/me-s/releases)。每个平台安装包都包含同一版本的 `me-s`、`me-gateway` 和 `me-client`；引导脚本只下载当前平台的一个完整产品包与 `SHA256SUMS`，验证完整包后再调用系统安装器。

### macOS

```bash
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/LytsingStudio/me-s/s/install.sh | sh
```

一个通用安装包同时支持 Apple Silicon 与 Intel，固定安装到：

```text
/usr/local/bin/me-s
/usr/local/bin/me-gateway
/Applications/ME Client.app
```

安装过程中会请求管理员权限。macOS 产品包不支持通过 `ME_INSTALL_DIR` 更改安装位置。

### Linux

```bash
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/LytsingStudio/me-s/s/install.sh | sh
```

默认安装到：

```text
/usr/local/bin/me-s
/usr/local/bin/me-gateway
/usr/local/bin/me-client
```

x86_64 与 arm64 分别使用各自的完整 `.run` 包。`me-client` 是 AppImage 客户端入口；安装程序不会启动图形界面，因此无桌面环境的服务器仍可正常安装和使用 `me-s`、`me-gateway`。CLI 程序兼容 glibc 2.17 及以上系统。

如需安装到其他目录：

```bash
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/LytsingStudio/me-s/s/install.sh | ME_INSTALL_DIR="$HOME/.local/bin" sh
```

### Windows

在 PowerShell 中执行：

```powershell
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; irm https://raw.githubusercontent.com/LytsingStudio/me-s/s/install.ps1 | iex
```

x64 安装程序固定使用当前用户目录：

```text
%LOCALAPPDATA%\Programs\ME\me-s.exe
%LOCALAPPDATA%\Programs\ME\me-gateway.exe
%LOCALAPPDATA%\Programs\ME\me-client.exe
```

安装目录会加入用户 `PATH`，开始菜单会创建 ME Client 与卸载入口。如果当前窗口尚未识别命令，请重新打开终端。

安装 ME 不会覆盖或删除已有的 `me`/`me.exe`。旧 `me` 与当前 ME 可以同时存在，并共同使用现有的 me 全局配置目录和工作区格式。

### 从源码构建

构建两个 CLI 程序需要 Rust：

```bash
git clone https://github.com/LytsingStudio/me-s.git
cd me-s
cargo build --locked --release --bins
```

产物位于 `target/release/me-s` 与 `target/release/me-gateway`。构建桌面客户端还需要 Bun 与 Tauri 的平台依赖：

```bash
cd me-client
bun run build
bunx @tauri-apps/cli@2.11.3 build
```

## 首次初始化

三个程序共享同一份全局模型配置。首次使用前先执行：

```bash
me-s init
```

`me-gateway` 不提供首次初始化向导；全局配置不存在时会直接提示先运行 `me-s init`。

## 直接使用 me-s

`me-s` 保持单工作区语义：当前目录就是该进程唯一的工作区。

```bash
cd /path/to/project
me-s create                 # 也可以直接运行 me-s，再按提示创建
me-s model list
me-s model select MODEL_NAME
me-s
```

默认情况下，`me-s` 会同时启动 TUI 和 WebUI。WebUI 监听 `0.0.0.0`，从端口 `38199` 开始选择可用端口；本机可通过对应的 `127.0.0.1` 地址访问。

只启动 WebUI：

```bash
me-s --no-tui
```

为本次 WebUI 设置访问密码：

```bash
me-s --no-tui --webui-passkey "PASSWORD"
```

### 工作区与会话

在项目目录执行 `me-s create` 后，该目录就是一个 me 工作区。每个工作区可以包含多个独立会话，并保留各自的聊天记录、工作进展和设置。

TUI 与 WebUI 可以同时使用。在一个界面中发送消息、修改输入草稿或创建会话，其他已连接界面会同步更新。

## 使用 me-gateway

在一个准备作为 Gateway 状态根目录和内置“聊天”工作区的目录中启动：

```bash
mkdir -p "$HOME/me-workbench"
cd "$HOME/me-workbench"
me-gateway --webui-passkey "PASSWORD"
```

CLI 正常启动时只输出实际访问地址及 `warning:`/`error:` 调试诊断，不显示额外的运行说明。Gateway HTTP 服务监听 `0.0.0.0`，默认从端口 `38200` 开始选择可用端口。

Gateway WebUI 提供：

- 固定的“聊天”分组：使用 `me-gateway` 启动目录作为内置工作区；
- “工作”分组：新建或打开 Gateway 宿主机上的多个目录；
- 每个工作区内的会话新增、删除、消息、模型、推理强度、上下文、WorkMap、Terminal 和历史操作；
- 宿主机目录浏览器；
- 全局模型设置编辑器。

Gateway 状态保存在启动目录的 `.me-gateway/state.json`。外部工作区在 Gateway 正常退出后仍保留在活跃集合中，下次从同一目录启动时会按原顺序恢复。用户在侧边栏关闭工作区时，对应工作区会停止并从活跃集合中移除，但工作目录不会被删除。

浏览器页面只负责展示和控制。关闭、刷新或断开浏览器不会停止 Gateway、工作区、Agent 或工具；停止 `me-gateway` 进程才会正常关闭其管理的全部工作区。

### 宿主目录与远程访问

Gateway 的目录浏览器始终浏览运行 `me-gateway` 的宿主电脑，而不是访问 WebUI 的浏览器设备。浏览器可以运行在另一台电脑或手机上，无需安装 ME，也不会上传客户端目录。

`--webui-passkey` 只提供密码访问控制，不提供 TLS。通过局域网或公网访问时，请使用 HTTPS 反向代理、隧道、VPN 或其他适当的安全网络边界。

### 设置生效

Gateway 设置页直接编辑与 `me-s` 相同的全局模型配置。已有 inline API Key 会直接显示，可以在设置页中编辑、替换或明确清除。

保存设置不会热更新已经运行的工作区。重启 `me-gateway` 后，新启动的工作区会读取新配置。

## 使用 me-client

`me-client` 是可选的 Gateway 桌面前端。启动后输入目标 `me-gateway` 地址和访问密码即可连接：

- macOS：打开 `/Applications/ME Client.app`；
- Windows：从开始菜单打开 **ME Client**，或运行安装目录中的 `me-client.exe`；
- Linux：运行 `me-client`。

客户端只负责连接与呈现，不会启动本机 `me-s`、`me-gateway`、Agent、模型或工具。关闭、断开或崩溃都不会停止远端 Gateway 或工作区。不能安装客户端时，仍可直接使用 me-gateway 浏览器 WebUI。


## 工作模式

创建新会话时可以选择：

| 模式 | 适合场景 |
| --- | --- |
| `main-agent` | 默认模式。Agent 直接使用全部可用工具完成任务。 |
| `manager-agent` | 适合复杂任务。Manager 负责理解、规划和判断，Worker 协助完成具体操作。 |
| `chatbot` | 只进行普通对话，不使用工具。 |

直接运行 `me-s` 时，可用下面的命令查看或设置新会话默认模式：

```bash
me-s orch
me-s orch main-agent
```

已经创建的会话不会因为修改默认模式而改变。

## 长任务与工具

处理复杂工作时，Agent 可以维护目标、计划、发现和进度。当上下文接近模型上限时，Agent 会在合适的位置整理已有内容，以便继续工作。

ME 默认提供以下能力：

- 在真实终端中运行命令和交互式程序；
- 读取、创建、修改和删除文件；
- 浏览网页并在需要时保存页面截图；
- 查看图片；
- 创建和管理多个会话；
- 维护长任务的目标和计划。

如需扩展功能，可以在工作区的 `.me/tools/` 中加入兼容的 Python 工具箱。首次创建工作区时，`me-s` 会自动准备默认工具。

## TUI 与 WebUI

TUI 常用快捷键：

- `Tab`：切换会话、终端和其他页面；
- `Ctrl+O`：展开或收起工具详情；
- `Esc`：根据当前状态中止生成、撤回或清空输入框；
- `/model`：切换当前会话模型；
- `/effort`：切换推理强度；
- `/context`：查看上下文用量与组成；
- `/clear`：清空当前上下文；
- `/rewind`：回到之前的位置。

`me-s` WebUI、Gateway WebUI 与 me-client 直接复用同一权威前端核心，因此会话、渲染和交互行为保持一致；Gateway 只通过薄运行时适配增加多工作区与宿主管理入口。

## 常用 CLI

| 命令 | 说明 |
| --- | --- |
| `me-s` | 在当前目录启动单工作区 TUI 与 WebUI。 |
| `me-s --no-tui` | 只启动当前工作区 WebUI。 |
| `me-s --webui-passkey PASSWORD` | 为本次单工作区 WebUI 设置访问密码。 |
| `me-s init` | 初始化或重置全局配置。 |
| `me-s version` | 显示 `me-s` 当前版本。 |
| `me-gateway version` | 显示 `me-gateway` 当前版本。 |
| `me-s update` | 从公开 Release 更新完整 ME 产品。 |
| `me-gateway update` | 与 `me-s update` 等价，更新完整 ME 产品。 |
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
| `me-gateway [--webui-passkey PASSWORD]` | 启动多工作区 Web 工作台。 |

## 更新

以下命令语义等价：

```bash
me-s update
me-gateway update
```

更新器会解析最新公开 ME Release，选择当前系统与架构对应的一个完整产品包，下载 `SHA256SUMS` 并验证整包，然后调用平台安装器一起升级或修复 `me-s`、`me-gateway` 和 `me-client`。macOS 使用通用 pkg，Linux 使用对应架构的 `.run`，Windows 在发起命令的进程退出后静默运行 NSIS 安装器。

即使版本号已经是 latest，只要任一 CLI 缺失、两项 CLI 没有精确报告当前产品版本，或客户端文件缺失，update 都会执行同版本修复。安装与更新不会修改全局模型配置、凭据、工作区 `.me`、EDB 或 Gateway 远端资源。

## 配置模型

全局模型配置文件位于：

- macOS/Linux：`~/.config/me/conf.d/models.toml`
- 设置了 `XDG_CONFIG_HOME`：`$XDG_CONFIG_HOME/me/conf.d/models.toml`
- Windows：`%APPDATA%\me\conf.d\models.toml`
- 设置了 `ME_CONFIG_HOME`：`$ME_CONFIG_HOME/conf.d/models.toml`

建议通过环境变量或独立凭据文件保存 API Key，不要把密钥直接提交到版本库。OpenAI-compatible 模型示例：

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
```

保存后可以执行：

```bash
me-s model list
me-s model test example-model
me-s model select-default example-model
```

Codex OAuth 不需要手写预设。运行 `me-s codex login` 后，可用模型会自动出现在模型列表中。

## 开发

```bash
cargo fmt --all -- --check
cargo test --all-targets
bun test tests/gateway_webui.test.js tests/webui_*.test.js tests/me_client.test.js
sh tests/install_scripts.sh
```

跨平台发行资产完全由 Apple Silicon macOS 主机本地构建，不使用 GitHub Actions 或外部 Windows 构建机。首次初始化或依赖输入变化后，需要显式运行 `./build.sh --online`，并提供可联网的 Rust、Bun、Docker/Colima、LLVM、`cargo-xwin`、NSIS `makensis` 和 7-Zip 环境；初始化完成后，日常运行 `./build.sh` 默认严格离线，一次生成 macOS universal pkg、Windows x86_64 NSIS setup、Linux x86_64/arm64 `.run` 与 `SHA256SUMS`。

- macOS：原生构建 arm64/x86_64 CLI 与 Tauri universal App，再生成一个 pkg；
- Windows：通过 `cargo-xwin`/LLVM 交叉构建 MSVC ABI x64 的三个 PE 程序，再由 macOS `makensis` 生成 NSIS setup；
- Linux：首次通过 `./build.sh --online` 为两个架构初始化固定的本地 builder image，以及持久的 Cargo、target 和内嵌 Python runtime 缓存；builder image 预置 AppImage 工具及固定校验的 type-2 runtime，后续使用相同 Docker runtime 环境直接复用，不重复安装或下载系统、Rust、Zig、Bun、Tauri、Python/AppImage runtime 和项目依赖。

`./build.sh` 和 `./build.sh --offline` 都严格要求 host/Linux 工具、当前依赖集合及 AppImage runtime 已初始化：host Cargo 使用 offline，Linux 容器禁用网络；缺失任何资源都会直接失败，只有显式 `./build.sh --online` 才允许初始化或下载。所有模式均先在临时 staging 完成全部构建和静态验收，成功后才原子替换 `dist/`，因此只清理旧发行包，并保留 `.build-cache`、Cargo、xwin、Docker image/volume、内嵌 Python runtime 和依赖编译缓存。构建不会运行 Windows/Linux 目标程序、AppImage、`.run` 或安装器。

`./release.sh` 不执行任何构建或依赖初始化，只验证干净且已推送的 `s` 分支、`BUILD-MANIFEST.json` 与当前 commit、四包静态结构及 SHA-256，然后创建 tag 并上传现有 `dist/`。Release 恰好包含：

```text
ME-macos-universal.pkg
ME-windows-x86_64-setup.exe
ME-linux-x86_64.run
ME-linux-arm64.run
SHA256SUMS
```

## License

ME 使用 [MIT License](LICENSE)。
