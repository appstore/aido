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

# 找回上次的结果（剪贴板被覆盖也不怕，见「结果留底」）
aido last
aido last --copy              # 直接塞回剪贴板

# 结果另存一份到指定文件（与任意 --output 模式可叠加）
aido ocr --copy --save /tmp/ocr.txt

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

# 流式输出（默认开启，长回复实时可见；--no-stream 改回一次性请求）
aido -p "帮我写一版周报"
```

> **Action 语法**：action（即 preset 名，`aido list` 查看）直接作为第一个参数，`aido ocr` 等价于
> `aido --preset ocr`，后面可接任意 flags 和文件路径。action 名必须最先出现——只有当它不方便
> 放第一位时才需要显式写 `--preset`（如 `aido --no-spinner --preset ocr`）。`aido --copy ocr`
> 会把 `ocr` 当成文件名，报错并提示 action 应放在最前。临时指令一律用 `-p/--prompt` 传递。

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
| `<ACTION>` | 第一个参数位：运行一个 action（preset 名，`aido list` 查看），如 `aido ocr`；action 名必须最先出现 |
| `<FILE>...` | 输入文件（须搭配 action 或 `-p`，位置任意）：文本原样发送，PNG/JPEG 图片走 vision 模型，可一次传多个混用；单个文件超过 32 MB 直接报错 |
| `-p, --prompt <PROMPT>` | system 指令（临时任务用这个）；输入内容作为 user 消息发送 |
| `--preset <NAME>` | 显式指定 action，等价于把名字放第一位；仅当 action 名不便最先出现时需要（如 `aido --no-spinner --preset ocr`），与 `-p` 互斥 |
| `--profile <NAME>` | 使用配置文件中的 profile |
| `-m, --model <MODEL>` | 模型名 |
| `--base-url <URL>` | 接口地址；无路径时自动补 `/v1` |
| `--api-key <KEY>` | API key（⚠️ 会进入 shell 历史和 `ps` 输出，日常请用环境变量） |
| `-o, --output <MODE>` | `stdout`（默认）/ `clipboard` / `both` |
| `-c, --copy` | `--output clipboard` 的简写 |
| `--save <FILE>` | 结果同时写入指定文件（父目录不存在会自动创建），可与任意 `--output` 模式叠加，对 `aido last` 同样生效 |
| `--max-tokens <N>` | 默认 8192；传 `0` 表示完全不发送该字段 |
| `--temperature <T>` | 采样温度 |
| `--timeout <SECS>` | 请求超时，默认 120 秒；流式下作用于首包等待和相邻数据间隔，不限制整条回复时长 |
| `--no-spinner` | 关闭 stderr 上的等待动画 |
| `--stream` / `--no-stream` | 流式输出（SSE，**默认开启**）：token 实时写到 stdout；纯剪贴板输出时静默流式，不实时打印 |
| `--no-split` | 长图不切片，整张发送（默认自动切） |
| `--adapter <NAME>` | 协议适配器，通常在 Profile 中配置 |
| `--input-mode` / `--output-mode` | 输入类型约束 / 期望输出类型，支持逗号分隔 |
| `--option KEY=VALUE` | 协议专有参数，可重复传入 |
| `--text <TEXT>` | 显式文字输入 |
| `--save-dir <DIR>` | 保存多个输出产物 |
| `--list-presets` | 列出所有 preset（同 `aido list`） |
| `--init` | 生成示例配置文件 |

结果只写 stdout；进度、警告、错误一律走 stderr，退出码非 0 表示失败——可以放心接管道。

请求默认走 SSE 流式：token 一到就实时写到 stdout（第一个 token 到达时 stderr 的 spinner 自动让位），长回复不再干等；`both` 模式下剪贴板仍在结束时一次性写入，接管道时最终字节与一次性请求完全一致。纯 `--copy` 运行同样走流式，但不实时打印——stderr 的 spinner 会就地更新已收字数（`⠙ asking glm-4.6... 1,204 chars`），推理类模型在输出正文前计数保持不动。`--timeout` 在流式下的语义是「等待响应头」和「相邻两次数据的间隔」，而非整条回复的总时长——慢模型的长回复不会被中途掐断。个别服务端会忽略 `stream: true` 而直接回普通 JSON，aido 会照常解析。要改回一次性请求：`--no-stream` 或配置 `stream = false`。

模型返回空内容时：`stdout` 模式只输出一条警告；`clipboard` / `both` 模式直接失败退出，**不会**用空串覆盖剪贴板里的原始内容。

## 结果留底

剪贴板里的结果一旦没及时粘贴、又被新内容覆盖，就丢了——重跑一遍既费时又费 token。因此每次成功运行的**非空结果**都会在本地留一份：

- `aido last`：把最近一条结果打到 stdout（可接管道/重定向）
- `aido last --copy`：把最近一条结果直接塞回剪贴板（`aido -c last`、`-o clipboard/both` 以及 config 的 `output` 设置同样生效）
- `--save <FILE>`：本次结果另存到指定文件（与 `--copy` 等模式叠加生效，对 `aido last` 同样可用）

历史目录与保留策略：

| 平台 | 路径 |
|---|---|
| Linux | `~/.local/share/aido/history/` |
| macOS | `~/Library/Application Support/aido/history/` |
| Windows | `%LOCALAPPDATA%\aido\history\` |

- 文件名为 UTC 时间戳（如 `20260909-153012.123.txt`），内容即模型回复原文
- 默认保留最近 50 条，旧的自动清理；`settings.history_keep` 可调，设为 `0` 关闭留底
- 留底失败（如目录不可写）只会在 stderr 给一条 warning，不影响本次运行
- 注意：留底的是**明文**。若不希望结果落在磁盘上，把 `history_keep` 设为 `0`，或用 `AIDO_HISTORY_DIR` 指向内存盘等位置
- Unix 下新建的历史目录权限为 `0700`、文件为 `0600`，避免多用户系统上被其他账号读取

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
# output = "stdout"        # stdout | clipboard | both
# stream = true              # SSE 流式请求（默认开启）；false = 一次性缓冲请求
# timeout_secs = 120
# hold_secs = 45             # Linux: 写入剪贴板后保活秒数
# history_keep = 50          # 磁盘留底条数（0 关闭），见「结果留底」

[profiles.default]
base_url = "https://api.openai.com/v1"
model = "gpt-4o-mini"
# api_key = "sk-..."       # 建议走环境变量 AIDO_API_KEY / OPENAI_API_KEY

[profiles.local]
base_url = "http://localhost:30000"
model = "qwen3"

[profiles.zhipu]
base_url = "https://open.bigmodel.cn/api/paas/v4"
model = "glm-4.6"
```

