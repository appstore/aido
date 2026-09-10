# aido

把材料交给 AI 任务，把结果送到你要的地方：终端、文件、目录或剪贴板。

```
aido ocr screenshot.png --copy

git diff | aido code-review

aido tts --text "你好" -o hello.mp3
```

内置任务：OCR、翻译、摘要、代码审查、语音合成（TTS）、音频转写、图片生成，外加 `ask`（临时指令）。全部任务共用一套输入 / 输出 / 配置规则；自定义任务放进配置目录即可。

## 安装

从 [Releases](https://github.com/appstore/aido/releases) 下载对应平台的压缩包（Linux / macOS / Windows 二进制由 CI 自动构建，单个可执行文件，无 runtime 依赖）。

或从源码构建（需要 Rust 1.88+）：

```bash
cargo install --path .
```

> 源码构建需要系统装有 **cmake** 与 C 编译器：`tts` 的协议实现 `kothok-edge-tts` 经由其 `tokio-rustls` 依赖的默认特性引入了 `aws-lc-sys`（C 构建需要 cmake）。运行时 TLS 实际使用 ring，aws-lc 只是构建期的额外成本；上游修正特性声明后此要求即可移除（CI 的依赖树检查会在它消失时提醒）。

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

`tts` 无需任何配置和 key：只要运行时落到内置的 `openai` provider——完全没有配置文件时正属此类——语音合成默认走免费的 Edge TTS（微软非官方接口，输出 mp3）；文本类任务仍指向 OpenAI 兼容服务，需要 `AIDO_API_KEY`（或 `OPENAI_API_KEY`）。

```bash
aido tts --text "你好，世界" -o hello.mp3
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

空文件、空 stdin、空剪贴板都会得到明确报错，不会切换来源。

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

指定了显式去向后只执行指定去向。已存在的目标文件默认报错；写入走"同目录临时文件 + 原子提交"。`--json` 与 `--stdout` / `-o -` 互斥。

### 流式与退出码

`--stream` / `--no-stream` 控制正文向 stdout 的实时交付（终端默认实时，管道默认缓冲；两种模式最终字节完全一致；请求仍可用 SSE 收集）。截断的回复**默认不交付**并记入历史。

| 退出码 | 含义 |
|---|---|
| 0 | 完整生成且所有显式输出去向成功 |
| 2 | 命令行、配置、输入、能力或输出目标预检错误 |
| 3 | 服务、网络、超时或响应协议失败 |
| 4 | 生成不完整或产物不满足请求 |
| 5 | 显式输出目标交付失败 |
| 130 | 用户取消（Ctrl+C） |

## 配置：Provider / Profile / Task 三层

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
| `translate` | generate | text | text | `--to LANG` |
| `summarize` | generate | text | text | |
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
processor = "ocr-tiles"        # 或 "single"（默认）

# 可选：
# profile = "vision"           # 默认 Profile
# params = ["to"]              # 接受的专属参数
# [defaults]                   # 参数默认值
# to = "auto"
# [options]                   # 协议选项默认值
```

与管理命令（`tasks` / `profiles` / `config` / `history` / `last`）重名的任务用 `aido run NAME` 调用。未知字段会在报错里指出文件与字段名。

## Edge TTS（免费语音合成）

`tts` 任务除了 OpenAI 兼容的 speech 服务，还内置 `edge-tts` 适配器：走微软 Edge「大声朗读」的非官方接口，无需 API key。只要运行时落到内置的 `openai` provider（完全没有配置文件时正属此类），`aido tts` 默认就走这条免费路径；混用其他服务时也可以显式配置一个只有 speech 路由的 Provider（不需要 `base_url`，端点由适配器持有）：

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

## 长图 OCR

视觉服务端会把超限图片等比压缩（OpenAI 约定长边 2048px，Qwen-VL 系有 `max_pixels` 上限），长图整张发送会被压到文字不可读。`ocr` 任务声明 `ocr-tiles` 策略：高超过 3072px 的竖长图自动切成若干竖条（切缝优先落在空白行，硬切处回看一小段重叠带），逐条请求后按顺序合并；只有重叠带内确实重复的行会被去掉。每个切片请求都携带任务指令。`--no-split` 可关闭。

## 结果留底与恢复

每次运行在历史目录留下一条运行记录（manifest + 原生产物文件）。**生成完成先于交付记录**：剪贴板写失败（退出码 5）后结果照样可以找回，不需要再请求一次模型。

```bash
aido last                    # 重新输出最近一次完整结果
aido last --copy             # 直接塞回剪贴板
aido last --out-dir out/     # 落盘到目录
aido history list            # 查看所有运行（最新在最前，带序号）
aido history show 1          # 按序号输出某次运行（1 = 最新）
aido history show 1 --copy   # 序号 + 交付旗标（-o / --out-dir / --json 同样适用）
aido history show <RUN_ID>   # 也可以用完整 id，或能唯一确定的前缀
aido ocr x.png --no-history  # 本次不留底
```

`last` 只恢复**完整**生成；截断 / 失败的运行保留元数据用于诊断，但不当成可恢复结果。`history show` 的序号按 `list` 显示顺序计数（包含不完整运行）：指向不完整运行时不交付，会提示换一个序号或用 `aido last`（它自动跳到最近的完整运行）。历史默认保留 50 条且总字节不超过 512 MiB（`history_keep` / `history_bytes`）。

## 解释与校验

```bash
aido ocr screenshot.png --dry-run
```

不发请求、不写历史、不动剪贴板：显示输入来源、选中的任务与 Profile、参数来源（cli/task/profile）、预计切片方案、输出去向，以及凭据引用是否已设置（不显示值）。

```bash
aido tasks list / tasks show ocr
aido profiles list
aido config init / config check
```

## 图片输入

图片以 `image_url`（base64 PNG）发送，需要视觉模型（gpt-4o、glm-4.6v、qwen2.5-vl 等）。PNG 原样透传，JPEG/WebP 在适配器边界转 PNG。文本与图片可混用、顺序保留；多个文本文件按文件名分节标注。

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

push（或合并）到 `release` 分支会触发 [GitHub Actions](.github/workflows/release.yml)：跑测试，构建 Linux / macOS（Intel + Apple Silicon）/ Windows 二进制，打 `v<version>` 标签并发布到 [Releases](https://github.com/appstore/aido/releases)。

1. 在 `Cargo.toml` 中更新 `version`
2. 把代码合入 `release` 分支并 push
3. 等待 CI 完成，Release 页即出现对应产物

版本号不变而重复 push 时，同一个 `v<version>` Release 会滚动更新：标签移到最新 commit，同名产物被替换。手动触发（workflow_dispatch）只构建不发布，可用于验证 CI 配置。
