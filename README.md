# aido

把文件、剪贴板或管道里的内容发给任意 **OpenAI 兼容**的模型，结果写回终端或剪贴板。

```
截图 → Ctrl+C → aido ocr --copy → Ctrl+V

aido ocr screenshot.png

git diff | aido code-review
```

适用于 OpenAI / vLLM / SGLang / llama.cpp / Ollama / LM Studio / 智谱 等一切暴露 `/v1/chat/completions` 的服务。

## 安装

从 [Releases](https://github.com/appstore/aido/releases) 下载对应平台的压缩包，解压即用（Linux / macOS / Windows 二进制由 CI 自动构建，单个可执行文件，无 runtime 依赖）。

或从源码构建（需要 Rust 1.88+）：

```bash
cargo install --path .
```

或从源码构建发布版（产物为单个静态二进制，无 runtime 依赖）：

```bash
cargo build --release
# 二进制位于 target/release/aido
```

## 快速开始

```bash
# 1. 生成示例配置
aido --init

# 2. 设置 API key（推荐环境变量，避免明文落盘）
export OPENAI_API_KEY=sk-...

# 3. 用起来：剪贴板文本 → 整理 → 写回剪贴板
aido -p "帮我润色这段文字" --copy
```

**输入优先级：文件路径 > 管道 stdin > 剪贴板。** 剪贴板里是文本就当文本发，是图片就走 vision 模型（自动转 PNG → base64 → `image_url`）。

## 常用场景

```bash
# 截图 OCR（剪贴板中的图片发给 vision 模型）
aido ocr --copy

# 翻译（英文→中文，其他语言→英文）
aido translate --copy

# Code review
git diff | aido code-review

# 摘要
aido summarize

# 文件输入（文本或图片；须搭配 action 或 -p，可以一次给多个，文本和图片可混用）
aido ocr screenshot.png
aido -p "总结一下" notes.md
aido code-review diff.txt screenshot.png

# 本地模型（vLLM / SGLang / llama.cpp / Ollama / LM Studio）
git diff | aido code-review --base-url http://localhost:30000 -m qwen3

# 管道传图片（无需剪贴板，可用于无头环境）
aido < screenshot.png ocr --copy

# 指定完整 prompt
pbpaste | aido -p "改成正式商务邮件语气" --copy
```

> **Action 语法**：preset 名直接作为第一个参数（`aido ocr`），等价于 `aido --preset ocr`，后面可接任意
> flags 和文件路径。注意 action 名必须最先出现——`aido --copy ocr` 会把 `ocr` 当成文件名，报错并提示
> action 应放在最前。临时指令一律用 `-p/--prompt` 传递。

> **关于图片输入**：图片以 `image_url`（base64 PNG）形式发送，必须搭配支持视觉的模型
> （如 gpt-4o、glm-4.6v、qwen2.5-vl），纯文本模型无法处理图片。剪贴板中的图片自动走此
> 路径；文件和 stdin 支持 PNG 和 JPEG（JPEG 会自动转 PNG）。文本与图片文件可混用，
> 会合并进同一条 user 消息（多个文本文件按文件名分节）。
>
> **长图（滚动截屏）**：视觉服务端会把超限图片等比压缩（OpenAI 约定长边 2048px，
> Qwen-VL 系有 `max_pixels` 上限），长图整张发送会被压到文字不可读、OCR 丢行。
> aido 会把高超过 3072px 的竖长图自动切成若干竖条（切缝优先落在无内容的空白行），
> 逐条请求后按顺序拼接结果；`--no-split` 可关闭该行为。

## 命令行参数

| 参数 | 说明 |
|---|---|
| `<ACTION>` | 第一个参数位：运行一个 preset（如 `aido ocr`），等价于 `--preset`；action 名必须最先出现 |
| `<FILE>...` | 输入文件（须搭配 action 或 `-p`，位置任意）：文本原样发送，PNG/JPEG 图片走 vision 模型，可一次传多个混用；单个文件超过 32 MB 直接报错 |
| `-p, --prompt <PROMPT>` | system 指令（临时任务用这个）；输入内容作为 user 消息发送 |
| `--preset <NAME>` | 使用 prompt 预设（`aido list` 查看），与 `<ACTION>` 写法等价 |
| `--profile <NAME>` | 使用配置文件中的 profile |
| `-m, --model <MODEL>` | 模型名 |
| `--base-url <URL>` | 接口地址；无路径时自动补 `/v1` |
| `--api-key <KEY>` | API key（⚠️ 会进入 shell 历史和 `ps` 输出，日常请用环境变量） |
| `-o, --output <MODE>` | `stdout`（默认）/ `clipboard` / `both` |
| `-c, --copy` | `--output clipboard` 的简写 |
| `--max-tokens <N>` | 默认 8192；传 `0` 表示完全不发送该字段 |
| `--temperature <T>` | 采样温度 |
| `--timeout <SECS>` | 请求超时，默认 120 秒 |
| `--no-spinner` | 关闭 stderr 上的等待动画 |
| `--no-split` | 长图不切片，整张发送（默认自动切） |
| `--list-presets` | 列出所有 preset（同 `aido list`） |
| `--init` | 生成示例配置文件 |

结果只写 stdout；进度、警告、错误一律走 stderr，退出码非 0 表示失败——可以放心接管道。

模型返回空内容时：`stdout` 模式只输出一条警告；`clipboard` / `both` 模式直接失败退出，**不会**用空串覆盖剪贴板里的原始内容。

## 配置文件

| 平台 | 路径 |
|---|---|
| Linux | `~/.config/aido/config.toml` |
| macOS | `~/Library/Application Support/aido/config.toml` |
| Windows | `%APPDATA%\aido\config.toml` |

（环境变量 `AIDO_CONFIG` 可指定其他路径；指定的文件必须存在，否则启动即报错。）

```toml
default_profile = "default"

[settings]
# output = "clipboard"      # stdout | clipboard | both
# timeout_secs = 120
# hold_secs = 45             # Linux: 写入剪贴板后保活秒数

[profiles.default]
base_url = "https://api.openai.com/v1"
model = "gpt-4o-mini"

[profiles.local]
base_url = "http://localhost:30000"
model = "qwen3"

[profiles.zhipu]
base_url = "https://open.bigmodel.cn/api/paas/v4"
model = "glm-4.6"
```

取值优先级：**命令行参数 > 环境变量 > profile > 默认值**。

## 环境变量

| 变量 | 说明 |
|---|---|
| `AIDO_CONFIG` | 配置文件路径（必须指向已存在的文件） |
| `AIDO_PROFILE` | 默认 profile |
| `AIDO_BASE_URL` / `OPENAI_BASE_URL` | 接口地址 |
| `AIDO_API_KEY` / `OPENAI_API_KEY` | API key |
| `AIDO_MODEL` | 模型名 |
| `AIDO_MAX_TOKENS` | 最大生成 token 数（`0` 表示不发送该字段） |
| `AIDO_TEMPERATURE` | 采样温度 |
| `AIDO_PRESETS_DIR` | 自定义 preset 目录 |

> ⚠️ 安全提示：避免在命令行用 `--api-key` 传 key——参数会进入 shell 历史和 `ps` 进程列表，优先使用环境变量。

## Presets

内置 4 个：`ocr`、`translate`、`summarize`、`code-review`（`aido list` 查看）。

自定义 preset：在配置目录下的 `presets/` 放 `NAME.toml`（文件名即 preset 名，Linux 为 `~/.config/aido/presets/`，其他平台见上文配置路径表；可用 `AIDO_PRESETS_DIR` 指定其他目录）。例如润色工具 `polish.toml`：

```toml
system = """
你是中文写作助手。润色用户提供的文字：
- 修正错别字和标点
- 让表达更流畅自然，但不改变原意和语气
- 保留 markdown 格式；代码块内容原样保留
- 只输出润色后的文字
"""
```

使用：`aido polish --copy`。

写 preset 的要点：

- preset 就是 **system 指令**，剪贴板/管道/文件内容永远是 user 消息——写“要求模型做什么”，待处理内容不要写进去。
- 指令末尾加一条“只输出 X、不加解释”之类的收尾约束，能显著减少模型废话。
- 多行文本用 TOML 三引号字符串 `"""..."""`，内容中不能出现连续三个双引号。
- 与内置 preset 同名的文件会覆盖它，如自定义 `translate.toml` 即可修改默认翻译目标语言。
- 文件名即 action 名（`aido polish`），因此 `list` / `help` 是保留名，且不能以 `-` 开头。
- 格式非法的文件只会在 stderr 给一条 warning 并被忽略，不影响其他 preset。
- `aido list` 可随时核对最终生效的完整列表。

## Linux 剪贴板说明

X11（以及多数 Wayland 合成器）的剪贴板内容依附于写入它的进程，进程退出后内容即失效。aido 写剪贴板时会自动派生一个后台子进程，把内容保持一段时间（默认 45 秒，`settings.hold_secs` 可调），行为与 `xclip` / `wl-copy` 一致。

## 发布流程

push（或合并）到 `release` 分支会触发 [GitHub Actions](.github/workflows/release.yml)：跑测试，构建 Linux / macOS（Intel + Apple Silicon）/ Windows 二进制，打 `v<version>` 标签并发布到 [Releases](https://github.com/appstore/aido/releases)。

1. 在 `Cargo.toml` 中更新 `version`
2. 把代码合入 `release` 分支并 push
3. 等待 CI 完成，Release 页即出现对应产物

版本号不变而重复 push 时，同一个 `v<version>` Release 会滚动更新：标签移到最新 commit，同名产物被替换。手动触发（workflow_dispatch）只构建不发布，可用于验证 CI 配置。

## 已知限制 / Roadmap

- 暂无 streaming 输出（长回复期间只有 stderr spinner）
- 部分 OpenAI 新模型不接受 `max_tokens` 参数名，需要 `--max-tokens 0` 略过
- 可选方向：shell 补全、`--profile` 列表查看、热键常驻模式
