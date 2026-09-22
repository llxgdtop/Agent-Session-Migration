//! 临时诊断(不入库):复现 9 月 10 日成功路径——ZCode 退出状态下写入真实库。
use hub_core::{read_session, scan_sessions, write_zcode_session};
use std::path::Path;

fn main() {
    let home = std::env::var("HOME").unwrap();
    let claude_root = Path::new(&home).join(".claude/projects");
    let summaries = scan_sessions(&claude_root).expect("扫描失败");
    // 选一个消息较少的真实会话,避免写入过大
    let target = summaries
        .iter()
        .filter(|s| s.message_count >= 2 && s.message_count <= 20)
        .min_by_key(|s| s.message_count)
        .expect("没有合适会话");
    println!("源会话: {} ({} 条消息)", target.title, target.message_count);
    let ir = read_session(&target.source_path).expect("读取失败");

    let cli_db = Path::new(&home).join(".zcode/cli/db/db.sqlite");
    let tasks_db = Path::new(&home).join(".zcode/v2/tasks-index.sqlite");
    match write_zcode_session(&ir, &cli_db, &tasks_db) {
        Ok(out) => println!(
            "✅ 写入成功: session_id={}, 会在 ZCode 列表顶部(迁移时刻排序)",
            out.session_id
        ),
        Err(e) => println!("❌ 写入失败: {e}"),
    }
}