取值优先级：**命令行参数 > preset 自带参数 > 环境变量 > profile > 默认值**（preset 自带参数见下文 Presets 一节）。

## Profile、适配器与多模态输出

Profile 是完整的调用配置，包含服务地址、协议适配器、模型、认证及默认模式。
省略 `adapter` 时仍使用 `openai-chat`，原有配置和文本命令继续有效。

| adapter | 输入类型 | 可请求的输出类型 | 默认输出 |
|---|---|---|---|
| `openai-chat` | text、image，可组合 | text | text |
| `openai-responses` | text、image，可组合 | text、image，可组合 | text |
| `openai-speech` | text | audio | audio |
| `openai-transcription` | 单个 audio | text | text |
| `openai-images` | text | image，可有多张 | image |

表格表示适配器已实现的能力，具体服务和模型还必须支持所选模式。
Responses 的 image 输出会启用并选择 `image_generation` 工具；不会自动切换协议。
Responses 显式发送 `store: false`，本地历史仍由 `history_keep` 控制。
Chat/Responses 支持文本流式输出；图片在完整产物到达后保存。独立媒体适配器使用缓冲请求；显式 `--stream` 会报错，默认的流式设置不会影响它们。

```toml
[profiles.general]
base_url = "https://api.openai.com/v1"
adapter = "openai-responses"
model = "gpt-4o-mini"

[profiles.speech]
adapter = "openai-speech"
model = "tts-1"
[profiles.speech.options]
voice = "alloy"
format = "mp3"

[profiles.transcription]
adapter = "openai-transcription"
model = "whisper-1"

[profiles.images]
adapter = "openai-images"
model = "gpt-image-1"
```

