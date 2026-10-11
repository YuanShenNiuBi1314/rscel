//! 检查 C5：遮蔽（§11.5）。
//!
//! 错误码：
//! - RSC-E070：`let` 绑定名 ∈ 本 Cell receive 别名集。

use rscel_parser::ast::{is_plain_ident, RawBlockPart};
use rscel_parser::diag::{code, Diagnostic};
use rscel_parser::{CellDef, Stmt};

/// 检查 C5：遮蔽。
pub fn check_shadow(cell: &CellDef) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    let aliases = &cell.receive.aliases;
    if aliases.is_empty() {
        return diags;
    }
    for stmt in &cell.body {
        check_stmt(cell, stmt, aliases, &mut diags);
    }
    diags
}

fn check_stmt(
    cell: &CellDef,
    stmt: &Stmt,
    aliases: &[String],
    diags: &mut Vec<Diagnostic>,
) {
    match stmt {
        Stmt::RawRust(r) => scan_text(cell, &r.text, r.span, aliases, diags),
        Stmt::RustBlock { block } => {
            for p in &block.parts {
                match p {
                    RawBlockPart::Text(t) => scan_text(cell, &t.text, t.span, aliases, diags),
                    RawBlockPart::Stmt(s) => check_stmt(cell, s, aliases, diags),
                }
            }
        }
        _ => {}
    }
}

fn scan_text(
    cell: &CellDef,
    text: &str,
    span: rscel_parser::Span,
    aliases: &[String],
    diags: &mut Vec<Diagnostic>,
) {
    // 简化策略：在 Rust 透传区里寻找 `let <ident>` 模式，识别遮蔽。
    // 跳过字符串/字符/原始串/行注释/块注释（这里简化：按空格分词）。
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(rest) = text.get(i..) {
            // 行注释
            if rest.starts_with("//") {
                if let Some(end) = rest.find('\n') {
                    i += end + 1;
                } else {
                    break;
                }
                continue;
            }
            // 块注释
            if rest.starts_with("/*") {
                if let Some(end) = rest.find("*/") {
                    i += end + 2;
                } else {
                    break;
                }
                continue;
            }
            // 字符串
            if rest.starts_with('"') {
                let mut j = i + 1;
                while j < bytes.len() {
                    if bytes[j] == b'\\' {
                        j += 2;
                        continue;
                    }
                    if bytes[j] == b'"' {
                        j += 1;
                        break;
                    }
                    j += 1;
                }
                i = j;
                continue;
            }
            // 字符
            if rest.starts_with('\'') {
                let mut j = i + 1;
                while j < bytes.len() {
                    if bytes[j] == b'\\' {
                        j += 2;
                        continue;
                    }
                    if bytes[j] == b'\'' {
                        j += 1;
                        break;
                    }
                    j += 1;
                }
                i = j;
                continue;
            }
            // let
            if rest.starts_with("let ") || rest.starts_with("let\t") || rest.starts_with("let\n") || rest.starts_with("let\r") || rest.starts_with("let(") || rest.starts_with("let:")
                || rest == "let" {
                // 跳过 "let "
                let after_let = if rest == "let" {
                    // 单独 "let" — 跳过它
                    i += 3;
                    if i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'\n') {
                        i += 1;
                    }
                    continue;
                } else {
                    i += 3;
                    if i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'\n' || bytes[i] == b'(' || bytes[i] == b':') {
                        i += 1;
                    }
                    &text[i..]
                };
                // 跳过 mut
                let mut s = after_let.trim_start();
                if let Some(rest2) = s.strip_prefix("mut ") {
                    s = rest2.trim_start();
                }
                // 读取标识符
                let id_end = s
                    .char_indices()
                    .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '_')
                    .last()
                    .map(|(idx, _)| idx + 1)
                    .unwrap_or(0);
                if id_end == 0 {
                    continue;
                }
                let ident = &s[..id_end];
                if is_plain_ident(ident) && aliases.iter().any(|a| a == ident) {
                    diags.push(
                        Diagnostic::error(
                            code::E070,
                            format!("`let` 绑定名 `{ident}` 遮蔽 receive 别名"),
                            span,
                        )
                        .with_detail(format!(
                            "§11.5：本 Cell receive 别名为 {:?}；`let {ident}` 与之冲突",
                            aliases
                        ))
                        .with_cell(cell.name.clone()),
                    );
                }
                i += id_end;
                continue;
            }
            i += 1;
        } else {
            break;
        }
    }
}