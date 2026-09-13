# Agent Session Hub

**让你的 AI 编程会话,不再被工具锁死。**

在 Claude Code 里聊到一半的工作,一键迁移到 Codex 接着干——消息、推理过程、工具调用记录完整保留,无需从头交代背景。

## 它能做什么

- **会话一览**:自动读取本机所有 Claude Code 会话,按最近活跃排序,点开即可预览完整消息流
- **一键迁移**:选中任意会话,转换为 Codex 会话格式写入本机,源文件不被修改
- **无缝续聊**:迁移完成即给出续聊命令,复制到终端执行,Codex 就带着全部上下文继续工作

## 使用

1. 构建并启动应用:

   ```sh
   cargo build --release
   ./scripts/build-app.sh        # 生成 dist/AgentSessionHub.app,双击即可打开
   ```

2. 在左侧列表选择一个会话,确认预览内容
3. 点击「迁移到 Codex」
4. 复制界面给出的命令,粘贴到终端执行——开始续聊

## 隐私与安全

- **纯本地运行**:不联网、不上传任何数据
- **只增不改**:迁移仅新建文件,绝不修改你的 Claude Code 与 Codex 既有数据
- **原子写入**:先写临时文件再落盘,失败不会产生半成品

## 支持范围

当前版本支持 **Claude Code → Codex** 单向迁移。

路线图:

- 支持反方向迁移(Codex → Claude Code)
- 支持 ZCode(读入与写出)
- 三个工具任意方向互迁,统一会话列表
- 迁移前自动检测目标工具运行状态并备份数据

## 开发

```sh
cargo test                    # 运行全部测试
cargo run -p hub-app          # 开发模式启动
cargo clippy --all-targets    # 静态检查
```

架构:`crates/hub-core`(核心库:读取 → 统一中间表示 → 写入)+ `crates/hub-app`(界面)。

## 许可

私有项目,保留所有权利。
