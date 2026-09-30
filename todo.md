# Web UI 待办

> 本文件跟踪 aido Web UI（`aido ui`）的工作状态。与 `TODO.md`（PR-25 审查追踪）无关。

## 状态：P2 全部完成（2026-09-30，英文语言包除外——见下）

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
| P1-f README 收尾 | `46f8dd5` |
| P2-a 任务创建向导（tasks 缓存可失效 + POST/DELETE /api/tasks + 任务页） | `8ecd679` |
| P2-b 服务器端交付白名单（out_dir/out_file + 交付状态展示） | `29d4514` |
| P2-c 历史 zip 打包下载 | `ebfeff9` |
| P2-d watch 仪表盘（watch 事件化 + 守护注册表 + /watch 页） | `eff613e` |
| P2-e --produce/--format 高级项 + provenance 时间线 | `87f7cc2` |

- 测试：692 通过 / 0 失败（含 20 个 `tests/ui.rs` 黑盒用例）；fmt/clippy/`--no-default-features`/npm typecheck 全绿。
- 每块功能都在浏览器里实测过再提交（向导增删改与跨页 reload、交付落盘与状态、zip 内容、守护的实时日志与协作停止、produce/format 的等效命令与拒绝文案、时间线映射）。

## 已知边界（有意为之，改前先想清楚）

- 服务器端交付是白名单不是路径：`out_dir`/`out_file` 只能是 aido 交付目录（`AIDO_DELIVERY_DIR`，默认 `data_local/aido/deliveries/`）内的一个名字；请求永远不能指定任意路径。链在 v1 仍以浏览器为目的地（不带服务器交付）。
- watch 仪表盘只管理本 UI 启动的守护进程；CLI 另起的 `aido watch` 是另一个进程，注册表看不到（有意）。UI 守护的交付固定为守护目录的子目录（默认 `out/`）；停止是协作式的——当前文件跑完才生效。
- SSE 无回放（运行与守护同理）：晚订阅者错过早期帧，done 帧 / 详情端点 / 列表计数是兜底。
- UI 守护（watches）的 `WatchRequest` 不含 `--produce/--format`/交付名；任务白名单其余项与运行一致。
- 前端 i18n：无语言包设施，全部中文硬编码（见下）。

## P2 未做：UI 英文语言包（有意押后）

范围问题没有答案之前不动手：全文案抽取（约 2100 行 JSX 内联中文 + 服务端少量中文错误文案）需要先决定 (a) 覆盖面——全量还是核心 UI；(b) 切换与持久化方式；(c) 服务端消息（400/404 文案目前多为英文，少量中文）是否一并翻。半翻译的界面比纯中文更糟，所以宁可押后。做的时候顺带引入 `ui/src/i18n.ts` 词典 + localStorage 语言切换，`StatusBadge`/`TaskPicker`/`History` 的映射表先行。

## 环境备忘

- 演示服务器：`http://127.0.0.1:8710/?t=e2e`。完整启动（两个进程）：
  `python3 /tmp/aido-ui-e2e/fake_provider.py`（127.0.0.1:9931，假 chat 服务）+
  `AIDO_UI_TOKEN=e2e AIDO_CONFIG=/tmp/aido-ui-e2e/config.toml AIDO_HISTORY_DIR=/tmp/aido-ui-e2e/history ./target/debug/aido ui --port 8710 --no-open`。
  注意：8710 上若残留旧会话的服务器会顶掉新进程的端口，先 `pkill -f "aido ui"`。
- 前端开发回路：`AIDO_UI_TOKEN=dev aido ui --port 8710 --no-open` + `cd ui && npm run dev`（vite 代理已剥 Origin）。
- 纪律：每提交 fmt/clippy/test 全绿；测试放 `src/<module>/tests.rs` 或 `tests/`；慢 provider 的 bodies 数必须等于实际请求数（监听器已改非阻塞，多出的 accept 30 秒超时后失败而非挂死）；黑盒测试里换 provider 要连 UI 服务器一起换（config 记死了 provider 地址）。
