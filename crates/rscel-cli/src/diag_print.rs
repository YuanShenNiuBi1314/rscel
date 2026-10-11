//! 诊断格式化（§15.1）：`RSC-<E|W><NNN>: <消息>；<细节>；位置: cell <name> @ <行>:<列>`。
//!
//! 与 rscel-parser 的 `Diagnostic::render` 略有差异：本模块接受外部 cell 名（避免
//! 依赖诊断里的 cell 字段，CLI 拿到 check_outcome 的 diagnostics 时 cell 字段已填）。

use rscel_parser::Diagnostic;

pub fn format_diag(file_name: &str, cell_name: &str, d: &Diagnostic) -> String {
    let kind = match d.severity {
        rscel_parser::Severity::Error => "E",
        rscel_parser::Severity::Warning => "W",
    };
    let detail = d
        .detail
        .as_ref()
        .map(|s| format!("；{s}"))
        .unwrap_or_default();
    format!(
        "RSC-{kind}{code}: {msg}{detail}；位置: {file} @ {line}:{col}",
        kind = kind,
        code = d.code,
        msg = d.message,
        detail = detail,
        file = if cell_name.is_empty() { file_name.to_string() } else { format!("cell {cell_name}") },
        line = d.span.line,
        col = d.span.col,
    )
}

/// 仅供单元测试暴露：可注入 mock Diagnostic
pub fn format_diag_for_test(d: &Diagnostic) -> String {
    format_diag("test.rscel", d.cell.as_deref().unwrap_or(""), d)
}
