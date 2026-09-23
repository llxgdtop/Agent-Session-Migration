# Agent Session Hub

**让你的 AI 编程会话,不再被工具锁死。**

[English](README.md) | [简体中文](README.zh-CN.md)

<!-- TODO: 截图。截取主界面保存为 docs/screenshot.png 后取消注释:
![Agent Session Hub 主界面](docs/screenshot.png)
-->

在 Claude Code 里聊到一半的工作,迁移到 Codex 或 ZCode 接着干——消息、推理过程、工具调用记录完整保留,无需从头交代背景。三个工具之间任意方向互迁。

## 它能做什么

- **会话一览**:自动读取本机所有 Claude Code、Codex、ZCode 会话,合并为单一列表,按最近活跃排序;卡片带来源徽章,支持按来源筛选与搜索。点开任意会话即可预览完整消息流。
- **一键迁移**:任意会话可转换为三家工具中任意一家的格式(六个方向全部支持)。迁移只新建文件或新数据行,既有会话绝不被修改。
- **无缝续聊**:迁移到 Claude Code 或 Codex 后,复制界面给出的续聊命令(或一键在 Terminal 中打开),带着全部上下文继续工作;迁移到 ZCode 则一键打开 ZCode 桌面应用,会话出现在任务列表中。

## 快速开始

环境要求:macOS 11 及以上,较新的 Rust 工具链。Windows 与 Linux 构建在路线图中。

```sh
cargo build --release
./scripts/build-app.sh          # 生成 dist/AgentSessionHub.app
open dist/AgentSessionHub.app   # 或在 Finder 中双击打开
```

迁移一个会话:

1. 在列表中选择会话,确认预览内容。
2. 点击目标工具对应的「迁移到 …」按钮。若目标工具正在运行,应用会先警示,经你确认后才执行写入。
3. 把界面给出的续聊命令粘贴到终端——或让应用直接帮你打开——开始续聊。

## 工作原理

三家工具各有自己的会话存储格式:Claude Code 与 Codex 为 `~/.claude/projects` 和 `~/.codex/sessions` 下的 JSONL 文件,ZCode 为 `~/.zcode/` 下的一对 SQLite 数据库。Agent Session Hub 通过三段式流水线将其统一:

```
reader(解析源格式)--> 统一中间表示 IR --> writer(生成目标格式)
```

- `crates/hub-core` —— 核心库:各工具的 reader、统一中间表示、各工具的 writer,以及续聊命令生成(launcher)与写入前安全检查(safety)。
- `crates/hub-app` —— 原生桌面界面(基于 egui/eframe)。

全部在本地运行,应用不发起任何网络请求。

## 隐私与安全

- **纯本地运行**:不联网、无遥测、不上传任何数据。
- **只增不改**:迁移仅新建文件或新数据行,绝不修改或删除既有会话。
- **原子写入**:文件目标先写临时文件再改名落盘;ZCode 目标每个库单事务写入。失败不会留下半成品会话。
- **ZCode 写入护栏**:写入前校验数据库 schema 版本(不认识的版本直接拒绝写入),并对两个库做带时间戳的备份;备份失败即中止迁移。
- **运行检测**:目标工具运行中时写入可能冲突或损坏数据,应用会检测目标工具进程,先警示、经确认后才放行。

## 路线图

- 深色模式
- 拖拽迁移
- Windows 与 Linux 构建
- 迁移前敏感信息脱敏
- 跨设备会话传输

## 参与贡献

欢迎提交 issue 与 pull request。本地开发:

```sh
cargo test                  # 运行全部测试
cargo run -p hub-app        # 开发模式启动
cargo clippy --all-targets  # 静态检查
```

转换逻辑在 `crates/hub-core`,界面在 `crates/hub-app`。

## 许可

Apache-2.0,详见 [LICENSE](LICENSE)。
