# PR #25 Review 修复 TODO

> 本文件是 `docs/pr-25-review.md` 的**完整工作版**：原审阅的全部内容——审阅背景与结论、F01–F33 每条的问题描述（含示例、行号）、实现方案（含备选与权衡）、追加审阅的状态说明——逐条保留并加上勾选框跟踪进度。
>
> **工作方式**：**每个issue都开独立的subagent进行逐个issue修复按批次顺序逐条修复**，再由总agent来进行统一review；每条 = 改代码 + 补测试 + `cargo fmt` + `cargo clippy` + `cargo test` 全绿 + 自 review diff + 勾选本条 + 独立 commit。
> **分支**：`fix/pr-25-review`（基于 `50c6559`）。

## 批次总览（执行顺序）

原文：「分 5 批。批次是有序的：第 1 批决定『能不能合并』，第 2 批决定『结果对不对』，之后三批可以并行推进。每批列出覆盖的 finding、改动位置和需要补的测试。」

| 批次 | 主题                       | Findings                    | 批次说明（原文）                                                                                                                                           |
| ---- | -------------------------- | --------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1    | 解除阻断：让合法命令跑起来 | F01 F03 F05 F12             | 四个改动都很小、互不耦合，但每一个都在拒绝一条文档承诺过的用法。**合并前必须清空。**                                                                       |
| 2    | 执行链语义：让结果是对的   | F02 F06 F09                 | 都在 `processors/` 和 `runner.rs`，改动面比批次 1 大。F02 和 F09 会改变 summarize/translate 的实际输出，建议在同一个 commit 里连带更新 README 的行为说明。 |
| 3    | 安全与数据完整性           | F04 F07 F08 F13 F17 F26     | 都是独立的小改动，可以拆成单独的 commit 并行推进。                                                                                                         |
| 4    | 契约一致性                 | F10 F11 F14 F15 F16 F19     | 让 README 里写的退出码契约和 `--json` 契约真正成立，并清掉两处会误导后续维护者的结构。                                                                     |
| 5    | 死代码、依赖与文档         | F18 F20 F21 F22 F23 F24 F25 | 清理三处死代码、补齐 dry-run 的参数来源、消除硬编码、修正文档与行为。均为低风险独立改动，可并行。                                                          |
| 6    | 追加审阅（af6f9c5 合并后） | F27 F28 F29 F30 F31 F32 F33 | 2 中 5 低；F29–F33 均为小改动，可并行。                                                                                                                    |

---

# 第一部分 · 主审阅（PR #25）

## 审阅背景与结论

- PR #25 把 aido 从「按任务名分支的脚本」重写为「材料 → 任务 → 产物 → 交付」的统一执行链，落地 issue #24 的全部设计。方向正确、分层干净；但有 6 个问题会让正确的命令在正常环境下失败或给出错误结果。
- 规模：+12034 / −5146，7 commits，CI 3/3 通过，26 findings。
- **结论：暂不合并，先修完第 1、2 批。**
- 架构收益（审阅确认属实，非仅文档宣称）：`domain.rs` 不依赖任何 IO；`plan → runner → output → history` 的单向依赖成立；主流程确实不再按任务名分支；凭据只在发请求前解析。测试扎实——116 单元 + 97 集成，全部打本地假服务器，没有一处真实网络调用。
- 问题集中在契约的边界情形：几条新规则在设计文档里成立，落到真实环境（CI、容器、剪贴板工作流、长文本）就会拒绝合法命令，或者安静地给出错误结果。F01（非交互式环境全线报错）和 F02（长文摘要输出 N 段互不相干的摘要）是最需要先解决的两个。
- 这是一次破坏性重构，且刻意不保留迁移路径。合并前把第 1 批修完，等于保证「老用户按新文档改完就能用」——目前还做不到这一点。
- 严重度分布：高 6（正确命令失败 / 结果错误）、中 11（契约不一致 / 数据完整性）、低 9（死代码 / 依赖 / 文档）。
- 编号 F01–F26 在实现方案里被逐条引用；行号对应 PR 分支 `e8bad8c`。

---

## 高 · F01–F06 · 会让正确的命令失败或产出错误结果

- [x] **F01 · 高 · `src/input.rs:117` · 非交互式环境下，任何带文件的命令都会报错退出**
  - 问题：「stdin 被管道占用但没用 `-` 消费」这条规则只看 `stdin_is_terminal`，不看管道里是否真有数据。而 CI runner、cron、systemd、不带 `-t` 的 docker run，stdin 一律是 `/dev/null` 或已关闭的管道——都不是 tty。

    ```console
    $ aido translate README.md --to en < /dev/null
    error: stdin is piped but not consumed; add `-` where the piped data belongs
    ```

    也就是说，重构后 aido 在所有自动化场景里都无法直接调用，除非每条命令都补一个 `< /dev/tty`。这条规则的本意是拦住「用户以为自己在传管道」的误用，代价不该是让非交互式调用整体失效。

  - 方案：核心是把「stdin 不是 tty」换成「stdin 上确实有待读数据」。改 `input.rs`：
    - 给 `InputEnv` 增加 `fn stdin_has_data(&mut self) -> bool`。真实实现里对 fd 0 做非阻塞 poll/select（Unix 用 `libc::poll`，超时 0；Windows 走 `PeekNamedPipe`），返回「可读且非 EOF」。测试实现直接由构造参数给定。
    - `gather()` 里 `:117` 的分支条件从 `!env.stdin_is_terminal` 改成 `env.stdin_has_data()`。
    - `:99` 的「无 specs 时读 stdin」分支保持不变——那里读到空会明确报「stdin is empty」，语义是对的。
    - 退一步方案（不想引入 poll 时）：先把 stdin 读进缓冲，为空就当作「没有管道」继续执行，非空才要求 `-`。行为等价，代价是最多缓存 32 MB，且会消耗掉 stdin（对这个工具无所谓）。
  - 测试：`tests/input.rs` 增加「stdin 重定向自 `/dev/null` + 显式文件 → 退出 0」，以及「stdin 是非空管道 + 显式文件 → 仍然报 `add -`」。现有的 `unconsumed_pipe_with_explicit_material_is_an_error` 保留。