认证继续使用 `AIDO_API_KEY` / `OPENAI_API_KEY`，也可在 Profile 中设置 `api_key`。
使用媒体任务时选择对应 Profile，避免继承文本模型的配置：

```bash
aido ocr scan.png --profile general
echo "你好" | aido tts --profile speech --save hello.mp3
aido transcribe meeting.m4a --profile transcription | aido summarize
aido image --profile images --text "一只柴犬" --save dog.png

# 不用任务预设也可以调用
echo "你好" | aido --profile speech --output-mode audio --save hello.mp3

# 多张图片保存到目录
aido image --profile images --text "一只柴犬" --option n=2 --save-dir dogs

# Responses 同时生成文本和图片；需选择支持图片工具的模型
aido --profile general -m gpt-4.1 --text "画一只柴犬并简要说明" \
  --output-mode text,image --save-dir dog-result

# 恢复最近一次媒体结果，无需重新调用模型
aido --save-dir recovered last
```

- `--input-mode text,image`：约束允许的输入类型，可重复传入；不进行类型转换。默认按内容检测文本、PNG/JPEG/WebP 图片及 WAV/MP3/FLAC/Ogg/M4A/WebM 音频容器。原始 PCM 输入暂不支持；音频容器检测不保证其中的编解码器受到远端支持。
- `--output-mode text,image`：请求生成的类型，可重复传入；省略时使用 Preset / Profile / Adapter 默认值。
- `--option KEY=VALUE`：适配器参数。值可为字符串或 JSON 标量，例如 `voice=alloy`、`speed=1.2`、`n=2`。不支持的参数会在请求前报错。
- `--text TEXT`：显式输入文字，优先于管道和剪贴板，与文件输入互斥。
- `--save FILE`：保存一个产物；媒体格式由 `--option format=...` 选择，文件扩展名必须匹配，不自动转码。
- `--save-dir DIR`：保存多产物，命名为 `text.txt`、`image-1.png`、`audio-1.mp3` 等；同名文件会覆盖，建议每次使用独立目录。
- `--output stdout|clipboard|both` 仍表示输出去向。单个图片可写入剪贴板；音频或混合产物不能写入剪贴板。向管道输出单个媒体产物时使用原始字节，不附加换行；同时指定保存路径时，stdout 只输出文本。终端不会直接打印二进制。
- 文本历史保持 `.txt` 格式；含媒体的历史使用 `.json` 保存内容、base64 媒体和状态，共享保留条数限制。失败和无终态的流不会入库；服务明确报告截断时保留已有结果并警告。单条响应最多 128 MiB，单个输入文件或 stdin 最多 32 MiB。

任务由 Preset 组合上述配置；不需要为任务名增加执行分支。例如自定义 `read-image.toml`：

```toml
profile = "general"
input_modes = ["image", "text"]
required_inputs = ["image"]
output_modes = ["text"]
system = "识别图片中的文字，保留段落。"
```

`input_modes` 是允许集合，`required_inputs` 是必须出现的类型，`output_modes` 是期望输出。
原有 OCR 预设保持其兼容行为；需要强制图片输入时使用上面的声明。
`system` 可省略，因此 TTS / STT 等预设无需伪造系统提示词。Preset 也可设置 `adapter` 及 `[options]`。

