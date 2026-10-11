//! 检查 C2：收敛（CFG 可达性，§11.2）。
//!
//! 错误码：
//! - RSC-E010：分支可达但未收敛；
//! - RSC-E014：无限无出口 loop（只存在 loop 回边、无可达 exit/die/plain 出口）。
//!
//! **简化策略**（按 §1 文档裁定 §11.2 的简化版本）：
//! - `jump recurse/and` 视为 fallthrough（继续下一语句），不模拟 spawn；
//! - `jump loop` 视为条件回边到入口（loop kind 且带 when 时模拟条件分支）；
//! - 出口：plain jump / recurse / and（含 when 真） / loop 含 when 真 → 视为出口。
//!
//! 注意：本模块**不**导 `pub fn converge`（撞 std prelude）。导出 `check_convergence`。

use rscel_parser::diag::{code, Diagnostic};
use rscel_parser::{CellDef, JumpKind, Stmt};

/// 对单个 Cell 做收敛检查（§11.2）。
///
/// 返回诊断列表（错误）。
pub fn check_convergence(cell: &CellDef) -> Vec<Diagnostic> {
    let body = &cell.body;
    let mut diags = Vec::new();

    // 第零遍：**出口语句之后的死代码**（§5.3「所有可达路径收敛到恰好一个出口」）。
    //
    // `exit go with` / `die` / 无条件 `jump` 都会终止本实例；它们后面还挂着语句，
    // 意味着那段代码**永远不会执行**。修前 gen 照样把它生成出来，rustc 报
    // `unreachable_code`/`unreachable_statements`——用户在一个 `-D warnings` 的
    // 流水线里看到的是一条指向生成物的 Rust 错误，而不是一句「你这行写晚了」。
    diags.extend(check_dead_code_after_terminal(cell));

    // 第一遍：检测死循环（只有 jump loop，无 exit/die/plain）
    let has_exit = body.iter().any(|s| matches!(s, Stmt::Exit { .. } | Stmt::Die { .. }));
    let has_plain = body.iter().any(|s| {
        matches!(s, Stmt::Jump(j) if matches!(j.kind, JumpKind::Plain | JumpKind::Recurse | JumpKind::And) && j.condition.is_some())
    });
    let has_loop = body.iter().any(|s| matches!(s, Stmt::Jump(j) if j.kind == JumpKind::Loop));

    // E014：无限无出口 loop（无 when 的 jump loop 且无其它出口）
    if has_loop {
        let loop_with_when = body.iter().any(|s| {
            matches!(s, Stmt::Jump(j) if j.kind == JumpKind::Loop && j.condition.is_some())
        });
        if !loop_with_when && !has_exit && !has_plain {
            // 找出第一个 loop 的 span
            let first_loop_span = body
                .iter()
                .find_map(|s| match s {
                    Stmt::Jump(j) if j.kind == JumpKind::Loop => Some(j.span),
                    _ => None,
                })
                .unwrap_or_default();
            diags.push(
                Diagnostic::error(
                    code::E014,
                    "无法抵达出口的无限 jump loop 路径",
                    first_loop_span,
                )
                .with_detail("§11.2：jump loop 无 when 条件且 Cell 内无 exit/die/plain 出口".to_string())
                    .with_cell(cell.name.clone()),
            );
        }
    }

    // 第二遍：CFG 模拟（活跃路径）
    let initial = vec![Frame { idx: 0, has_loop_exit: false }];
    let mut stack: Vec<Frame> = initial;
    while let Some(frame) = stack.pop() {
        if frame.idx >= body.len() {
            // 走到 body 末尾但未出口 → E010
            let span = body.last().map(stmt_span).unwrap_or_default();
            diags.push(
                Diagnostic::error(
                    code::E010,
                    "分支可达但未收敛（走到 Cell body 末尾）",
                    span,
                )
                .with_detail(format!(
                    "§11.2：路径条件：无；位置 cell {}",
                    cell.name
                ))
                .with_cell(cell.name.clone()),
            );
            continue;
        }
        let stmt = &body[frame.idx];
        match stmt {
            Stmt::RawRust(_) | Stmt::Out { .. } | Stmt::ExternRustBlock { .. } | Stmt::Sync { .. } => {
                // 透传 / out / extern / sync：fallthrough
                stack.push(Frame { idx: frame.idx + 1, has_loop_exit: frame.has_loop_exit });
            }
            Stmt::Die { .. } | Stmt::Exit { .. } => {
                // 合法出口
            }
            Stmt::Jump(j) => match j.kind {
                JumpKind::Plain | JumpKind::Recurse | JumpKind::And => {
                    if j.condition.is_some() {
                        // when 真 → 出口；假 → fallthrough
                        stack.push(Frame { idx: frame.idx + 1, has_loop_exit: frame.has_loop_exit });
                        // 真分支不再压栈（视为已离开）
                    } else {
                        // 无 when → 直接出口（合法终点）
                    }
                }
                JumpKind::Loop => {
                    if j.condition.is_some() {
                        // when 假 → fallthrough；当 → 回边到入口（视为出口）
                        stack.push(Frame { idx: frame.idx + 1, has_loop_exit: true });
                    } else {
                        // 无 when 的 loop：回到入口；若是唯一路径，会被 E014 检测
                        // 不再压栈
                    }
                }
            },
            Stmt::RustBlock { block } => {
                // RustBlock 内若含 DSL 语句，按"先按文本继续，after 末尾回到正文下一句"近似。
                // 保守：若有 DSL，跳到下一条；否则继续。
                if block.has_dsl() {
                    // 复杂情形：递归检查子句的收敛（这里简化为 fallthrough）
                    stack.push(Frame { idx: frame.idx + 1, has_loop_exit: frame.has_loop_exit });
                } else {
                    stack.push(Frame { idx: frame.idx + 1, has_loop_exit: frame.has_loop_exit });
                }
            }
        }
    }

    diags


}

