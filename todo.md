# Web UI 待办（P1 进行中）

> 本文件跟踪 aido Web UI（`aido ui`）的未完成工作。与 `TODO.md`（PR-25 审查追踪）无关。
> 已落地：P0 全部（8 个提交，见 `git log 17791f6..923f847`）、P1 的配置读写（`877c5ce`）、配置页（`5ae204b`）、前端审查修复（`e503feb`）。

## 1. P1-c：链构建器服务端 —— 代码已写完，待验证 + 测试 + 提交（工作树未提交）

工作树里已有改动（未提交）：

- `src/chain.rs`：`execute_with`（事件 sink 穿透每个阶段的 runner 调用）；`describe_chain` 改为接收 `terminal`/`env` 参数（UI 传非 TTY/无剪贴板环境，CLI 传 real）。
- `src/app.rs`：`run_chain` 的 dry-run 调用点适配新签名。
- `src/ui/invoke.rs`：`StageRequest`/`ChainRequest`（白名单，deny_unknown_fields）；`chain_argv` 用 `--then` 形式组装 argv（避开 chain 规格串的无转义问题）；`parse_chain` 走 normalize → parse_syntax → prepare 全链路校验。
- `src/ui/runs.rs`：抽出 `spawn_run_thread` 公共骨架（注册/线程/运行时/注销）；新增 `spawn_chain`（取消快照镜像 run_chain 的 Ctrl+C 占位：on_started/on_progress 捕获已付费产物，取消时写入 cancelled 记录）；`finish` 收敛为 `RunContext` 结构体参数，链与单运行共用。
- `src/ui/api.rs`：`POST /api/chain`（202 + run_id）、`POST /api/chain/preview`（describe_chain 文本 + 每阶段结构化摘要）；`parse_multipart` 泛型化。

待做：

- [ ] `cargo fmt --check` + `cargo clippy --all-targets --locked -- -D warnings` + `cargo test --locked` 全绿（最后一次 clippy 修复后被中断，未跑完验证）。
- [ ] `tests/ui.rs` 补链集成测试：
  - `POST /api/chain` 两段链（ask → summarize，MultiServer 两个 canned 回复）→ SSE done 帧带 `task: "ask|summarize"`、`artifacts` 2 件、`stages` 2 条、`last_stage_len` 1；`GET /api/runs/{id}` 一致；历史一条记录。
  - `POST /api/chain/preview` 合法链 → 文本含 `chain:`；junction 类型不符（如 translate → transcribe）→ 400，报错含 "stage 1 … → stage 2"。
  - 链取消：慢 provider + cancel → cancelled 帧 + 详情 `status: cancelled`，已完成阶段的产物在记录里。
- [ ] 提交（`feat(ui): chain API — the --then pipeline over SSE`）。

## 2. P1-d：链构建器前端（`ui/src/pages/Chain.tsx`）

- [ ] `/chain` 页：横向管线卡片（每环：任务选择、输入/输出类型徽标、`ParamForm` 参数、profile datalist、删除/上移/下移、＋加环）；材料区只属第 1 环（复用 Dropzone）。
- [ ] 预览按钮 → `POST /api/chain/preview`，describe 文本 + 每阶段 chips；类型检查错误（junction 报错）在对应两环之间的连线上标红显示（报错文案已带 "stage N (task) → stage M (task)" 前缀，前端可解析定位）。
- [ ] 运行 → SSE（step 事件的 label 自带 `chain k/n` 前缀）→ done 帧渲染全部阶段产物：用 `last_stage_len` 区分中间产物与最终交付物（中间产物标记「阶段 N」徽标）。
- [ ] 模板区：README 四个经典链（截图→翻译→配音 / 语音同传 / 每日简报 / 长图提问）一键填充。
- [ ] 等效命令：`--then` 形式常驻显示。
- [ ] 导航加「链」；`npm run build` + 浏览器端到端验证 + 提交。

## 3. P1-e：批处理实时网格

- [ ] `src/runner.rs`：`RunEvent::Step` 增加 `part: Option<String>`（批处理时填输入文件名；emit 处 `step.part.and_then(|id| plan.inputs.get(id)).map(|p| p.name.clone())`）。
- [ ] `ui/src/components/StreamView.tsx`：step 帧带 part 时显示逐文件状态 chips（完成 N/M 高亮、当前文件名）。
- [ ] `tests/ui.rs`：两文件批处理（slow provider 逐个延迟回复）断言至少一个 step 帧带 part 名；done 帧的 failed_parts 网格已有覆盖。
- [ ] 提交。

## 4. P1-f：收尾

- [ ] README「Web UI」节补链构建器与批处理网格说明。
- [ ] 全量验证：`cargo fmt/clippy/test --locked`、`--no-default-features`、`cd ui && npm run build`、重启演示服务器浏览器冒烟（运行/链/历史/详情/配置五页）。
- [ ] （可选）`docs/` 或 PR 描述里写 P0+P1 的 API 契约小结。

## 5. 审查收尾（独立代理）

- [ ] 前端审查 ✅ 已完成（18 项发现，已修复于 `e503feb`；报告在会话记录中）。
- [ ] 服务端 Rust 审查：代理仍在后台运行，等完成后核对发现（预期关注点：argv 注入面、临时上传清理、guard 解码、run 线程 panic 路径、MSRV）。
- [ ] 集成面审查（tests/CI/README/契约）：首次运行因账号限流失败（1302），需重试；重试用与首次相同的提示词。
- [ ] 三份报告去重合并 → 按 TODO.md 惯例编号（高/中/低 + 文件:行 + 修法），修复项落独立提交。

## 6. 后续（P2，未排期）

- watch 仪表盘（UI 只能管理 UI 启动的守护进程；实时动态流 + 新建守护）。
- 任务创建向导（表单 → TOML 预览 → 写入 tasks 目录；需要 `tasks::load_all` 的 OnceLock 加 reload）。
- 服务器端交付（`-o`/`--out-dir` 白名单 + 交付状态展示）、历史 zip 打包下载、`--produce/--format` 高级项、UI 英文语言包。

## 环境备忘

- 演示服务器（本轮调试用）：`http://127.0.0.1:8710/?t=e2e`，假 provider 在 127.0.0.1:9931（`/tmp/aido-ui-e2e/`），配置已含 `default_profile = "test"`。
- 前端开发回路：`AIDO_UI_TOKEN=dev aido ui --port 8710 --no-open` + `cd ui && npm run dev`（vite 代理已剥 Origin）。
- 测试纪律：fmt/clippy/test 全绿才提交；`scripts/check_no_inline_tests.py` 拦内嵌测试；单测放 `src/<module>/tests.rs`。