- [x] **F02 · 高 · `src/processors/chunk.rs:306` · `src/runner.rs:141` · 名为 chunk-map-reduce 的处理器没有 reduce 步骤**
  - 问题：`ChunkGate` 只做一件事：把各分块的回复用空行拼起来。没有任何汇总请求。对 translate 这没问题（逐段翻译再拼接是对的），但 summarize 默认也挂了这个处理器。结果是：一篇 12000 字的文档被切成 3 块，用户拿到的是 3 段各自独立的「一句话总结 + 3-6 条要点」，而不是一份整体摘要。issue #24 里明确写了「长文摘要：若将来支持分段，需要『分段摘要再汇总』的策略」——这一条没有实现，但处理器的名字和 README 都在暗示它实现了。
  - 方案：不是「补一个 reduce」，而是把一个名字拆成两个策略：
    - `ProcessorKind::ChunkJoin` —— 现有行为，逐块处理后顺序拼接。`translate.toml` 改用它。
    - `ProcessorKind::ChunkReduce` —— 逐块处理后，再发一次汇总请求，把各块结果作为材料、带上任务原始 instruction。`summarize.toml` 用它。
    - 实现上，`RequestStep` 需要一个 `role: StepRole { Map, Reduce }` 字段。`plan_steps` 在 `ChunkReduce` 且块数 > 1 时追加一个 reduce 步骤（它的 inputs 在 planning 阶段还不存在，所以标记为占位，由 runner 在跑完所有 map 步后填入）。`runner.rs` 的主循环相应地分两段：先跑完所有 map 步收集文本，再构造 reduce 请求。
    - 流式交付注意：reduce 模式下 map 步的输出不能打到 stdout（它们是中间结果）。所以 `live_stdout` 在 `ChunkReduce` 下只对 reduce 步生效，map 阶段只转 spinner 进度。
    - 另外 `plan.rs:373` 的 `select_processor` 和 `--no-split` 的语义保持不变：`--no-split` 一律退回 `Single`。
  - 测试：`tests/chunk.rs` 增加「假服务器对 3 个 map 请求分别回 A/B/C，对第 4 个请求断言其材料里同时含 A、B、C，最终 stdout 只有 reduce 的回复」。
  - 批次 2 说明：会改变 summarize/translate 的实际输出，同 commit 连带更新 README 行为说明。

- [x] **F03 · 高 · `src/plan.rs:445` · `aido image --copy` 在终端里必然被拒**
  - 问题：`validate_outputs` 判断「二进制产物是否有去处」时，`has_file_target` 只看 `-o` 和 `--out-dir`，完全没把 `--copy` 算进去。而这个检查跑在 `resolve_destinations` 之前。

    ```console
    $ aido image --text "一只柴犬" --copy
    error: binary output needs -o FILE or --out-dir, or a stdout pipe
    ```

    README 第 88 行写的是「`--copy`：把一个文本/图片产物写入剪贴板」。代码和文档直接矛盾，且没有测试覆盖这条路径。

  - 方案（最小改法）：

    ```rust
    let has_file_target = cli.output.is_some() || cli.out_dir.is_some();
    //                    ↓
    let has_binary_target = cli.output.is_some()
        || cli.out_dir.is_some()
        || (cli.copy && !resolved.produce.contains(&MediaKind::Audio));
    ```

    音频要排除掉，因为 `resolve_destinations:560` 本来就会拒绝音频进剪贴板——那条更具体的错误信息更有用，应当让它先说话。
    更彻底的做法（**推荐**）：把这个检查整体挪到 `resolve_destinations` 之后，让它对已解析出的 destinations 判断「有没有一个能接住二进制产物」，而不是重新推断 CLI 旗标。这样以后新增去处不会再漏。

  - 测试：`tests/output.rs` 增加 `image --text ... --copy` 在伪 tty 下 `--dry-run` 退出 0、且计划里列出 clipboard。

- [x] **F04 · 高 · `src/api/mod.rs:281` · `src/processors/ocr.rs:113` · 解压炸弹护栏对 JPEG / WebP 完全失效**
  - 问题：`slice_if_tall()` 的第一行就是 `image_as_png(part)`，而它对非 PNG 输入会执行一次无像素上限的完整解码。`MAX_DECODE_PIXELS`（200 MP）的检查在这之后才跑——护栏永远来不及生效。输入侧只限制字节数（32 MB），而 JPEG 的压缩比足以让一个 40 KB 的文件声明 30000×30000。解到 RGBA 就是约 3.6 GB，进程直接 OOM。代码里那句「Every image here is PNG，所以读 IHDR 就能拿到尺寸、不用付完整解码的代价」的注释，对非 PNG 路径是不成立的。
  - 方案：让像素上限对所有格式生效，且在完整解码之前。改 `api/mod.rs`：
    - 新增 `pub(crate) fn image_dimensions(bytes: &[u8]) -> Result<(u32, u32)>`，用 `image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?.into_dimensions()?`——它只读头部，不解码像素。
    - `image_as_png()` 开头先调它，宽×高超过 `MAX_DECODE_PIXELS` 直接 `bail!`。把常量从 `ocr.rs` 提到 `api/mod.rs`，让适配器和处理器共用同一个护栏。
    - `ocr.rs:113` 的 `slice_if_tall` 相应改成先 `image_dimensions(原始 bytes)` 判断尺寸和是否需要切分，确定要切了才调 `image_as_png`。这同时修掉了现在非 PNG 图片被解码两次的浪费，也让那句「读 IHDR 不用完整解码」的注释重新成立。
  - 测试：构造一个声明 20000×20000 的小 JPEG，断言退出 2 且内存不爆（测试里用 `--dry-run` 即可触发切分规划）。

