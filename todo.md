# Web UI 待办（P1 进行中）

> 本文件跟踪 aido Web UI（`aido ui`）的未完成工作。与 `TODO.md`（PR-25 审查追踪）无关。

## 已完成（供参考）

- **P0 全部**：`17791f6..923f847`（runner 事件回调、assemble_record 抽取、RunMeta 拓宽、ui 骨架、读 API、run API、SPA、CI/README）。
- **P1 配置读写**：`877c5ce`（GET/PUT /api/config + /api/profiles）、`5ae204b`（配置页 + 运行页 profile 下拉）。
- **前端审查修复**：`e503feb`（SSE 悬挂/双击守卫/closer/404 链接等 18 项）。
- **服务端审查修复**：`3d986f7`（U01 占位文件热修——fresh clone 编不过的发布阻断）、`05a0e5e`（U02 静默 spinner、U03 上传重名/并发目录、U05 panic 守卫、U06 取消记录形状、U07 token URL 编码、U08 body 上限、U09 幂等取消、U10 任务名旗标拒绝）。
- **P1-c 链 API 服务端**：`05a0e5e`（POST /api/chain + preview、chain::execute_with 事件穿透、取消快照、3 个集成测试）。

## 1. P1-d：链构建器前端（下一步，`ui/src/pages/Chain.tsx`）

- [ ] `/chain` 页：横向管线卡片（每环：任务选择、输入/输出类型徽标、`ParamForm` 参数、profile datalist、删除/上移/下移、＋加环）；材料区只属第 1 环（复用 Dropzone，texts 同样进第 1 环）。
- [ ] 预览 → `POST /api/chain/preview`，describe 文本 + 每阶段 chips；junction 类型错误文案已带 `stage N (task) → stage M (task)` 前缀，前端解析后在对应两环连线上标红。
- [ ] 运行 → SSE（step label 自带 `chain k/n`）→ done 帧用 `last_stage_len` 区分中间产物（`stage-N-*` 徽标）与最终交付物。
- [ ] 模板：README 四条经典链一键填充（截图→翻译→配音 / 语音同传 / 每日简报 / 长图提问）。
- [ ] 等效命令 `--then` 形式常驻；导航加「链」；`npm run build` + 浏览器端到端验证 + 提交。

## 2. P1-e：批处理实时网格

- [ ] `src/runner.rs`：`RunEvent::Step` 加 `part: Option<String>`（批处理时填输入文件名；emit 处 `step.part.and_then(|id| plan.inputs.get(id)).map(|p| p.name.clone())`）。
- [ ] `StreamView.tsx`：step 帧带 part 时显示逐文件状态 chips。
- [ ] `tests/ui.rs`：两文件批处理（慢 provider）断言 step 帧带 part 名。提交。

## 3. P1-f：收尾

- [ ] README「Web UI」节补链构建器与批处理网格。
- [ ] 全量验证：fmt/clippy/test、`--no-default-features`、`cd ui && npm run build`、重启演示服务器浏览器冒烟（运行/链/历史/详情/配置）。

## 4. 审查收尾

- [x] 前端审查（18 项，修复于 `e503feb`）。
- [x] 服务端审查（U01–U10；U01 热修 `3d986f7`，U02/03/05/06/07/08/09/10 修复于 `05a0e5e`）。
- [ ] **U04（中，未修）**：per_part 任务（ocr/translate）多文件在 UI 跑不起来——`plan.rs` 的 batch 预检硬性要求 `--out-dir`，而 RunRequest 故意不含交付旗标。两个方向择一：(a) invoke::build_plan 给 headless 路径放宽该检查（batch 产物本就进历史目录）；(b) preview 阶段给 UI 定向报错说明。需要设计决策。
- [ ] 集成面审查（tests/CI/README/契约）：首次因账号限流失败，待重试。
- [ ] 三份报告合并摘要（前端+服务端已修，集成面待出）。

## 5. P2（未排期）

watch 仪表盘、任务创建向导（需 `tasks::load_all` reload）、服务器端交付（`-o`/`--out-dir`）、历史 zip 下载、UI 英文语言包。

## 环境备忘

- 演示服务器：`http://127.0.0.1:8710/?t=e2e`（旧二进制——**重启才有链端点**：`AIDO_UI_TOKEN=e2e AIDO_CONFIG=/tmp/aido-ui-e2e/config.toml AIDO_HISTORY_DIR=/tmp/aido-ui-e2e/history ./target/debug/aido ui --port 8710 --no-open`；假 provider 127.0.0.1:9931 需同时起：`python3 /tmp/aido-ui-e2e/fake_provider.py`）。
- 前端开发回路：`AIDO_UI_TOKEN=dev aido ui --port 8710 --no-open` + `cd ui && npm run dev`。
- 纪律：每提交 fmt/clippy/test 全绿；测试放 `src/<module>/tests.rs` 或 `tests/`；`slow_chain_provider` 的 bodies 数必须等于实际会发出的请求数（多出的 accept 会永久阻塞，listener 是阻塞模式）。