Profile 选择优先级：**`--profile` > Preset.profile > AIDO_PROFILE > default_profile > default**。
字段优先级保持 **CLI > Preset > 环境变量 > 已选 Profile > 默认值**。
模式列表整体覆盖；`options` 按参数名合并。`AIDO_ADAPTER` 可覆盖 Profile 的 adapter。
内置 `tts` / `transcribe` / `image` 预设指定适配器和模式；如需改用其他协议，可使用 `--adapter` 或自定义预设。

适配器选项：speech 支持 `voice`、`format`、`speed`；transcription 支持 `language`；images 支持 `format`、`size`、`quality`、`background`、`n`；Responses 图片工具支持 `format`、`size`、`quality`、`background`。
图像格式为 png/jpeg/webp，音频格式为 mp3/opus/aac/flac/wav/pcm；模型不一定支持所有选项。

## 环境变量

| 变量 | 说明 |
|---|---|
| `AIDO_CONFIG` | 配置文件路径（必须指向已存在的文件） |
| `AIDO_ADAPTER` | 协议适配器名称 |
| `AIDO_PROFILE` | 默认 profile |
| `AIDO_BASE_URL` / `OPENAI_BASE_URL` | 接口地址 |
| `AIDO_API_KEY` / `OPENAI_API_KEY` | API key |
| `AIDO_MODEL` | 模型名 |
| `AIDO_MAX_TOKENS` | 最大生成 token 数（`0` 表示不发送该字段） |
| `AIDO_TEMPERATURE` | 采样温度 |
| `AIDO_PRESETS_DIR` | 自定义 preset 目录 |
| `AIDO_HISTORY_DIR` | 自定义结果留底目录 |

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

### preset 专属 API 参数

preset 除了 system 指令，还可以为**该 action 单独指定** API 参数，适合“某个 action 固定要用某个模型/服务商”的场景。例如视觉 OCR 专用 preset `vision.toml`：

```toml
system = "提取图片中的文字，只输出文字本身。"
model = "glm-4.6v"
base_url = "https://open.bigmodel.cn/api/paas/v4"
# api_key = "..."      # 同样支持 api_key / max_tokens / temperature
```

之后 `aido vision --copy` 就自动走这套配置，不用每次敲 `--base-url` / `-m`。

取值优先级：**命令行参数 > preset > 环境变量 > profile > 默认值**。preset 排在环境变量之前，因为 `OPENAI_API_KEY` / `OPENAI_BASE_URL` 往往是为其他工具导出的，不应悄悄盖掉 action 里写明的配置；命令行 flag 永远保留最终决定权。

注意事项：

- 调用字段包括 `adapter` / `base_url` / `api_key` / `model` / `max_tokens` / `temperature`，以及模式和 `options`；Preset 还可通过 `profile` 引用调用配置（`max_tokens = 0` 表示请求里不带该字段）。字段按条独立生效：preset 里没写的字段继续走环境变量 → profile → 默认值。
- `aido list` 会用 `[api_key, model]` 这样的标签标注带参数的 preset——只列字段名，不显示值，避免 key 泄漏到终端。
- ⚠️ `api_key` 会明文保存在 preset 文件里，注意文件权限；更稳妥的做法是省略该字段、走环境变量。

写 preset 的要点：

- 生成文本的 preset 用 **system 指令**描述处理要求，剪贴板/管道/文件提供待处理内容；媒体 preset 也可只声明调用配置和模式。
- 指令末尾加一条“只输出 X、不加解释”之类的收尾约束，能显著减少模型废话。
- 多行文本用 TOML 三引号字符串 `"""..."""`，内容中不能出现连续三个双引号。
- 与内置 preset 同名的文件会覆盖它，如自定义 `translate.toml` 即可修改默认翻译目标语言。
- preset 可自带 `model` / `base_url` / `api_key` 等参数，仅对该 action 生效（见上节）。
- 文件名即 action 名（`aido polish`），因此 `list` / `help` / `last` 是保留名，且不能以 `-` 开头。
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

- 部分 OpenAI 新模型不接受 `max_tokens` 参数名，需要 `--max-tokens 0` 略过
- 可选方向：shell 补全、`--profile` 列表查看、热键常驻模式
