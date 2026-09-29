# Web UI 待办

> 本文件跟踪 aido Web UI（`aido ui`）的工作状态。与 `TODO.md`（PR-25 审查追踪）无关。

## 状态：P0 + P1 全部完成（2026-09-29）

| 里程碑 | 提交 |
|---|---|
| P0：runner 事件、记录抽取、ui 骨架、读/运行 API、SPA、CI/README | `17791f6..923f847` |
| 配置读写 API + 配置页 | `877c5ce`、`5ae204b` |
| 前端审查修复（18 项） | `e503feb` |
| U01 占位文件热修（发布阻断） | `3d986f7` |
| P1-c 链 API + 服务端审查修复 U02–U10 | `05a0e5e` |
| 集成面审查修复（4 中 4 低） | `1ef1dd0` |
| P1-d 链构建器前端 + useRunStream 抽取 | `ed1d1b7` |
| P1-e 批处理实时网格 + U04 解除 | `5e26f4e` |
| P1-f README 收尾 | 本提交 |

- 测试：683 通过 / 0 失败（含 15 个 `tests/ui.rs` 黑盒用例）；fmt/clippy/`--no-default-features`/npm typecheck 全绿。
- 三份独立审查（前端 / 服务端 / 集成面）的全部可行动项已修复；U04（per_part 批处理在 UI 的 `--out-dir` 预检）以占位目录方案解除（见 `invoke.rs` build_plan 的注释）。

## 已知边界（有意为之，改前先想清楚）

- UI 运行无服务器端交付（`-o`/`--copy`/`--out-dir` 不在请求白名单）：浏览器即目的地，产物走历史。
- SSE 广播无回放：晚订阅者错过早期帧（done 帧 + 详情端点是兜底，前端 hook 已实现）。
- `tasks::load_all` 的 OnceLock：运行中新增自定义任务 TOML 需重启 `aido ui`。
- 链的中间阶段不支持 per-part 批处理（junction 只交接一件文本产物，plan 期拒绝——CLI 同规则）。

## P2 候选（未排期）

- watch 仪表盘（UI 只能管理 UI 启动的守护进程；实时动态流 + 新建守护）。
- 任务创建向导（表单 → TOML 预览 → 写入 tasks 目录；需给 `tasks::load_all` 加 reload）。
- 服务器端交付白名单（`-o`/`--out-dir` + 交付状态展示）、历史 zip 打包下载。
- `--produce/--format` 高级项、UI 英文语言包、provenance 时间线可视化。

## 环境备忘

- 演示服务器：`http://127.0.0.1:8710/?t=e2e`。完整启动（两个进程）：
  `python3 /tmp/aido-ui-e2e/fake_provider.py`（127.0.0.1:9931，假 chat 服务）+
  `AIDO_UI_TOKEN=e2e AIDO_CONFIG=/tmp/aido-ui-e2e/config.toml AIDO_HISTORY_DIR=/tmp/aido-ui-e2e/history ./target/debug/aido ui --port 8710 --no-open`。
- 前端开发回路：`AIDO_UI_TOKEN=dev aido ui --port 8710 --no-open` + `cd ui && npm run dev`（vite 代理已剥 Origin）。
- 纪律：每提交 fmt/clippy/test 全绿；测试放 `src/<module>/tests.rs` 或 `tests/`；慢 provider 的 bodies 数必须等于实际请求数（监听器已改非阻塞，多出的 accept 30 秒超时后失败而非挂死）。
