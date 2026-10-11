//! `rscel trace <file> [--max-items N] [--arg v]...` —— §16.5 的干跑模拟。
//!
//! 模拟本体在 `rscel_check::trace`（§16.5 属于前端语义层，`rscel edit` 的
//! `/api/trace` 与这里必须共用同一份判定，否则同一份源码在页面上和终端里
//! 会给出不一样的链路）。本模块只做**读文件 + 排版 + 定退出码**。
//!
//! 退出码（§4.2「1 前端错误（check/build/trace 均适用）」）：
//! parse 失败 → 1；有 error 级契约诊断 → 诊断进 stderr、**步骤仍进 stdout**、退 1；
//! 文件读不了 → 3。理由与 [`crate::graph`] 相同：干跑就是拿来排障的，
//! 有错时把结果藏起来等于让读者对着退出码猜。

use crate::graph::print_diagnostics;
use rscel_check::check_program;
use rscel_check::trace::{trace_program, Trace, TraceOptions, DEFAULT_MAX_ITEMS, MAX_DEPTH, MAX_STEPS};
use rscel_parser::parse_program;
use std::io::Write;
use std::path::PathBuf;

/// 一次 `rscel trace` 的入参。
#[derive(Clone, Debug)]
pub struct TraceRequest {
    pub file: PathBuf,
    /// `--max-items`：over 实例数 / loop 轮数 / recurse 层数的模拟上限。
    pub max_items: usize,
    /// `--arg`：入口 Cell 的实参（`key=value` 或按 receive 顺序的位置值）。
    pub args: Vec<String>,
}

impl TraceRequest {
    pub fn new(file: PathBuf) -> Self {
        TraceRequest { file, max_items: DEFAULT_MAX_ITEMS, args: Vec::new() }
    }
}

/// 跑 `rscel trace`。返回退出码。
pub fn run_trace(req: &TraceRequest) -> i32 {
    let src = match std::fs::read_to_string(&req.file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("RSC: cannot read {}: {e}", req.file.display());
            return 3;
        }
    };
    let parsed = parse_program(&src, 0);
    if parsed.has_errors() {
        print_diagnostics(&req.file, &parsed.diagnostics);
        eprintln!("trace: parse 失败，未做模拟");
        return 1;
    }
    let outcome = check_program(&parsed.program);
    let errors = print_diagnostics(&req.file, &outcome.diagnostics);

    let opts = TraceOptions { max_items: req.max_items, entry_args: req.args.clone(), ..Default::default() };
    let trace = trace_program(&parsed.program, &opts);
    let text = render(&trace);
    let out = std::io::stdout();
    let mut lock = out.lock();
    if let Err(e) = write!(lock, "{text}") {
        eprintln!("RSC: 写 stdout 失败: {e}");
        return 3;
    }
    let _ = lock.flush();
    if errors > 0 {
        eprintln!("trace: {} 个前端错误（步骤已输出，退出码按 §4.1 取 1）", errors);
        return 1;
    }
    0
}

/// §16.5 第 3 条要求的形状：`cell → jump(target, keys) → …` + 末尾 notes。
///
/// 排版成**可粘贴**的定宽文本：step 行一条，packet / group / note 各缩进一行，
/// 这样贴进 issue 或终端 diff 都还读得动（一长行糊在一起就没法比对两条链路）。
pub fn render(t: &Trace) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "rscel trace 干跑（§16.5）：{} 步。不求值 Rust；when 只认 字面量/简单标识符 + == != < > <= >=。\n",
        t.steps.len()
    ));
    s.push_str(&format!("上限：--max-items（over/loop/recurse）· step {} · 深度 {MAX_DEPTH}\n", MAX_STEPS));
    s.push('\n');
    if t.steps.is_empty() {
        s.push_str("（没有可模拟的步骤 —— 检查入口是否可达）\n");
    }
    for step in &t.steps {
        // `  12  fact @16 [loop]  jump loop fact when ...`
        s.push_str(&format!("{:>4}.  {} @{}  [{}]  {}\n", step.seq, step.cell, step.line, step.kind.as_str(), step.text));
        if let Some(g) = &step.group {
            s.push_str(&format!("          group: {g}\n"));
        }
        if !step.packet.is_empty() {
            let fields: Vec<String> = step
                .packet
                .iter()
                .map(|f| if f.died { format!("{}={} (died)", f.key, f.value) } else { format!("{}={}", f.key, f.value) })
                .collect();
            s.push_str(&format!("          packet: {}\n", fields.join("  ")));
        }
        if let Some(n) = &step.note {
            s.push_str(&format!("          note: {n}\n"));
        }
    }
    if !t.notes.is_empty() {
        s.push_str("\n说明:\n");
        for n in &t.notes {
            s.push_str(&format!("  - {n}\n"));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use rscel_check::trace::Trace;

    #[test]
    fn render_shows_cell_jump_packet_group_and_notes() {
        let mut t = Trace::default();
        t.steps.push(rscel_check::trace::Step {
            seq: 1,
            cell: "start".to_string(),
            stmt: 2,
            line: 7,
            col: 1,
            kind: rscel_check::trace::StepKind::And,
            text: "jump and worker over items group g1".to_string(),
            packet: vec![
                rscel_check::trace::PacketField { key: "g1.W1.d".to_string(), value: "x * 2".to_string(), died: false },
                rscel_check::trace::PacketField { key: "g1.W2.d".to_string(), value: "(died) worker".to_string(), died: true },
            ],
            group: Some("g1".to_string()),
            note: Some("(unevaluated)".to_string()),
        });
        t.notes.push("已截断，实际 10".to_string());
        let text = render(&t);
        assert!(text.contains("1.  start @7  [and]"), "{text}");
        assert!(text.contains("group: g1"), "{text}");
        assert!(text.contains("g1.W1.d=x * 2"), "{text}");
        assert!(text.contains("g1.W2.d=(died) worker (died)"), "{text}");
        assert!(text.contains("note: (unevaluated)"), "{text}");
        assert!(text.contains("说明:\n  - 已截断，实际 10"), "{text}");
    }

    #[test]
    fn render_of_empty_trace_says_so_instead_of_printing_nothing() {
        let text = render(&Trace::default());
        assert!(text.contains("没有可模拟的步骤"), "{text}");
    }
}
