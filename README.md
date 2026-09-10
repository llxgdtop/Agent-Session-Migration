# Agent Session Hub(MVP)

Claude Code → Codex 会话迁移工具:把 `~/.claude/projects` 下的会话迁移为 Codex rollout,
迁移后用 `codex resume <SESSION_ID>` 接着聊。MVP 范围与业务规则见
`docs/MVP-DEVELOPMENT.md`(仓库外:../docs/MVP-DEVELOPMENT.md)。

## 结构

- `crates/hub-core` — 纯库:reader(Claude JSONL 解析)→ IR → mapper → writer(Codex rollout 写入)+ launcher(命令生成)
- `crates/hub-app` — egui 壳:左侧会话列表、右侧消息流预览、"迁移到 Codex" 按钮、成功后展示可复制的 resume 命令

## 构建与运行

```sh
cargo test          # 全部测试
cargo run -p hub-app  # 启动界面(唯一入口)
```

## 运行时行为

- 只读扫描 `~/.claude/projects/**/*.jsonl`,按最新活跃倒序展示;
- 迁移只新增文件:`~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`(临时文件 + rename 原子写),
  绝不修改源文件与目标目录既有文件;
- 目标文件已存在即拒绝写入(幂等键 = 目标文件路径);
- 迁移成功后界面显示 `cd <project_dir> && codex resume <SESSION_ID>`,复制到终端在源项目目录下执行。

## 已知限制(MVP)

- 界面未内置 CJK 字体:egui 默认字体不含中文。本程序会尝试加载 `$HOME/Library/Fonts`
  与 `$HOME/.fonts` 下的字体做回退;若两者皆无字体,中文会显示为方框
  (内容本身不受影响,迁移产物完整)。是否允许读取系统字体目录(如
  `/System/Library/Fonts`)待 owner 决策(自治边界 #9)。
- 不检测 Codex 是否正在运行(已知且接受的风险,见 MVP 文档开篇)。