- [x] **F05 · 高 · `src/config/mod.rs:160 vs :273` · 跑一次 `aido config init`，零配置 TTS 就坏了**
  - 问题：`default_config()`（无配置文件时使用）给 openai provider 挂了 `routes = { speech = "edge-tts" }`，所以 `aido tts` 不需要任何 API key。但 `config init` 写出的 `SAMPLE_CONFIG` 里没有这段 routes。于是用户按 README 的「第一步：`aido config init`」操作完，`aido tts` 就从「开箱即用」变成「报错：缺少 `AIDO_API_KEY`」。测试 `tests/config.rs:387` 恰好只覆盖了「完全没有配置文件」这一种情况，所以 CI 看不到。
  - 方案：在 `SAMPLE_CONFIG` 的 `[providers.openai]` 下补上与 `default_config()` 一致的路由，并解释为什么：

    ```toml
    [providers.openai]
    base_url = "https://api.openai.com/v1"
    api_key_env = "AIDO_API_KEY"

    # speech 默认走无需密钥的 Edge Read Aloud 协议（非官方接口，
    # 文本会发往微软端点）。要改用 OpenAI 的语音接口，删掉这一行。
    [providers.openai.routes]
    speech = "edge-tts"
    ```

    根治办法：让 `SAMPLE_CONFIG` 不再是一份手写字符串，而是由 `default_config()` 序列化后加注释生成——这样两者不可能再分叉。但那需要给 `Config` 加 `Serialize`，可以放到批次 5。

  - 测试：`tests/config.rs` 增加「跑 `config init` → 再跑 `tts --dry-run` → 计划里的 route 是 edge-tts 且 credentials 显示 none required」。这正好补上 `:387` 那个测试的盲区。

- [x] **F06 · 高 · `src/runner.rs:215` · 多请求运行中途失败会丢掉已生成的内容，且不留历史**
  - 问题：一张长截图切成 5 片，第 4 片请求失败 → `execute()` 立刻 `return Err`，前 3 片的文本全部丢弃，`save_generation` 一次都没被调用。用户重跑要重新付 5 次请求的钱。更糟的是流式场景：前 3 片的文本已经打到 stdout 上了，但进程以退出码 3 结束、历史里什么都没有。这恰好违背了这次重构自己立的核心承诺——「生成状态与交付状态分离，生成成功的内容一定可恢复」。
  - 方案：`runner.rs:215` 的 `return Err` 改成记录并终止：
    - 捕获错误后不直接返回，而是 `overall = GenerationStatus::Incomplete { reason: format!("request {}/{} failed: {e}", ...) }`，break 出循环。
    - 照常走 `gate.finish()` 和产物组装，把已合并的文本变成 artifact。
    - 把错误本身塞进 `RunOutput` 新增的 `pub failure: Option<AppError>`。
    - `app.rs` 侧：`record.generation` 已经是 `Incomplete`，走的就是现有的「记录但不交付」分支（`:186`），历史会保留元数据。这里再加一条：当 `output.status.is_complete()` 为假但已有非空 artifact 时，把 `save_generation` 的 `keep_artifacts` 传 `true`，让部分文本落盘可恢复。
    - 退出码用 `failure` 的分类（服务错误 → 3），而不是笼统的 4，这样脚本仍然分得清「服务挂了」和「模型答得不完整」。
    - 流式已经打到 stdout 的部分无法收回，所以 stderr 上要明确说一句：`warning: 已输出前 N/M 个分片的结果；完整记录见 aido history show <id>`。
  - 测试：`tests/protocol.rs` 增加「假服务器前 2 个请求正常、第 3 个返回 500 → 退出 3、历史里有一条 incomplete 记录且含前 2 片文本」。
  - 状态更新（追加审阅，2026-09-11）：**F06 只在 per-part 路径上被解决**，单文档 chunk（长文翻译）路径原样未修——详见第二部分「对旧 finding 的状态说明」。修复本条时以单文档路径为准，不要因为批处理有失败隔离就认为已闭合。

---

## 中 · F07–F17 · 契约不一致与数据完整性

- [x] **F07 · 中 · `src/output.rs:471` · `--out-dir` 不带 `--overwrite` 也会覆盖 `manifest.json`**
  - 问题：`write_directory()` 给产物文件传 `overwrite=args.overwrite`（默认拒绝覆盖），却给 manifest 硬编码了 `true`。后果：对同一个目录跑第二次、且两次产物文件名不同（比如上次是 `image-1.png`、这次是 `text.txt`），旧的 manifest 被无声替换成只列新产物的版本，上一次的文件就变成没人索引的孤儿。而 manifest 正是这个目录里唯一的「这次交付包含什么」的记录。
  - 方案：`:471` 的硬编码 `true` 改成 `overwrite`。但这会让「对同一目录跑第二次」整体失败——这其实是正确的默认行为（目录交付是一个整体，不该半新半旧）。所以配套把错误信息说清楚：
    > `{dir}` 里已有上一次交付的 `manifest.json`；加 `--overwrite` 覆盖整个目录，或换一个 `--out-dir`
    > 更好的做法（推荐）：在写任何文件之前先做一次预检：扫描目标目录，若 manifest 或任一目标文件名已存在且未给 `--overwrite`，立刻失败，一个字节都不写。现在的逐个写入会在中途失败时留下半个目录。
  - 测试：`tests/output.rs` 增加「对同一 out-dir 连跑两次、第二次不带 `--overwrite` → 退出 5、旧 manifest 内容不变」。

