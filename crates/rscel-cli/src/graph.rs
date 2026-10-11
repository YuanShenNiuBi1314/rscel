//! `rscel graph <file> [--mermaid]` —— §16.4 的调用链路报告。
//!
//! **直接复用 `rscel_check::topology`，不再走一遍 gen**：§16.4 要的是前端语义层
//! 的图（Cell / jump 形态 / 键名 / group / 并发上限 / 风险点），而 codegen 的产物
//! 是 Rust 文本 —— 从生成物反推图既慢又会丢信息（比如 `when` 条件在生成物里
//! 已经变成 `if` 表达式）。报告里唯一比 topology 多出来的东西（警告级风险点）
//! 在 [`api::report`] 里包了一层，没有改动 topology。
//!
//! 退出码（§4.2「1 前端错误（check/build/trace 均适用）」）：
//! parse 失败 → 只报诊断，退 1；有 error 级诊断 → **诊断进 stderr、报告仍进 stdout**，
//! 退 1。为什么报告照打：链路报告正是用来排查这类错误的，藏着不给人看等于
//! 让读者对着一个退出码猜图。

use crate::api;
use rscel_check::check_program;
use rscel_parser::{parse_program, Diagnostic, Severity};
use std::io::Write;
use std::path::Path;

/// 跑 `rscel graph`。返回退出码。
pub fn run_graph(file: &Path, mermaid: bool) -> i32 {
    let src = match std::fs::read_to_string(file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("RSC: cannot read {}: {e}", file.display());
            return 3;
        }
    };
    let parsed = parse_program(&src, 0);
    if parsed.has_errors() {
        // 残缺 AST 画出来的链报告会误导，所以这里只交语法诊断。
        print_diagnostics(file, &parsed.diagnostics);
        eprintln!("graph: parse 失败，未输出链路报告");
        return 1;
    }
    let outcome = check_program(&parsed.program);
    let errors = print_diagnostics(file, &outcome.diagnostics);
    // 报告写 stdout（§4.2：stdout 只放命令结果）。
    let report = api::report(&parsed.program, mermaid);
    let out = std::io::stdout();
    let mut lock = out.lock();
    // 写失败（管道被关闭，比如 `| head`）不该让命令变成「静默成功」：
    // 报错并退 3（IO 错误）。
    if let Err(e) = write!(lock, "{report}") {
        eprintln!("RSC: 写 stdout 失败: {e}");
        return 3;
    }
    let _ = lock.flush();
    if errors > 0 {
        eprintln!("graph: {} 个前端错误（报告已输出，退出码按 §4.1 取 1）", errors);
        return 1;
    }
    0
}

/// 把诊断按 §15.1 打到 stderr，返回 error 级条数（供调用方决定退出码）。
pub fn print_diagnostics(file: &Path, diags: &[Diagnostic]) -> usize {
    let name = file.display().to_string();
    let mut errors = 0usize;
    for d in diags {
        if d.severity == Severity::Error {
            errors += 1;
        }
        eprintln!("{}", d.render(&name));
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_diagnostics_counts_only_errors() {
        let span = rscel_parser::Span::new(0, 3, 1, 4);
        let diags = vec![
            Diagnostic::error(42, "错一条", span),
            Diagnostic::warning(7, "警一条", span),
            Diagnostic::error(43, "再错一条", span),
        ];
        // 这里只验返回值（文本走 stderr，进程内拿不到；由 cli_smoke 的
        // `run_check` 路径覆盖真实渲染）。
        assert_eq!(print_diagnostics(Path::new("t.rscel"), &diags), 2);
    }
}
