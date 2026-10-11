//! 检查 C8：extern rust 边界校验（§11.8）。
//!
//! 错误码：
//! - RSC-E090：extern rust 块内禁止 DSL 关键字（出现在语句位置）。
//!
//! **简化策略**：仅扫描 Cell 内 `extern rust read { }` 块体（`body.text`）与
//! `extern rust { }` 块体；文本扫描"语句起始位置"是否出现 DSL 关键字。
//! 不做 Rust 语法检查（交 rustc）。

use rscel_parser::diag::{code, Diagnostic};
use rscel_parser::{CellDef, Stmt};

const DSL_START_KEYWORDS: &[&str] = &[
    "jump ", "die;", "die ", "exit go with", "receive ", "go with", "read with",
];

/// 检查 C8：extern rust 边界。
pub fn check_extern_rust(cell: &CellDef) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for stmt in &cell.body {
        if let Stmt::ExternRustBlock { body, span, .. } = stmt {
            check_block(cell, &body.text, *span, &mut diags);
        }
    }
    diags
}

fn check_block(cell: &CellDef, text: &str, span: rscel_parser::Span, diags: &mut Vec<Diagnostic>) {
    // 逐行扫描：每行 trim 后看是否以 DSL 关键字起始。
    for line in text.lines() {
        let trimmed = line.trim_start();
        for kw in DSL_START_KEYWORDS {
            if trimmed.starts_with(kw) {
                diags.push(
                    Diagnostic::error(
                        code::E090,
                        format!("extern rust 块内出现 DSL 关键字 `{kw}`"),
                        span,
                    )
                    .with_detail(format!(
                        "§11.8：块内禁止 DSL 关键字；行 = `{}`",
                        trimmed
                    ))
                    .with_cell(cell.name.clone()),
                );
                return;
            }
        }
    }
}