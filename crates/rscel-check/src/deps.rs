//! 检查 C3：`concurrent_limit` 下的子实例依赖（§7.3 / D3 依赖检查）。
//!
//! §7.3 规定：**带 `concurrent_limit` 的 `jump and`**，其子实例必须完全独立：
//!
//! 1. 子 Cell 之间不能相互 jump / recurse（含传递可达）；
//! 2. 子 Cell 的返回包不能被彼此读取（同 1，表现为一条 jump 边）；
//! 3. **例外**：父通过 `read with` 把同一个只读 Arc 下发给全部子实例——不算子间依赖。
//!
//! 违反 → RSC-E050。
//!
//! 不带 `concurrent_limit` 的 `jump and` 不启用本检查（§7.3 明文"仅当带 concurrent_limit"）。
//!
//! 本模块同时填充 [`AnalysisResult::depends`]（Cell → 其 jump 目标集），供拓扑报告、
//! trace 与内联 pass 复用。

use std::collections::{BTreeSet, HashMap, HashSet};

use rscel_parser::diag::code;
use rscel_parser::{CellDef, Diagnostic, JumpKind, Program, Stmt};

/// Cell → 直接 jump 目标集合（Plain / Loop 自环 / Recurse / And 全部计入）。
pub fn jump_graph(program: &Program) -> HashMap<String, BTreeSet<String>> {
    let mut g: HashMap<String, BTreeSet<String>> = HashMap::new();
    for cell in program.cells() {
        let entry = g.entry(cell.name.clone()).or_default();
        collect_jump_targets_of_stmts(&cell.body, entry);
    }
    g
}

fn collect_jump_targets_of_stmts(body: &[Stmt], out: &mut BTreeSet<String>) {
    for stmt in body {
        match stmt {
            Stmt::Jump(j) => {
                for t in &j.targets {
                    out.insert(t.clone());
                }
            }
            Stmt::RustBlock { block } => {
                for part in &block.parts {
                    if let rscel_parser::RawBlockPart::Stmt(Stmt::Jump(j)) = part {
                        // 嵌套块里的 jump 同样是真实控制边
                        for t in &j.targets {
                            out.insert(t.clone());
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// 从 `from` 出发、沿 jump 边能到达的全部 Cell（不含自身，除非成环）。
pub fn reachable_from(from: &str, graph: &HashMap<String, BTreeSet<String>>) -> HashSet<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = vec![from.to_string()];
    while let Some(cur) = stack.pop() {
        if let Some(next) = graph.get(&cur) {
            for n in next {
                if seen.insert(n.clone()) {
                    stack.push(n.clone());
                }
            }
        }
    }
    seen
}

/// C3 检查 + 依赖图。返回（诊断，Cell → 依赖集）。
///
/// 依赖集与 `depends` 字段同形：每个 Cell 的**直接** jump 目标（键为 Cell 名）。
pub fn check_concurrent_deps(program: &Program) -> (Vec<Diagnostic>, HashMap<String, BTreeSet<String>>) {
    let mut diags = Vec::new();
    let graph = jump_graph(program);

    for cell in program.cells() {
        for stmt in &cell.body {
            let Stmt::Jump(j) = stmt else { continue };
            if j.kind != JumpKind::And {
                continue;
            }
            // §7.3：仅当带 concurrent_limit 才启用
            if j.concurrent_limit.is_none() {
                continue;
            }
            if j.targets.len() < 2 {
                // 单目标：兄弟实例跑的是同一个 Cell，彼此无从引用；只有多目标才可能
                // 出现"子 A jump 到子 B"这种真实耦合。
                continue;
            }
            for (i, a) in j.targets.iter().enumerate() {
                let reach_a = reachable_from(a, &graph);
                let group_name = j.group.clone().unwrap_or_else(|| "<无名>".to_string());
                for b in j.targets.iter().skip(i + 1).filter(|b| *b != a) {
                    let reach_b = reachable_from(b, &graph);
                    let a_to_b = reach_a.contains(b);
                    let b_to_a = reach_b.contains(a);
                    if !(a_to_b || b_to_a) {
                        continue;
                    }
                    let (from, to) = if a_to_b { (a, b) } else { (b, a) };
                    diags.push(
                        Diagnostic::error(
                            code::E050,
                            format!("group `{group_name}` 的子实例不独立：`{from}` 可跳转到 `{to}`"),
                            j.span,
                        )
                        .with_detail(format!(
                            "§7.3 / D3：带 `concurrent_limit` 的 `jump and` 要求子 Cell 互不依赖（含传递可达）。\
                             实测依赖边：{from} -> {to}。请拆分 Cell、去掉 concurrent_limit，\
                             或改为父侧 `read with` 下发只读数据（§7.3 允许的例外）"
                        ))
                        .with_cell(cell.name.clone()),
                    );
                }
            }
        }
    }

    (diags, graph)
}

/// `cell` 的直接依赖集（用于 `AnalysisResult::depends` 的按名查询）。
pub fn direct_depends(cell: &CellDef, graph: &HashMap<String, BTreeSet<String>>) -> BTreeSet<String> {
    graph.get(&cell.name).cloned().unwrap_or_default()
}