- [x] **F08 · 中 · `src/plan.rs:752` · `--dry-run` 会把 `base_url` 里的密码原样打出来**
  - 问题：`redact_url()` 只把 query 参数的值换成 `…`，不碰 URL 的 userinfo 段。配置里写 `base_url = "https://user:s3cret@gw.internal/v1"`（内网网关的常见写法），`--dry-run` 就会原样打印整串。PR 描述里说 dry-run「不展示密钥」，这条路径是个例外。
  - 方案：

    ```rust
    Ok(mut u) => {
        if !u.username().is_empty() { let _ = u.set_username("***"); }
        if u.password().is_some()   { let _ = u.set_password(Some("***")); }
        // …现有的 query 脱敏…
    }
    ```

    `Err(_)` 分支目前直接返回原串——解析失败的 URL 同样可能含密码。改成返回 `"(unparseable base_url, hidden)"` 更稳妥，反正 dry-run 的读者需要的是「配没配对」而不是原文。

  - 测试：`tests/config.rs` 增加「`base_url` 含 `user:pw@` → dry-run 输出不含 `pw`」。

- [x] **F09 · 中 · `src/processors/ocr.rs:71` · `src/processors/chunk.rs:76` · 未被切分的材料只进第一个请求**
  - 问题：两个处理器都把「没被切分的那些输入」（untouched）整体塞进第 0 步，后续分块只带上下文摘要。

    ```console
    aido translate 术语表.md 长文.md
    ```

    术语表只对第 1 块生效，第 2 块之后的译文看不到它。而且原始顺序也丢了：不管术语表写在命令的哪个位置，它都被提到最前面。对「用一段说明去处理另一份材料」这种组合输入场景，这是安静的错误结果。

  - 方案：
    - 不再把 untouched 单独收集，而是保留输入的原始序号；构造每个 step 时，按原序把「非切分材料」和「本步的切片」一起排列——切片替换掉它原来所在的位置。
    - 这样 `aido translate 术语表.md 长文.md` 的第 k 个请求携带的是 `[术语表.md, 长文.md 第 k 块]`，顺序和用户给的一致，每块都看得到术语表。
    - 代价是重复发送这些材料。加一条保护：当未切分材料的总字符数超过单块预算的一半时，退回现有行为并在 stderr 上说明原因，避免把上下文撑爆。
    - 多张长图的情形同理——第二张图的切片也应当带上同样的非切分材料。
  - 测试：`tests/chunk.rs` 断言第 2、3 个请求的材料里都含术语表内容，且材料顺序与命令行顺序一致。
  - 批次 2 说明：会改变 translate 的实际输出，同 commit 连带更新 README 行为说明。

- [x] **F10 · 中 · `src/output.rs:90, :101, :110` · 交付期的错误被分类成退出码 2，与契约冲突**
  - 问题：`deliver_inner()` 里「多个产物塞不进一个文件」「多个产物共享裸 stdout」「扩展名与编码不符」这三条发生在生成完成之后，却用了 `AppError::usage`（退出 2）。同一个函数里，剪贴板的同类检查用的是 `AppError::delivery`（退出 5）。README 定义的是「2 = 用法/预检、5 = 交付失败」。脚本看到退出 2 会以为「命令写错了，什么都没发生」，实际上模型已经跑完、内容已经存进历史了。
  - 方案：三处 `AppError::usage` 改成 `AppError::delivery`。判据很清晰：这个函数只在生成成功之后才会被调用，所以它产生的任何错误都是交付失败。同时把这三条早退改成「记录 `DeliveryState::Failed` 后再返回」，否则 `record.deliveries` 是空的，历史里看不出交付尝试过。可以给这三条构造一个 `Destination` 已知的 state 再 push。
  - 测试：`tests/output.rs` 增加「服务返回 2 张图但只给了 `-o one.png` → 退出 5、历史记录里 file 目标标记为 failed、`aido last --out-dir` 能恢复」。

- [x] **F11 · 中 · `src/app.rs:207` · `--json` 在退出码 2/3/4 时完全不输出 JSON**
  - 问题：JSON 报告是在 `output::deliver()` 里生成的，而生成失败、预检失败、服务错误都在到达 deliver 之前就 `return Err` 了。`--json` 的调用方只在成功和交付失败（0/5）时拿得到 JSON，其余情况只有 stderr 上的一行自然语言。对一个专门给脚本用的旗标来说，这让错误处理没法统一写。
  - 方案：把 JSON 报告的生成从 `output.rs` 里抽出来成 `pub fn error_report(kind, message, run_id, task) -> serde_json::Value`，与现有的成功报告共用 `version: 1` 和字段名。`app.rs` 的 `fail()` 需要知道当前是否 `--json`。最简单的接法：`run()` 解析出 cli 后把 `cli.json` 存进一个局部变量，传给 `fail()`；为 true 时把报告打到 stdout、把人类可读的一行仍然打到 stderr。`normalize` 阶段就失败的情况（clap 之前）拿不到 `cli.json`，可以在 `run()` 开头对 argv 做一次朴素扫描：含 `--json` 就置位。
  - 测试：`tests/output.rs` 对退出码 2、3、4 各断言一次 stdout 是合法 JSON 且 `error.kind` 分别为 usage/service/generation。