struct Frame {
    idx: usize,
    #[allow(dead_code)]
    has_loop_exit: bool,
}

fn stmt_span(s: &Stmt) -> rscel_parser::Span {
    match s {
        Stmt::RawRust(r) => r.span,
        Stmt::Jump(j) => j.span,
        Stmt::Exit { span, .. } => *span,
        Stmt::Die { span } => *span,
        Stmt::Out { span, .. } => *span,
        Stmt::ExternRustBlock { span, .. } => *span,
        Stmt::Sync { span, .. } => *span,
        Stmt::RustBlock { block } => block.span,
    }
}

/// 出口语句（终止本实例）。`jump loop` / `jump and` / `jump recurse` / 带 `when`
/// 的 plain jump 都**不是**出口：前两者继续往下跑，后者条件不成立时也要继续。
fn is_terminal(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Exit { .. } | Stmt::Die { .. } => true,
        Stmt::Jump(j) => j.kind == JumpKind::Plain && j.condition.is_none(),
        _ => false,
    }
}

/// §5.3：出口语句之后不允许还有语句。
fn check_dead_code_after_terminal(cell: &CellDef) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for (i, stmt) in cell.body.iter().enumerate() {
        if !is_terminal(stmt) {
            continue;
        }
        let Some(next) = cell.body.get(i + 1) else { continue };
        let span = match next {
            Stmt::Jump(j) => j.span,
            Stmt::Exit { span, .. } => *span,
            Stmt::Die { span } => *span,
            Stmt::Out { span, .. } => *span,
            Stmt::Sync { span, .. } => *span,
            Stmt::ExternRustBlock { span, .. } => *span,
            Stmt::RawRust(r) => r.span,
            Stmt::RustBlock { block } => block.span,
        };
        diags.push(
            Diagnostic::error(
                code::E010,
                format!(
                    "出口语句（第 {} 条）之后还有语句，永远不会执行",
                    i + 1
                ),
                span,
            )
            .with_detail(
                "§5.3：每个 Cell 的所有可达路径必须收敛到**恰好一个**出口                  （`exit go with` / `die` / 无条件 `jump`）。把后续逻辑移到出口之前，                 或用 `jump recurse`（非出口）替代 `jump`。"
                    .to_string(),
            )
            .with_cell(cell.name.clone()),
        );
        return diags; // 只报第一处，避免同一死代码块刷屏
    }
    diags
}
