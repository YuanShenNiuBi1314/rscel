//! 检查 C6：入口可达性（§11.6）。
//!
//! 错误码：
//! - RSC-E020：无入口（既无 `entry` 也无 `cell main`）。
//! - RSC-W001：不可达 Cell 告警（不阻断）。

use std::collections::{BTreeSet, HashSet};

use rscel_parser::diag::{code, Diagnostic};
use rscel_parser::{JumpKind, Program, Stmt};

/// 检查 C6：可达性 + E020 无入口。
///
/// 返回诊断列表（错误 + 警告）。
pub fn check_reachability(program: &Program) -> (Vec<Diagnostic>, BTreeSet<String>) {
    let mut diags = Vec::new();
    let cell_names: Vec<&str> = program.cells().map(|c| c.name.as_str()).collect();
    let cell_set: HashSet<&str> = cell_names.iter().copied().collect();

    // 入口确定
    let entry = program
        .entry
        .clone()
        .or_else(|| {
            if cell_set.contains("main") {
                Some("main".to_string())
            } else {
                None
            }
        });
    let entry = match entry {
        Some(e) => e,
        None => {
            diags.push(
                Diagnostic::error(
                    code::E020,
                    "缺少程序入口（无 `entry` 声明且无 `cell main`）",
                    rscel_parser::Span::default(),
                )
                .with_detail("§2.1 规则 3：必须有入口 Cell".to_string()),
            );
            return (diags, BTreeSet::new());
        }
    };
    if !cell_set.contains(entry.as_str()) {
        diags.push(
            Diagnostic::error(
                code::E020,
                format!("入口 Cell `{entry}` 不存在"),
                rscel_parser::Span::default(),
            )
            .with_detail("§2.1 规则 3".to_string()),
        );
        return (diags, BTreeSet::new());
    }

    // 闭包：从入口沿 jump 目标（含 plain/recurse/and/loop 目标）扩散。
    let mut reachable: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<String> = vec![entry.clone()];
    while let Some(cur) = stack.pop() {
        if reachable.contains(&cur) {
            continue;
        }
        reachable.insert(cur.clone());
        // 找 cell
        let cell = match program.cell(&cur) {
            Some(c) => c,
            None => continue,
        };
        for stmt in &cell.body {
            if let Stmt::Jump(j) = stmt {
                match j.kind {
                    JumpKind::Plain | JumpKind::Recurse | JumpKind::And => {
                        for t in &j.targets {
                            if !reachable.contains(t) {
                                stack.push(t.clone());
                            }
                        }
                    }
                    JumpKind::Loop => {
                        // loop 目标通常 == current（已在 E011 中校验），不扩散
                    }
                }
            }
        }
    }

    // W001：不可达 Cell（无任何入边）
    for cell_name in &cell_names {
        if reachable.contains(*cell_name) {
            continue;
        }
        // 是否有任何其它 cell jump 到这里？
        let has_inbound = program.cells().any(|c| {
            c.body.iter().any(|s| match s {
                Stmt::Jump(j) => j.targets.iter().any(|t| t == *cell_name),
                _ => false,
            })
        });
        if !has_inbound {
            // 入口 Cell 总是被 entry 引用，不会到这里
            let span = program
                .cell(cell_name)
                .map(|c| c.span)
                .unwrap_or_default();
            diags.push(
                Diagnostic::warning(
                    code::W001,
                    format!("Cell `{cell_name}` 不可达"),
                    span,
                )
                .with_detail("§11.6 / §2.2：未被任何 jump 目标引用".to_string()),
            );
        }
    }

    let _ = cell_set;
    (diags, reachable)
}