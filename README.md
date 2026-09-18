# aido

把材料交给 AI 任务，把结果送到你要的地方：终端、文件、目录或剪贴板。

```
aido ocr screenshot.png --copy

git diff | aido code-review

aido tts --text "你好" -o hello.mp3
```

内置任务：OCR、翻译、摘要、代码审查、语音合成（TTS）、音频转写、图片生成，外加 `ask`（临时指令）。全部任务共用一套输入 / 输出 / 配置规则；自定义任务放进配置目录即可。

## 安装

从 [Releases](https://github.com/appstore/aido/releases) 下载对应平台的压缩包（Linux / macOS / Windows 二进制由 CI 自动构建，单个可执行文件，无 runtime 依赖）。Linux 另提供 musl 全静态产物（`x86_64-unknown-linux-musl`），不依赖 glibc，任意 x86_64 Linux 环境（含极简容器）可直接运行。

或从源码构建（需要 Rust 1.88+）：

```bash
cargo install --path .
```

> 源码构建需要系统装有 **cmake** 与 C 编译器：`tts` 的协议实现 `kothok-edge-tts` 经由其 `tokio-rustls` 依赖的默认特性引入了 `aws-lc-sys`（C 构建需要 cmake）。运行时 TLS 实际使用 ring，aws-lc 只是构建期的额外成本。`edge-tts` 是 aido 侧的 feature（默认开启），不开 TTS 时可用 `cargo build --no-default-features` 同时甩掉 aws-lc-sys 和 cmake 这两个构建依赖；上游修正特性声明后此要求即可移除（CI 的依赖树检查会在它消失时提醒）。

## 快速开始

```bash
# 1. 生成示例配置（Provider 存连接，Profile 存模型选择）
aido config init

# 2. 设置 API key（推荐环境变量，避免明文落盘）
export AIDO_API_KEY=sk-...

# 3. 用起来
aido ask -p "写一首秋天的诗"
echo hello | aido translate
```

`tts` 无需任何配置和 key：只要运行时落到内置的 `openai` provider——完全没有配置文件时正属此类——语音合成默认走免费的 Edge TTS（微软非官方接口，输出 mp3）；文本类任务仍指向 OpenAI 兼容服务，需要 `AIDO_API_KEY`（或 `OPENAI_API_KEY`）。注意：零配置下的这条默认路由会把**合成文本发送到微软的端点**；要改用 OpenAI 的语音接口，需在配置中定义 provider（覆盖内置默认）并配置密钥，让 speech 不落在 `edge-tts` 路由上（详见下文「Edge TTS」）。

```bash
aido tts --text "你好，世界" -o hello.mp3
```

## 常用示例

每个内置任务的典型完整命令；输入与输出去向的完整规则见下文「命令结构」。

### OCR：图片文字识别

```bash
aido ocr screenshot.png --copy               # 识别截图上的文字，直接进剪贴板
aido ocr screenshot.png -o text.txt          # 存成文件（目标已存在时加 --overwrite）
aido ocr shots/ --out-dir out/               # 目录逐张识别：out/a.txt、out/b.txt
aido ocr "photos/**/*.png" --out-dir out/    # 引号里的 glob 由 aido 自己展开（** 显式递归）
aido ocr long-shot.png --no-split            # 关闭长图切片，整张发送
```

### 翻译与摘要

```bash
echo Hello world | aido translate --to zh-CN    # 管道进，译文出 stdout
aido translate article.md --to ja -o article.ja.md
aido translate "docs/*.md" --out-dir out/       # 多文件逐篇翻译，失败互不影响
aido summarize long-report.md                   # 长文自动分块，汇总成一份整体摘要
aido summarize notes.md --copy                  # 摘要直接进剪贴板
```

### 代码审查

```bash
git diff | aido code-review                     # 审查未提交的改动
git diff main..feature | aido code-review -o review.md
```

### TTS：语音合成

```bash
aido tts article.txt -o article.mp3 --voice zh-CN-YunxiNeural --speed 1.2
echo 文稿内容 | aido tts -o narration.mp3       # 零配置即用：默认走免费的 Edge TTS
```

### 音频转写

```bash
aido transcribe meeting.mp3 -o meeting.txt      # 输入恰好一个音频文件
aido transcribe voice-note.m4a --copy
```

### 图片生成

```bash
aido image --text "一只戴宇航员头盔的柴犬，扁平插画风格" -o dog.png
aido image --text "水彩风格的猫" --count 2 --size 1024x1024 --out-dir cats/   # 多张产物必须落目录
```

### ask：临时指令

```bash
aido ask -p "用一句话解释量子纠缠"               # 只有指令，不需要材料
aido ask a.png b.png -p "比较两张图的差异"       # 多个材料 + 指令
git log -5 | aido ask -p "用中文写本周 changelog"
aido --paste -p "润色这段话"                     # 从剪贴板读材料
```

### 脚本友好的修饰参数

```bash
aido ocr shot.png --dry-run                     # 只看执行计划：不发请求、不动剪贴板
aido summarize big.md --no-stream --quiet       # 缓冲输出 + 静默，适合脚本
aido ask -p "总结要点" --profile vision -m glm-4.6v   # 临时换 Profile / 模型
aido translate article.md --to en -o out.md --overwrite   # 允许覆盖已存在的文件
```

## 命令结构

```
aido <TASK> [INPUT...] [OPTIONS]     # 任务名直接开头
aido run <TASK> [INPUT...] [OPTIONS] # 无歧义入口；可访问与管理命令重名的自定义任务
aido ask [INPUT...] -p <INSTRUCTION> # 临时任务
aido -p <INSTRUCTION> [INPUT...]     # ask 的根命令简写
```

任务名可以出现在 flags 之前或之后：`aido ocr --copy` 与 `aido --copy ocr` 等价。
不带任务名、不带 `-p` 的文件输入会直接报错并给出示例。

### 输入（材料）

| 写法 | 含义 |
|---|---|
| `FILE` | 输入文件，按出现顺序提交，文本/图片/音频可混用 |
| `PDF` | 每页的内嵌图片与文本层按页序展开为材料（一页图+文即两个 part）；矢量绘制、无内嵌图亦无文本层的 PDF 默认报错并给出转图指引（见下） |
| `XLSX` | 每个非空工作表展开为一个文本 part（markdown 表格，按工作表顺序；日期渲染为 ISO 8601） |
| `GLOB` | glob 模式由 aido 自行展开（shell 未展开的场景——引号包裹、Windows——也能用）：结果按字典序，无匹配直接报错；`**` 显式递归 |
| `DIR` | 目录参数：展开为一层文件（按名称排序）；点文件被跳过，子目录直接报错；要递归请用 glob |
| `--text TEXT` | 字面文本材料，可重复、可与文件交错 |
| `-` | 在该位置读取 stdin，最多一次 |
| `--paste` | 在该位置读取剪贴板，最多一次 |
| `-p TEXT` | 本次处理要求（指令），**不是**材料 |

不猜测、不静默丢弃：

| 显式材料 | stdin 为管道 | 行为 |
|---|---|---|
| 无 | 是 | 读取 stdin；空输入报错，**不**回退剪贴板 |
| 含 `-` | 是 | 在 `-` 的位置读入并合并 |
| 无 `-` | 是 | 报错，提示加 `-`（不静默忽略管道） |
| 无 | 否 | 需要材料的任务读剪贴板；`ask` 可仅凭指令运行 |
| 有 | 否 | 用显式材料；若含 `-` 则从 stdin 读到 EOF |

空文件、空 stdin、空剪贴板都会得到明确报错，不会切换来源。glob 无匹配、目录为空或含子目录、单次展开超过 4096 个文件，同样会在发送任何请求之前明确报错。

### 输出（去向）

| 写法 | 含义 |
|---|---|
| （无） | 默认 stdout |
| `-o FILE` | 保存恰好一个产物；`-o -` 表示 stdout |
| `--out-dir DIR` | 保存完整产物集合及 `manifest.json` |
| `--copy` | 把一个文本/图片产物写入剪贴板，可与文件、目录、stdout 组合 |
| `--stdout` | 结果正文或单个媒体字节输出到 stdout |
| `--produce TYPES` | 请求的内容类型（逗号分隔），通常由任务决定 |
| `--format FORMAT` | 单一媒体类型的编码 |
| `--json` | stdout 输出版本化运行报告（替代正文） |
| `--overwrite` | 允许替换已存在的目标文件 |

指定了显式去向后只执行指定去向。已存在的目标文件默认报错；写入走"同目录临时文件 + 原子提交"。`--json` 与 `--stdout` / `-o -` 互斥；报告只含产物的路径与大小、不携带字节，所以二进制产物任务（tts / image）与 `--json` 组合时，终端上必须另给 `-o` 或 `--out-dir`，否则预检即报错。stdout 接管道时放行，产物不写向 stdout、只存历史，可凭报告中的 `run_id` 用 `aido history show <RUN_ID> --out-dir` 恢复。

`--out-dir` 写出的 `manifest.json` 记录本次交付：顶层 `version` 恒为 `1`（另有 `run_id` 与 `artifacts`），每个产物条目列出 `id` / `kind` / `mime` / `file` / `size`；自 0.3.0 起产物条目追加 `provenance` 字段（`{"type":"request","index":N}` 或 `{"type":"merged","requests":[…]}`，标明产物来自该次运行的哪个请求）——老读者应允许其缺省。

文本去向保留既有的格式约定：文件、目录和历史保存产物原始字节；stdout 对非空且未以换行结尾的文本补一个 `\n`；剪贴板文本去除尾部空白（包括空格、制表符和换行）。需要逐字节保留文本时请输出到文件。媒体字节输出不补换行。

### 流式与退出码

`--stream` / `--no-stream` 控制正文向 stdout 的实时交付（终端默认实时，管道默认缓冲；两种模式最终字节完全一致；请求仍可用 SSE 收集）。截断的回复**默认不交付**并记入历史。

| 退出码 | 含义 |
|---|---|
| 0 | 完整生成且所有显式输出去向成功 |
| 2 | 命令行、配置、输入、能力或输出目标预检错误 |
| 3 | 服务、网络、超时或响应协议失败 |
| 4 | 生成不完整或产物不满足请求 |
| 5 | 显式输出目标交付失败 |
| 6 | 批处理部分失败：成功部分已交付，失败部分在 warning 与历史中列出 |
| 130 | 用户取消（Ctrl+C） |

## 配置：Provider / Profile / Task 三层

路径可通过以下环境变量覆盖；未设置或仅包含空白时使用平台默认位置。相对路径相对于当前工作目录。

| 环境变量 | 用途与默认位置 |
|---|---|
| `AIDO_CONFIG` | 配置文件；默认平台配置目录下的 `aido/config.toml`。显式指定的文件不存在时直接报错，不回退默认配置。 |
| `AIDO_TASKS_DIR` | 用户任务目录；默认平台配置目录下的 `aido/tasks/`。 |
| `AIDO_HISTORY_DIR` | 历史目录；默认平台本地数据目录下的 `aido/history/`。 |

Linux 通常使用 `~/.config/aido/` 和 `~/.local/share/aido/history/`（遵循 XDG 配置）。例如：`AIDO_CONFIG=./config.toml AIDO_HISTORY_DIR=./history aido ask --text "你好"`。

- **Provider** 拥有连接：地址、凭据环境变量名、operation → 适配器路由。
- **Profile** 拥有模型选择与调用默认值，引用一个 Provider。
- **Task** 声明 operation、输入/输出契约、固定指令与处理策略，引用默认 Profile。

任务从不保存地址和密钥。凭据只在发送请求前解析；`--dry-run`、`--json` 和日志里只出现变量名。

```toml
default_profile = "vision"

[settings]
# stream = false
# timeout_secs = 120
# hold_secs = 45
# history_keep = 50
# history_bytes = 536870912

[providers.cloud]
base_url = "https://example.invalid/v1"
api_key_env = "MY_AI_API_KEY"

[providers.cloud.routes]
generate = "openai-chat"       # 或 openai-responses
speech = "openai-speech"
transcribe = "openai-transcription"
image = "openai-images"

[profiles.vision]
provider = "cloud"
model = "YOUR_VISION_MODEL"
operations = ["generate"]
input_types = ["text", "image"]
output_types = ["text"]
```

Profile 选择顺序：`--profile` → 任务默认 → `AIDO_PROFILE` → `default_profile`。
参数合并顺序：CLI → 任务默认值 → Profile → 程序默认值。
能力约束（输入/输出类型）取交集校验，不能被高优先级参数绕过：用文本-only 的 Profile 跑 `ocr` 会在请求前失败。

`--model` 只覆盖所选 Profile 的模型，不会切换 Provider 或继承其他服务的认证。

### 内置任务

| 任务 | operation | 输入 | 输出 | 专属参数 |
|---|---|---|---|---|
| `ocr` | generate | image（可加 text） | text | `--no-split` |
| `translate` | generate | text | text | `--to LANG` `--no-split` |
| `summarize` | generate | text | text | `--no-split` |
| `code-review` | generate | text | text | |
| `tts` | speech | text | audio | `--voice` `--speed` |
| `transcribe` | transcribe | 恰好一个 audio | text | |
| `image` | image | text | image | `--count` `--size` |
| `ask` | generate | 任意（可无材料） | text | |

### 自定义任务

把 `NAME.toml` 放进配置目录的 `aido/tasks/`（`AIDO_TASKS_DIR` 可覆盖），文件名即任务名：

```toml
operation = "generate"
instruction = """
识别图片中的文字，保留段落，只输出识别结果。
"""
input_types = ["text", "image"]
required_types = ["image"]
output_types = ["text"]
processor = "ocr-tiles"        # 或 "single"（默认）、"chunk-join" / "chunk-reduce"（长文分块，见下）

# 可选：
# profile = "vision"           # 默认 Profile
# params = ["to"]              # 接受的专属参数
# [defaults]                   # 参数默认值
# to = "auto"
# [options]                   # 协议选项默认值
```

与管理命令（`tasks` / `profiles` / `config` / `history` / `last`）重名的任务用 `aido run NAME` 调用。未知字段会在报错里指出文件与字段名。

## Edge TTS（免费语音合成）

`tts` 任务除了 OpenAI 兼容的 speech 服务，还内置 `edge-tts` 适配器：走微软 Edge「大声朗读」的非官方接口，无需 API key。只要运行时落到内置的 `openai` provider（完全没有配置文件时正属此类），`aido tts` 默认就走这条免费路径。**隐私提示**：这是零配置时的默认路由，不是你显式选择的服务——合成文本会原样发送到微软的端点；要改用 OpenAI 的语音接口，需在配置中定义自己的 Provider 并配置密钥（定义即整体覆盖内置的 `openai` Provider），speech 不路由到 `edge-tts` 时自动落回 `openai-speech` 适配器。混用其他服务时也可以显式配置一个只有 speech 路由的 Provider（不需要 `base_url`，端点由适配器持有）：

```toml
[providers.edge]
routes = { speech = "edge-tts" }

[profiles.edge]
provider = "edge"
model = "edge"                  # 协议不使用模型名，占位即可
operations = ["speech"]
output_types = ["audio"]
[profiles.edge.options]
voice = "zh-CN-XiaoxiaoNeural"  # 默认音色，可省略
```

```bash
aido tts --text "你好，世界" -o hello.mp3 --profile edge
aido tts article.txt -o article.mp3 --profile edge --voice zh-CN-YunxiNeural --speed 1.2
```

输出固定为 mp3（24kHz），长文本按 ~4 KiB 转义预算自动分块、并发合成后按序拼接，整体受 `--total-timeout` 约束（未设置时以每个分块的单请求超时为界）。协议没有指令通道，带 `-p`（或任务的固定指令）的运行在生成执行计划时就会被拒绝，`--dry-run` 也会报告。注意：这是微软的非公开接口，DRM 常量随 Edge 版本轮换，接口可能随微软调整而失效（协议实现依赖 `kothok-edge-tts`，失效时跟随上游更新）。

> `edge-tts` 适配器由同名 feature 控制（默认开启）。用 `--no-default-features` 构建的二进制不含该适配器：配置里指向 `edge-tts` 的路由会在 `config check` 和生成执行计划时被拒绝（提示用 `--features edge-tts` 重新构建，或改走 `openai-speech` 路由），零配置默认也不再路由到它。

## 长图 OCR

视觉服务端会把超限图片等比压缩（OpenAI 约定长边 2048px，Qwen-VL 系有 `max_pixels` 上限），长图整张发送会被压到文字不可读。`ocr` 任务声明 `ocr-tiles` 策略：高超过 3072px 的竖长图自动切成若干竖条（切缝优先落在空白行，硬切处回看一小段重叠带），逐条请求后按顺序合并；只有重叠带内确实重复的行会被去掉。每个切片请求都携带任务指令。未切分的材料（如同发的说明文本）按命令行原始顺序随每一个切片请求携带，超过同一携带预算（约 2000 字符）时退回只随首个请求携带并在 stderr 说明。`--no-split` 可关闭。

## 长文分块

长文本超出模型上下文窗口——或像翻译这类输出随输入等比增长的任务，超出回复上限——单请求会截断或失败。长文分块策略把超过约 4000 字符（按字符计，中英文同一预算）的文本在段落边界切块，逐块请求（每块都携带任务指令），之后的收束方式有两种：

- **chunk-join**（`translate` 声明）：各块回复按序以段落分隔拼接，拼接即全部结果——翻译的分段本来就是最终译文的组成部分。
- **chunk-reduce**（`summarize` 声明）：各块结果先各自完成，再追加**一个汇总请求**，材料是全部分段结果（带分段标记），指令仍是任务原指令，回复即整份最终结果——摘要因此是一份整体摘要，而不是每段一条。单块（未超过分块阈值，或 `--no-split`）不再汇总：单块即全文。

衔接靠**上下文携带**而不是输出重叠去重：每个后续分块附上前一分块末尾的一小段（约 400 字符），明确标注"仅供衔接，不处理、不输出"。翻译对同一段源的两次措辞不会逐字一致，精确匹配去重只在 OCR 这类确定性转写上可行；上下文携带让回复永不重复。没有段落边界的超长段落退到句子边界（中英文终止符都认），连句子都没有的巨句在字符边界硬切；过小的尾块并回前一块。`--no-split` 可关闭。

未切分的材料（如术语表）按命令行原始顺序随**每一个**分块请求携带——分块文本顶替其源文件的位置，每个分块都看得到它；当未切分材料总量超过单块字符预算的一半（约 2000 字符）时，重复携带不再划算，退回只随首个请求携带，并在 stderr 说明。

自定义任务声明 `processor = "chunk-join"` 或 `processor = "chunk-reduce"` 即可选用；旧名 `"chunk-map-reduce"` 仍被接受，等价于 `chunk-join`。

## 批处理（per-part）

多个文件不该共享一个请求。`ocr` 与 `translate` 任务声明了 `per_part = true`：每个文件输入独立走一遍该任务的处理策略（高图照常切片、长文照常分块），产物按输入文件名落盘，`manifest.json` 记录每个产物来自哪次请求。

```bash
aido ocr shots/a.png shots/b.png --out-dir out/     # → out/a.txt, out/b.txt
aido translate docs/a.md docs/b.md --out-dir out/   # 逐篇翻译，逐篇落盘
```

- **失败隔离**：单张失败不影响其余；成功的产物照常交付，最终以**退出码 6** 汇总失败清单（也写进运行记录的 warnings）。全部失败按生成失败处理（退出码 4），不交付。
- **命名**：`a.png → a.txt`；同批重名（不同目录的 `a.png`）自动追加 `-2`、`-3`；中文文件名原样保留（`截图.png → 截图.txt`）。
- **共享材料**：`--text` / stdin / 剪贴板不是批处理单元，会附在每个文件的请求里——术语表跟着每一篇走，而不是自己成为一篇。
- **约束**：批处理必须 `--out-dir`（多个产物无法共享一个 stdout / `-o` / 剪贴板），且强制缓冲交付；单个文件不构成批处理，行为与从前完全一致。
- 自定义任务在 TOML 里加 `per_part = true` 即可启用，与任意 `processor` 组合。

## 结果留底与恢复

每次运行在历史目录留下一条运行记录（manifest + 原生产物文件）。**生成完成先于交付记录**：剪贴板写失败（退出码 5）后结果照样可以找回，不需要再请求一次模型。

```bash
aido last                    # 重新输出最近一次完整结果
aido last --copy             # 直接塞回剪贴板
aido last --out-dir out/     # 落盘到目录
aido history list            # 查看所有运行（旧→新，最新在最后一行；序号 1 = 最新）
aido history show 1          # 按序号输出某次运行（1 = 最新，即 list 的最后一行）
aido history show 1 --copy   # 序号 + 交付旗标（-o / --out-dir / --json 同样适用）
aido history show <RUN_ID>   # 也可以用完整 id，或能唯一确定的前缀
aido ocr x.png --no-history  # 本次不留底
```

`last` 只恢复**完整**生成；截断 / 失败的运行保留元数据用于诊断，但不当成可恢复结果。`history show` 的序号即 `list` 每行显示的编号（1 = 最新一条，包含不完整运行）：指向不完整运行时不交付，会提示换一个序号或用 `aido last`（它自动跳到最近的完整运行）。历史默认保留 50 条且总字节不超过 512 MiB（`history_keep` / `history_bytes`）。

## 解释与校验

```bash
aido ocr screenshot.png --dry-run
```

不发请求、不写历史、不动剪贴板：显示输入来源、选中的任务与 Profile、参数来源（cli/task/profile）、预计切片方案、输出去向，以及凭据引用是否已设置（不显示值）。

```bash
aido tasks list / tasks show ocr
aido profiles
aido config init / config check
```

## 图片输入

图片以 `image_url`（base64 PNG）发送，需要视觉模型（gpt-4o、glm-4.6v、qwen2.5-vl 等）。PNG 原样透传，JPEG/WebP 在适配器边界转 PNG。文本与图片可混用、顺序保留；多个文本文件按文件名分节标注。

## 文档输入（PDF / XLSX）

文档在本地"物化"为普通材料后再走既有管线——不经过任何服务端文件接口，所有 provider 通用，单文件、零 runtime 依赖的性质不变：

- **PDF**：逐页提取内嵌图片（JPEG 原样透传、无损；Flate 位图重编码为 PNG）与文本层（含 `/ToUnicode` CMap，支持中日韩），按页序合并为材料。`ask` 会把全部页放进同一请求（绘本连页阅读）；`ocr` 是 per-part 任务，以"页"为单位——同页的图片与文本层合成一个请求，`--out-dir` 每页一个文件。只有文本层、无任何图片的 PDF 整档合并为一个文本 part（页间以 `----- page N -----` 分隔），`translate` / `summarize` 这类 per-part 文本任务因此保住跨页上下文（分块策略在段落边界切分），而不是每页孤立一个请求。无损编码之外的格式（JPEG 2000、CCITT 传真、JBIG2、PNG predictor 的阵列参数形式）跳过而不报错；解不出的字形丢弃而不输出乱码——页面图片兜住信息。文档整体物化有上限（4096 个 part / 32 MB），每条内容流的解压量也有上限——恶意构造的容器（解压炸弹、谎报尺寸的表头）会被拒绝而不是拖垮进程；矢量回退的渲染同样逐页计入预算，先超限先拒绝。
- **矢量 PDF**：既无内嵌图也无文本层的 PDF（纯矢量绘制）默认报错，报错里给出转图指引（`pdftoppm -png`）；以 `--features pdfium` 构建的版本会用静态链入的 [pdfium](https://github.com/bblanchon/pdfium-binaries/releases)（取 `-static` 产物，设置 `PDFIUM_LIB_DIR` 指向其目录）把每页渲染成图片，默认构建完全不触碰 pdfium。链接期如报 C++ 符号缺失，按 build.rs 顶部注释补 `-lstdc++` / `-lc++`。
- **XLSX**：每个非空工作表一个文本 part，markdown 表格、日期 `YYYY-MM-DDTHH:MM:SS`。超过 400 万单元格的巨型工作表会被拒绝（完整载入内存的保护），加载器急切解压的条目（共享字符串等）另有实测解压量上限——表头谎报尺寸拦不住它；中等大小的大表会撞单请求上下文，建议走 `summarize`（自动分块）。`.xlsm` 宏不会被读取或执行。

.docx / .pptx 已识别但尚未支持，会报错提示先转文本（或图片）；csv / md / txt 本就是文本输入，直接可用。

## Linux 剪贴板说明

X11（以及多数 Wayland 合成器）的剪贴板内容依附于写入它的进程，进程退出后内容即失效。aido 写剪贴板时会自动派生一个后台子进程，把内容保持一段时间（默认 45 秒，`settings.hold_secs` 可调），行为与 `xclip` / `wl-copy` 一致。

## 从 0.x 升级（破坏性变更）

接口围绕"任务"重构，旧参数不再兼容：

| 旧接口 | 新接口 |
|---|---|
| `--save FILE` | `-o FILE`（显式去向，不再默认同时 stdout） |
| `--save-dir DIR` | `--out-dir DIR` |
| `-o clipboard` / `both` / `stdout` | `--copy` / `--copy --stdout` / `--stdout` |
| `--preset NAME` / `aido NAME` | `aido NAME` / `aido run NAME`（同名即可） |
| `--output-mode` | `--produce` |
| `--input-mode` | 任务的输入契约（`aido tasks show`） |
| `--adapter` / `--base-url` / `--api-key` | Provider / Profile 配置 |
| `--init` / `list` / `--list-presets` | `config init` / `tasks list` |
| `--no-spinner` | `--quiet`（同时关闭成功提示） |
| preset 的 `base_url` / `api_key` / `adapter` 覆盖 | 专用 Provider / Profile 引用 |

行为变化：空管道**不再回退剪贴板**；文件与管道必须用 `-` 显式组合；`-p` 不再读剪贴板；`ocr` 必须有图片输入；已存在文件默认不覆盖；截断结果默认非零退出且不交付；管道默认缓冲模式；token 上限默认不发送。

## 发布流程

push（或合并）到 `release` 分支会触发 [GitHub Actions](.github/workflows/release.yml)：跑测试，构建 Linux（gnu + musl 全静态）/ macOS（Intel + Apple Silicon）/ Windows 二进制，打 `v<version>` 标签并发布到 [Releases](https://github.com/appstore/aido/releases)。

1. 在 `Cargo.toml` 中更新 `version`
2. 把代码合入 `release` 分支并 push
3. 等待 CI 完成，Release 页即出现对应产物

版本号不变而重复 push 时，同一个 `v<version>` Release 会滚动更新：标签移到最新 commit，同名产物被替换。手动触发（workflow_dispatch）只构建不发布，可用于验证 CI 配置。