- [x] **F12 · 中 · `src/input.rs:178` · OCR 的旗舰工作流没法用 `--dry-run` 检查**
  - 问题：`dry_run_clipboard_part()` 造的占位输入固定是 `MediaKind::Text`，而 ocr 声明了 `required_types = ["image"]`。所以：

    ```console
    $ aido ocr --copy --dry-run
    error: task 'ocr' requires image input; none of the material is image
    ```

    `aido ocr --copy`（截图在剪贴板里）正是 README 开篇第一个例子，而它的执行计划恰恰无法预览。`tests/input.rs:356` 把这个退出 2 当成了预期行为断言下来——这说明是设计取舍，但取舍的方向值得重新考虑：占位输入应当跳过类型检查，而不是伪装成 text 再被类型检查打回。

  - 方案（问题的根源是用 `MediaKind::Text` 去表示「类型未知」，两种改法）：
    - **推荐**：给 `InputPart` 加 `pub unknown_kind: bool`（`#[serde(default)]`），`dry_run_clipboard_part` 置 `true`。`plan.rs` 的 `validate_inputs` 对这类 part 跳过 `allowed_inputs` / `adapter.inputs()` / `required_types` 三项检查，同时在 `describe()` 的材料行里标注「类型将在运行时确定」。
    - **更轻量**：`validate_inputs` 里若 `required_types` 未满足、但存在来源为 Clipboard 且处于 dry-run 的 part，降级为 stderr 上的一行提示而非错误。
  - 测试：把 `tests/input.rs:356` 的断言从「退出 2 且提到 image」改成「退出 0、计划里材料行标注类型待定、且没有触碰剪贴板」。

- [x] **F13 · 中 · `src/history.rs:325` · 历史字节预算的清理只在绝对路径下碰巧生效**
  - 问题：`prune()` 的字节预算分支把已经拼好的完整路径又传回 `remove_run(&dir, id)`，后者再 `dir.join(id)` 一次。这在 id 是绝对路径时靠 `Path::join` 的「绝对路径覆盖 base」特性侥幸正确。一旦 `AIDO_HISTORY_DIR` 设成相对路径，拼出来的就是 `hist/hist/2026…`，删除失败，字节预算彻底不起作用，同时每次运行都在 stderr 上刷一行 `warning: failed to prune history entry`。测试全部用绝对临时目录，所以覆盖不到。
  - 方案：`history.rs:325` 把 `dirs: Vec<PathBuf>` 换成 `Vec<(String, u64)>`（id + 尺寸），循环里 `remove_run(&dir, &id)`。顺带避免了 `dir_size` 被算两遍。
  - 测试：`tests/history.rs` 已有的字节预算测试改用相对 `AIDO_HISTORY_DIR`（配合 `current_dir`）跑一遍，现在的绝对路径版本保留。

- [x] **F14 · 中 · `src/runner.rs:241, :248` · `src/api/media.rs:117, :174` · Provenance 是个形同虚设的字段**
  - 问题：`domain.rs` 用整整一段注释解释 Provenance 的用途：「一次运行可能发出多个请求（OCR 切片），记录哪个请求产出了哪个产物」。实际上 `runner.rs` 给所有产物硬编码 `Request { index: 0 }`；适配器那边更离谱，新生成的产物一律标成 `Provenance::Restored`（「从历史恢复，非本进程产出」），只是随后被 runner 覆盖掉了。字段写进了 manifest、序列化进了历史，但承载的信息是假的。
  - 方案（两条路选一条，不要维持现状）：
    - **做实（所选）**：`GenerateResult` 增加 `request_index`，由 `runner.rs` 在循环里用 `step.index` 填；文本 artifact 的 provenance 改成 `Merged { requests: Vec<usize> }`（切片合并本来就来自多个请求）。适配器里那些 `Provenance::Restored` 占位全部删掉——改成让适配器返回不带 provenance 的中间结构，由 runner 统一赋值，这样「新生成的东西被标成 Restored」在类型上就不可能发生。
    - **删掉**：如果暂时没有消费方，就从 `Artifact` 和 manifest 里移除，等真正需要溯源时再加。
  - 测试：多请求运行的 artifact provenance 指向真实请求序号；单请求运行指向 `Request { index: 0 }`；切片合并运行指向 `Merged`。

- [x] **F15 · 中 · `src/cli.rs:684-726` · `src/app.rs:405` · `history show` 子命令自带的旗标字段永远是假**
  - 问题：normalize 的设计是把所有 flag 收进 rest、再把管理命令的词追加到末尾。所以 clap 拿到的永远是 `[--copy, history, show, 1]`——`--copy` 被顶层 `Cli` 吃掉，`HistoryCmd::Show` 那 7 个 `#[arg]` 字段在结构上不可能被赋值。功能上没坏，因为 `app.rs:405` 用 `output.or_else(|| cli.output)` 做了兜底。但这是个陷阱：后面谁往 `Show` 加一个新旗标、忘了同步加兜底，它就会静默失效，而代码看起来完全正常。
  - 方案（※补全，择一）：
    - **推荐**：删掉 `HistoryCmd::Show` 上结构性不可达的 7 个 `#[arg]` 字段，统一走顶层 `Cli` 的旗标，`app.rs:405` 的 `or_else` 兜底改为直接读 `cli.output`，消除双真相。这样新增旗标只有一个来源。
    - **备选**：改 normalize，把已被顶层吃掉的重复旗标从 rest 剥离/正确映射到子命令。但 clap 的解析顺序决定了顶层优先，改动面更大。
  - 测试：`aido history show 1 --copy` 行为不变；grep 确认 `Show` 无不可达字段。

- [x] **F16 · 中 · `src/config/resolve.rs:239` · `produce.dedup()` 只去掉相邻重复**
  - 问题：`Vec::dedup` 的语义是「移除连续重复项」。`--produce image,text,image` 原样保留三项，随后 `validate_outputs` 数出 2 个媒体类型，错误地报「`--format` 在产出多种媒体类型时有歧义」；`expected_counts` 也会重复计算。
  - 方案（※补全）：保序去重——遍历时用 `HashSet<MediaKind>` 记录已见，跳过重复项。**不要** sort+dedup，因为 produce 顺序有意义（决定产物顺序），排序会改变行为。
  - 测试：`--produce image,text,image` 不再报 format 歧义、计数正确、产物顺序为 image→text。

