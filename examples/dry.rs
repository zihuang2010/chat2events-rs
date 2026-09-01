//! `--dry`：按**真实分段**逐段渲染 prompt，**一个 token 都不花**。
//!
//! ```sh
//! cargo run --example dry -- <raw_root> <corp> <room> <since> <until> <segment_msgs>
//! ```
//!
//! ⚠️ `segment_msgs` **必填，没有默认值** —— 跟 `config.toml` 同一条规矩。
//! 这里曾经兜底成 400：对拍工具悄悄用一个和配置无关的数跑，改了 config 也不反映过来，
//! 而分段边界正是这个工具要验的东西。
//!
//! 走的是和生产同一条 `view` —— 便签为空（它只有跑过模型才有内容，第一段本来就是空的）。
//!
//! 用途是**跟 Python 版逐字节对拍**（`../pychat2events` 的 `src.extract.preview`）：
//! 那边的 `_body` / `_labels` / `render` / `_segments` / `_cut` 被真实样本校准过，
//! 搬运是否等价，diff 一次就知道。实测 823 条样本双方 **59664 字节逐字节相同**、
//! 分段边界同为 `(0,277) (277,535) (535,823)`。
use chat2events_rs::{extract, ingest, window::Window};

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .without_time()
        .with_target(false)
        .init();
    let a: Vec<String> = std::env::args().skip(1).collect();
    let [root, corp, room, since, until, cap] = a
        .get(..6)
        .and_then(|s| <[String; 6]>::try_from(s.to_vec()).ok())
        .expect("用法: dry <raw_root> <corp> <room> <since> <until> <segment_msgs>");
    // 无默认值，缺失即报错 —— 与 config.toml 的 segment_msgs 同一条规矩
    let cap: usize = cap.parse().expect("segment_msgs 要是个数");

    let w = Window::span(since.parse().unwrap(), until.parse().unwrap());
    let conv = ingest::read_room(std::path::Path::new(&root), &corp, &room, &w).unwrap();
    eprintln!("msgs = {}", conv.msgs.len());
    print!("{}", extract::preview(&conv.msgs, cap));
}
