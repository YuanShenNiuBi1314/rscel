//! 检查 C7：over 闭包纯函数校验（§11.7）。
//!
//! 错误码：
//! - RSC-E080：over 闭包非纯函数（含 DSL 关键字 + 修改局部变量）。
//!
//! **保守策略**：
//! - 闭包文本里若出现 `jump / die / exit / receive / go with / read with` → E080；
//! - 闭包文本里若出现 `v_<id> = ` 形式赋值 → E080（修改 Cell 环境）。
//! - 闭包文本里若出现 Cell 局部变量（`v_<id>`）之外的别名（如 `let x = `）→ 仅警告（不阻断）——
//!   保守地不报。简化：只检测 v_ 前缀赋值。

use rscel_parser::diag::{code, Diagnostic};
use rscel_parser::{CellDef, JumpStmt, OverOp, OverSpec, Stmt};

const DSL_KEYWORDS: &[&str] = &[
    "jump", "die", "exit go with", "go with", "read with", "receive",
];

/// 检查 C7：over 闭包。
pub fn check_over_closures(cell: &CellDef) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for stmt in &cell.body {
        if let Stmt::Jump(j) = stmt {
            if let Some(over) = &j.over {
                check_over_spec(cell, j, over, &mut diags);
            }
        }
    }
    diags
}

fn check_over_spec(cell: &CellDef, j: &JumpStmt, over: &OverSpec, diags: &mut Vec<Diagnostic>) {
    for op in &over.ops {
        check_op(cell, j, op, diags);
    }
}

fn check_op(cell: &CellDef, j: &JumpStmt, op: &OverOp, diags: &mut Vec<Diagnostic>) {
    let text = match op {
        OverOp::Filter(r) | OverOp::Map(r) | OverOp::FlatMap(r) | OverOp::Take(r) | OverOp::Skip(r) => {
            &r.text
        }
    };
    // 1) DSL 关键字检测
    for kw in DSL_KEYWORDS {
        // 用"独立词"匹配：必须前后是非标识符字符
        if contains_word(text, kw) {
            diags.push(
                Diagnostic::error(
                    code::E080,
                    format!("over 闭包非纯函数（含 DSL 关键字 `{kw}`）"),
                    j.span,
                )
                .with_detail(format!("§11.7：闭包内禁止 DSL 关键字；闭包文本 = `{text}`"))
                    .with_cell(cell.name.clone()),
            );
            return;
        }
    }
    // 2) v_<id> = 赋值检测（保守：仅匹配 "v_<ascii> = "）
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("v_") {
            // 取标识符部分
            let id_end = rest
                .char_indices()
                .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '_')
                .last()
                .map(|(idx, _)| idx + 1)
                .unwrap_or(0);
            if id_end > 0 {
                let after = rest[id_end..].trim_start();
                if after.starts_with('=') && !after.starts_with("==") {
                    diags.push(
                        Diagnostic::error(
                            code::E080,
                            "over 闭包修改了 Cell 环境变量（`v_<id> = ...`）",
                            j.span,
                        )
                        .with_detail(format!(
                            "§11.7：闭包必须为纯函数；闭包文本 = `{text}`"
                        ))
                        .with_cell(cell.name.clone()),
                    );
                    return;
                }
            }
        }
    }
}

/// 在 text 中查找 word（独立词，前后是非标识符字符或文本边界）。
fn contains_word(text: &str, word: &str) -> bool {
    let bytes = text.as_bytes();
    let wb = word.as_bytes();
    let mut i = 0;
    while i + wb.len() <= bytes.len() {
        if &bytes[i..i + wb.len()] == wb {
            let before_ok = i == 0 || !is_ident_byte(bytes[i - 1]);
            let after_idx = i + wb.len();
            let after_ok = after_idx == bytes.len() || !is_ident_byte(bytes[after_idx]);
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}