- [x] **F17 · 中 · `src/output.rs:381` · 所有输出文件被固定成 0600，无视 umask**
  - 问题：`write_file_atomic()` 给临时文件设了 `0o600`，而无论是 hard-link 提交还是 rename 提交都会保留这个 inode 的权限位。于是 `aido translate a.md -o b.md` 产出的是一个 0600 文件。历史目录用 0600 是合理的（那是隐私数据），但用户显式指定的 `-o` 目标是普通产物——写进共享目录、构建产物目录或静态站点目录时，这个权限会造成意外。`set_permissions` 的返回值也被丢弃了，失败时无声。
  - 方案：`write_file_atomic` 增加一个参数 `mode: FileMode { Private, Default }`：
    - `history.rs` 走 `Private`（保持 0600）。
    - `output.rs` 的 `-o` / `--out-dir` 走 `Default`：临时文件写完后 `set_permissions(0o666 & !umask)`。取 umask 可以用 `libc::umask(0)` 读完立刻还原，或者更简单——建一个空文件让系统应用 umask，读它的 mode 作为模板。
    - `set_permissions` 的 `Result` 不要再丢，失败时进 warning。
  - 测试：`-o` 产物权限尊重 umask，历史文件保持 0600。

---

## 低 · F18–F26 · 死代码、依赖与文档

- [x] **F18 · 低 · `resolve.rs:33` · `tasks.rs:115, :45` · 三处死代码**
  - 问题：`Sourced<T>` 定义了但全仓无引用；`TaskParam::maps_to()` 声明了参数到适配器 option 的映射，但 `plan.rs` 的 `apply_param_options` 把这套映射又硬编码了一遍；`Operation::default_route()` 同样无调用点（实际用的是 `conventional_adapter`）。三份真相来源里有两份是死的，改一处不会报错但会不一致。
  - 方案（※补全，择一）：
    - **推荐**：让 `apply_param_options` 改用 `maps_to()`，删掉硬编码映射，消除双真相；删掉 `Sourced<T>` 与 `Operation::default_route()`。
    - **备选**：三处全部删除，只留 `apply_param_options` 的硬编码。但这样 `maps_to()` 承载的设计意图（参数→option 的映射契约）就丢了，未来新增适配器容易漏。
  - 测试：编译 + grep 确认无引用；`apply_param_options` 的现有行为测试不变。

- [x] **F19 · 低 · `src/config/resolve.rs:184` · task ∩ profile 的输入类型交集为空时不报错**
  - 问题：produce 为空时有明确的 `bail!`，`allowed_inputs` 为空却直接放行。到 `validate_inputs` 才逐个拒绝每份材料，错误信息里的候选列表是空的：`…does not accept (allowed: )`。
  - 方案：应当在 resolve 阶段就报「这个 profile 和这个 task 的输入类型没有交集」。（finding 原文指明方向。）
  - 测试（※补全）：构造交集为空的 task/profile 组合 → 明确报交集错误而非空候选列表。

- [x] **F20 · 低 · `src/plan.rs:580` · dry-run 的「参数来源」只覆盖三个参数**
  - 问题：`describe_param_sources` 只报 model、max_tokens、to，缺 temperature、voice、speed、count、size。而且 max_tokens 的来源被硬编码成 Cli——profile 里设的值根本不显示。`resolve.rs:255` 明明算出了正确的 `ParamSource`，转手就 `Some((v, _))` 丢掉了。issue #24 把「参数来源」列为 dry-run 的核心价值之一。
  - 方案（※补全）：保留并传递 `resolve.rs:255` 算出的 `ParamSource`，`describe_param_sources` 覆盖全部参数（model、max_tokens、temperature、to、voice、speed、count、size）。
  - 测试（※补全）：profile 设 max_tokens/temperature → 来源显示 profile。

- [x] **F21 · 低 · `src/plan.rs:206` · 硬编码的 50 没有引用 `history::DEFAULT_KEEP`**
  - 问题：`record_history: !cli.no_history && cfg.settings.history_keep.unwrap_or(50) > 0`。数值目前和 `DEFAULT_KEEP` 一致，改常量时会分叉。
  - 方案（※补全）：替换为 `history::DEFAULT_KEEP`。
  - 验证（※补全）：编译 + 现有测试。

- [x] **F22 · 低 · `src/domain.rs:383` · `AppError::chain()` 与它的文档不符**
  - 问题：文档写「完整信息加上每一层 cause，每行一个」，实现只展开一层 source，且用 `": "` 连接而不是换行。
  - 方案（※补全，倾向改实现使符合文档）：逐层展开 `source()`、每行一个（或改文档，二选一）。
  - 测试（※补全）：嵌套 error 的 chain 输出为多行。

- [x] **F23 · 低 · `src/config/mod.rs:211, :279` · `config check` 对占位模型名报 ok**
  - 问题：`config init` 写入 `model = "YOUR_MODEL"`，`check()` 只判断 `model.is_none()`，于是一个必然在运行时失败的配置通过了校验。而 `config init` 的输出里第 3 步恰好就是「用 `aido config check` 验证」。
  - 方案（※补全）：`check()` 识别 `YOUR_MODEL` 占位并报「未配置模型」。
  - 测试（※补全）：init 后 check → 非 ok。

