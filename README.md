# kimi-code-hud-rs

自定义底部状态栏（HUD）for [Kimi Code CLI](https://www.kimi.com/) —— [kimi-code-hud](https://github.com/FinbackYu/kimi-code-hud)（Node.js 版）的 Rust 移植，单一可执行文件，无 Node 运行时依赖。

在终端 TUI 底部显示：模型与思考强度、Git 分支与脏标记、生成速度（TPS / TTFT / `gen` 计时）、`/compact` 压缩计时、会话缓存命中率、Kimi 托管订阅额度（5h / 7d 柱条 + 百分比 + 重置倒计时）、模式徽章（`[yolo]` `[auto]` `[plan]` `[swarm]`）。

与 Node 版的差异（有意为之）：

- **不含第三方 provider 支持** —— 没有 DeepSeek / OpenAI / Anthropic 的余额查询与会话成本估算，只有托管订阅（`managed:kimi-code`）额度；
- 不含 goal 徽章与后台任务徽章；
- 配置与缓存目录为 `~/.kimi-code-hud-rs/`，与 Node 版的 `~/.kimi-code-hud/` 互不干扰，两者可以并存；
- 插件（`/plugins`）托管方式未移植，只支持手动安装。

## 构建

要求 Rust 1.85+（edition 2024）。

```bash
cargo build --release
```

产物为 `target/release/kimi-code-hud-rs(.exe)`。

## 安装

```bash
cargo build --release
./target/release/kimi-code-hud-rs --install
```

`--install` 会先备份再修改两个宿主文件（均幂等，保留其他设置）：

- `~/.kimi-code/tui.toml`：在 `[status_line]` 段写入 `command = "<可执行文件绝对路径>"`；
- `~/.kimi-code/config.toml`：追加一段 START/END 注释包裹的受管 `SessionStart` hook（`<可执行文件> --sync-status-line`）。宿主某些升级会重写 `tui.toml`、抹掉 `[status_line]`，而 `config.toml` 的 hooks 在升级中保留，于是每次会话启动时 hook 都会自检并把条目修回。

**重启 Kimi Code 或运行 `/reload-tui` 生效。** 如果 `[status_line]` 已配置了别的命令，hook 不会覆盖它。

## 临时关闭 / 开启 / 卸载

```bash
./target/release/kimi-code-hud-rs --off         # 写入 disabled 旗标并摘除 tui.toml 条目（hook 休眠不复活）
./target/release/kimi-code-hud-rs --on          # 清旗标、写回条目、确保 hook 在位
./target/release/kimi-code-hud-rs --uninstall   # 移除条目与 hook 块
```

## 配置

- `~/.kimi-code-hud-rs/config.json`：`{"layout":"compact"|"normal"}`（默认 `normal`）；`"disabled": true` 是 `--off` 写入的开关旗标；
- 环境变量 `KIMI_HUD_RS_LAYOUT` 优先于配置文件；
- `NO_COLOR` / `KIMI_HUD_RS_NO_COLOR`：禁用全部 ANSI 颜色；
- `KIMI_HUD_RS_THEME=dark|light`：手动固定配色主题；缺省跟随 `tui.toml` 顶层 `theme`，`auto` 经 `COLORFGBG` 判定、回退 dark；
- `KIMI_CODE_HOME` / `KIMI_HUD_RS_HOME` / `KIMI_HUD_RS_TUI_TOML` / `KIMI_HUD_RS_CONFIG_TOML`：路径覆盖（测试与沙箱用）。

两档布局（超过 200 可见字符自动 normal → compact 降级）：

```
compact: [manual] K3 high │ git:(main*) │ ⚡ 47 │ Cache 92% │ 5h 31% ~2h18m
normal:  [manual] K3 high │ kimi-code-hud git:(main) │ ⚡ 47 t/s · TTFT 1.3s │ Cache 92% │ 5h ███░░░░░░░ 31% ~2h18m · 7d ██░░░░░░░░ 25% ~3d2h
```

## 原理

Kimi Code 的 `~/.kimi-code/tui.toml` 支持 `[status_line]` 自定义命令：宿主每秒最多一次通过 stdin 传入 JSON 快照（model、cwd、gitBranch、permissionMode、sessionId 等），命令 stdout 的第一行接管 Footer 第一行，且必须在 300ms 内完成。本工具内部预算 220ms，所有错误静默降级、绝不打印诊断。

数据来源与 Node 版一致：

| 段 | 来源 |
|---|---|
| 模型 / 分支 | stdin 快照 + PATH 解析（拒绝工作区内命中）的 `git status --porcelain=v1 --branch`，结果跨进程缓存 15 秒（cwd 只存 SHA-256） |
| TPS / TTFT / gen / 压缩计时 / Cache / thinking / swarm | 增量解析 `~/.kimi-code/sessions/*/session_<id>/agents/*/wire.jsonl`（main + 全部 subagent）。每 agent 维护持久化字节游标（`~/.kimi-code-hud-rs/metrics-<sessionId>.json`），每帧只读新增字节（≤1MiB），半行 JSON 跨进程无损拼接，尾部指纹检测文件原地重写。速度样本带事件时间戳，多 agent 活跃时聚合为舰队总速 `⚡ 156 t/s (3 agents @52)` |
| thinking 强度 | wire 事件（`llm.request` / `config.update` / `profile.bind`）> 按会话固定的快照（`thinking-<sessionId>.json`，防止其他会话 `/effort` 改全局配置影响本会话）> `config.toml` 的 `[thinking]` 与模型表回退推断；未确认值以暗灰显示 |
| 配额（5h/7d） | 官方 `/usages` 配额接口（`api.kimi.com` / `api.kimi.ai`，按 oauth host / base_url 判区，任何非官方地址一律回退默认）。热路径只读 60 秒 TTL 缓存，过期时经文件锁去重后 spawn 分离的 `--refresh-quota` 子进程刷新，绝不阻塞渲染。仅当前模型归因于 `managed:kimi-code` 时显示；`/logout` 后缓存自动删除 |

## 隐私与安全

Kimi access token 仅从 `~/.kimi-code/credentials/` 本地读取（只读不写，续期由 Kimi Code CLI 负责），且发送前经过双重白名单校验：只会发往 `https://api.kimi.com/coding/v1/usages` 或 `https://api.kimi.ai/coding/v1/usages`。持久化状态里只有字节偏移量、token 计数与速度样本，不含任何提示词、回复或工具输出。所有来自外部的动态显示文本在着色前都会剥除 OSC/CSI/ESC 终端控制序列。

## 开发

```bash
cargo test                # 单元测试（47 个）
cargo build --release     # 发布构建
echo '{"model":"K3","gitBranch":"main"}' | cargo run --quiet   # 烟雾测试
```

## License

MIT