- [x] **F24 · 低 · `Cargo.toml:20` · `src/config/mod.rs:160` · edge-tts 没做 feature gate，却成了零配置默认路由**
  - 问题：这个 PR 引入了 29 个新的传递依赖，其中包括 `aws-lc-sys`——一个需要 cmake 才能构建的大型 C/汇编加密库（PR 自己在 Cargo.toml 注释里承认运行时实际用的是 ring，它是被 kothok 的默认特性拖进来的）。所有用户都要付这个构建成本，即使从不用 TTS。另一面是行为：`default_config()` 把 speech 路由到 edge-tts，意味着无配置状态下 `aido tts --text "..."` 会把文本发给微软的非官方端点，用户没有任何显式选择。这值得在 README 里明说，而不只是在配置样例的注释里提一句。
  - 方案（※补全）：kothok 改 `default-features = false` 关掉非必要传递依赖（实现时对照 Cargo.toml 注释确认）；README 明说零配置 TTS 走微软非官方端点（行为 + 隐私）。
  - 验证（※补全）：`cargo tree` 无 aws-lc-sys、测试全绿。
  - 状态（2026-09-11 修复）：依赖侧经实测**不可行**——`kothok-edge-tts 0.2.10` 自身声明零 feature（`default-features = false` 为 no-op），aws-lc-sys 由它对 `tokio-rustls` 的默认特性传递引入（`tokio-rustls default = [logging, tls12, aws_lc_rs]`），Cargo 无下游关闭他人默认特性的机制；上游 0.2.10 已是最新版。README 已补零配置默认路由的隐私说明与改用 OpenAI 语音的方法（快速开始 + Edge TTS 两节）；测试全绿。

- [x] **F25 · 低 · PR description · PR 描述里的测试数字过期**
  - 问题：描述写「71 单元 + 71 集成测试」，实际是 116 + 97。后续 4 个 commit 补了测试但没更新描述。
  - 处置（※补全）：PR #25 已合并（`50c6559`），PR 描述本身无处再改；记录实际数字即可（追加审阅时点为 146 单元 + 121 集成 = 267 全绿）。
  - 状态（2026-09-11 本 TODO 收尾时点）：本轮 31 条修复全部落盘后实测 **177 单元 + 160 集成 = 337 通过、0 失败、1 忽略**（忽略项为既有的 live Edge 端点用例）。

- [x] **F26 · 低 · `src/api/transport.rs:47` · `http://` 的远程 `base_url` 会明文发送 API key，无任何提示**
  - 问题：`normalize_base_url` 接受 http 是对的（本地推理服务器就是 http）。但对非 loopback 的 http 地址，密钥走 Authorization 头明文出去，连一行 warning 都没有。
  - 方案：`transport.rs` 在 `Client::new` 里（而不是 `normalize_base_url` 里，那是纯函数）判断：scheme 为 http、host 不是 localhost/127.0.0.0/8/::1、且 `api_key.is_some()` → stderr 一行 `warning: 凭据将以明文发送到 {host}（base_url 用的是 http://）`。不阻断，只提示。`plan::describe` 里也加一行同样的提醒，让 dry-run 就能看到。
  - 测试（※补全）：http 非 loopback + key → dry-run 输出含 warning。

---

# 第二部分 · 追加审阅 · 合并 af6f9c5（2026-09-11）

## 背景与结论

- PR #25 合入后，其上又开发并合并了两个分支：`246375b`（**per-part 批处理**：`ocr`/`translate` 声明 `per_part = true`，每个文件独立走一遍处理策略，新退出码 6）与 `4278212`（**glob 与单层目录展开**：`aido ocr "shots/*.png"`、`aido ocr shots/`，新增 `glob = "0.3"` 依赖）。追加审阅对象是合并点 `af6f9c5`，行号对应该提交。
- 规模：+1965 / −145，3 commits，267 全绿（146 单元 + 121 集成），clippy 无警告，7 findings。
- **结论：无阻断项。** 合并本身干净——相对两个父提交各只引入对侧改动，两边都碰过的 `plan.rs` 两处改动完好。批处理的关键性质都有集成测试锁住：合并 gate 按 part 隔离（切片去重带不跨文件泄漏）、失败 part 丢弃半成品且跳过剩余步骤、同 stem 命名去重、全败退出 4 不交付、部分失败退出 6 交付幸存者、失败清单经 warnings 落历史（`app.rs:222`）。`catch_unwind` 兜底 glob 0.3 的非 UTF-8 panic 成立（未设 `panic = "abort"`）。
- 新增问题严重度：中 2（F27 F28）、低 5（F29–F33）。

## 对旧 finding 的状态说明

- **F06 只在 per-part 路径上被解决**：批处理的中途失败改为「失败 part 单独记入、幸存者照常交付、整体退出 6」，方向正确；但单文档 chunk（长文翻译）中途失败仍丢弃已流出内容且不留历史——原样未修。**不要因为批处理有失败隔离就认为 F06 已闭合。**
- 其余 F01–F26 这两个分支未顺带修复，状态不变。

## 新增问题 F27–F33

- [x] **F27 · 中 · `src/runner.rs:340` · 批处理把一切非 Complete 回复都报成 "the reply was truncated"，原始原因丢失**
  - 问题：`truncated` 的判据是 `reply.status != GenerationStatus::Complete`，而 `GenerationStatus` 还有 `Incomplete { reason }`、`Failed`、`Cancelled` 等变体。批处理里这些一律落成固定文案 `"the reply was truncated"`——`Incomplete { reason }` 携带的原始 reason（finish_reason 映射出的真实原因）被丢弃，`Failed`/`Cancelled` 也被误报成「截断」。失败分类与退出码不受影响（都按该 part 失败处理），但诊断信息降级：用户看到「截断」，实际可能是别的原因。
  - 方案：`src/runner.rs:342` 的错误文案从 `reply.status` 取值：`Incomplete { reason }` → 用 reason；`Failed`/`Cancelled` → 变体名；「truncated」只留给真正 length 截断映射出的那类状态。`close_group!` 里「no usable text」的文案不动。
  - 测试：`tests/per_part.rs` 的 `a_truncated_part_fails_alone_and_the_rest_deliver` 改断言具体 reason；加一个 `Failed` 状态的用例断言不出现 "truncated"。

- [x] **F28 · 中 · `src/plan.rs:153` · dry-run 会展示一个真实执行会被拒绝的交付计划**
  - 问题：batch 的三条交付目标规则（`-o` 拒绝、缺 `--out-dir` 拒绝、stdout/clipboard 拒绝）全部包在 `if batch && !cli.dry_run` 里。于是：

    ```console
    $ aido ocr a.png b.png --dry-run
    destinations: stdout            ← 计划说发 stdout
    $ aido ocr a.png b.png
    error: 2 inputs are processed one request each; use --out-dir ...   ← 真实执行退出 2
    ```

    代码注释表明这是有意的（"Delivery-target rules don't bind a --dry-run: it only shows the plan"），但对「先 dry-run 验证再跑」的脚本化用法，dry-run 通过不再等于能跑，而且计划里展示的是一个必被拒绝的目标。

  - 方案（两条取舍选一；**所选：检查照常跑（推荐）**）：
    - 检查照常跑：去掉 plan.rs:153 三处的 `!cli.dry_run`。这三条是纯预检、无副作用，`validate_outputs`/`resolve_destinations` 在 dry-run 下本来就照常执行，batch 没有理由例外。代价是 `aido ocr a.png b.png --dry-run`（无 out-dir）从「展示计划」变为退出 2。
    - 保留豁免 + 提示：维持现状，`describe()` 在 `batch` 且交付目标不合法时追加一行「实际执行将拒绝：需 --out-dir」。
  - 测试：按所选方向更新 `tests/per_part.rs` 的 `dry_run_shows_the_batch_plan_without_requesting`。

- [x] **F29 · 低 · `src/app.rs:531` · `src/history.rs` · `failed_parts`/`parts_total` 不进运行记录，恢复交付的 JSON 报告不对称**
  - 问题：`FailedPart` 只活在 `RunOutput` 与 `--json` 报告里；`RunRecord` 没有对应字段，历史里只能靠 warnings 字符串（`part 'a.png' failed: …`）间接还原失败清单。`deliver_restored` 给 `DeliverArgs` 传 `failed_parts: &[]`，于是同一个 run：原始执行的 `--json` 报告带 `error.kind = "partial"` 与 `failed_parts` 数组，`aido last --json` 恢复交付的报告两者皆无。恢复交付本身成功没有错，但两次报告对「这次跑成没跑全」给出不同答案，脚本会误判。
  - 方案：给 `RunRecord` 加 `failed_parts: Vec<(String, String)>` 字段并在 `deliver_restored` 回填。
  - 测试（※补全）：`last --json` 恢复报告含失败清单。

- [ ] **F30 · 低 · `src/output.rs:474` · 目录 manifest 新增 provenance 字段，version 仍是 1**
  - 问题：per-part 给 manifest 的产物条目加了 `"provenance"` 字段，但 manifest 顶层 `version: 1` 未动。追加字段对宽松读者向后兼容，但对持有严格 schema（如 `additionalProperties: false`）的下游是破坏。
  - 方案：要么升 version，要么在 README 的 manifest 说明里写明该字段自 0.3.0 起追加、允许缺省。
  - 验证（※补全）：文档更新（或 version 变更 + 兼容说明）。

- [ ] **F31 · 低 · `src/input.rs:352` · 未闭合 `[` 的报错语义与 shell 不一致**
  - 问题：文件名含元字符时，「字面文件存在则优先」的兜底（`expand_glob` 开头的 `is_file()` 检查）让常见场景正确，比 shell 还好。但文件不存在时，bash 把未闭合的 `[` 当字面量、报 No such file；aido 的 glob 解析直接报 "invalid glob pattern"。`aido ocr "shot[1.png"`（漏写 `]`）看起来像「模式写错了」，其实是「文件没找到」。两种行为都说得通，只是错误分类可能误导排查方向。
  - 方案：调整 invalid pattern 的报错文案/分类，指向「文件未找到」方向。
  - 测试（※补全）：`ocr "shot[1.png"`（文件不存在）→ 报 file-not-found 方向的错误。

- [ ] **F32 · 低 · `src/processors/perpart.rs:43` · 共享材料随每个 part 重复发送且无护栏**
  - 问题：`--text`/stdin/剪贴板作为共享上下文整体附进每个 part 的请求。术语表场景这是文档化的正确行为；但没有类似 chunk 分块的退避护栏——2 个文件 + 30 个 `--text` 大段时，每个请求都扛全部共享材料，可能直接顶到模型上下文上限。输入总预算（128 MB）在 gather 阶段只算一次、不因重复放大，所以没有任何机制拦截。
  - 方案：建议后续在单请求材料接近上限时给出 warning 或拒绝。
  - 测试（※补全）：超限组合 → 有 warning。

- [ ] **F33 · 低 · `src/runner.rs:247` · group 生命周期隐式假设 steps 按 part 连续排列**
  - 问题：runner 以「`g.id != step.part` 即边界」来 close group，要求同一 part 的 steps 必须连续。当前 `perpart::plan_steps` 按 part 顺序 append，保证成立；但 `RequestStep.part` 是公开字段，未来任何 processor 或对 steps 的重排/过滤一旦交错 part，行为不是报错而是静默错误：同 part 被拆成多个 group、同 stem 产出两个同名 artifact，在 `write_directory` 里互相冲突。
  - 方案：建议在 plan 构建后加一条顺序断言（part id 非降序），把违约变成显式 usage 错误。
  - 测试（※补全）：构造交错 steps（单测）→ 报错。

---

## 完成标准

- 全部 33 条勾选；`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test` 全绿。
- 批次 2 的 F02/F09 连带更新 README 行为说明（原文要求同 commit）。
- TODO.md 随每次勾选一起提交。
- ⚠ 批次 4 的 F15/F16/F19 与批次 5 整批（F18、F20–F25）方案为补全，动手前建议人工复核（或补全 review 文档）。
