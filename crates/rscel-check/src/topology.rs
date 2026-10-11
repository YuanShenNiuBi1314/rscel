//! 拓扑模型：把 `Program` 压成一张可渲染的 Cell 有向图（§16.4 调用链路报告 +
//! 前端编辑器的拓扑调试数据源）。
//!
//! ## 为什么放在 rscel-check 而不是 rscel-cli
//!
//! §16.4 要求「check 输出调用链路报告（文本 / mermaid）」，所以图的构建属于前端
//! 语义层，不是 CLI 的展示层：`rscel check`、`rscel graph`、`rscel trace`、
//! `rscel edit` 的 HTTP API 都消费同一份模型。放在 CLI 会让 check 拿不到它，
//! 逼出第二套图构建代码 —— 那才是真正的重复。
//!
//! ## 与 `deps::jump_graph` 的分工
//!
//! [`crate::deps::jump_graph`] 只给「Cell → 目标集合」的**集合**语义（C3 依赖检查用）；
//! 本模块保留**每条边**的形态（plain/loop/recurse/and/sync）、传输模式（go with /
//! read with / force …）、键名、group、`concurrent_limit`、`over` 管道、`when` 条件与
//! 语句序号 —— 拓扑调试要看的就是这些细节。
//!
//! ## 刻意不放进模型的东西
//!
//! - **所有权状态（Active/Moved/Frozen）**：判定在 [`crate::ownership`] 的状态机里，
//!   而且它本身就产出 E060–E064 诊断。这里再实现一遍只会和真实规则漂移，
//!   所以编辑器要展示所有权时，直接消费诊断列表。
//! - **解析后的类型**：D2 的解析结果在 `rscel-gen`（`ty::TypeEnv`），而
//!   `rscel-check` 不允许依赖 `rscel-gen`（依赖方向是 gen → parser、check → parser）。
//!   因此 `Node.receive` 里只带**源码注解文本**；CLI 层可以同时拿到两边并补齐。

use std::collections::BTreeSet;

use rscel_parser::{CellDef, JumpKind, Program, Stmt, TransMode};

/// 一条边的 jump 形态（与 §6 的五种形态一一对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    Plain,
    Loop,
    Recurse,
    And,
    /// `jump sync <group>`：不是数据边，而是**等待边**，单独成边才能画出同步点。
    Sync,
}

impl EdgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Plain => "plain",
            EdgeKind::Loop => "loop",
            EdgeKind::Recurse => "recurse",
            EdgeKind::And => "and",
            EdgeKind::Sync => "sync",
        }
    }
}

/// 传输模式文本（§6.6）。
pub fn mode_text(m: Option<TransMode>) -> Option<&'static str> {
    match m {
        Some(TransMode::GoWith) => Some("go with"),
        Some(TransMode::ReadWith) => Some("read with"),
        Some(TransMode::ForceGoWith) => Some("force go with"),
        Some(TransMode::ForceReadWith) => Some("force read with"),
        None => None,
    }
}

/// 一个 receive 槽位（含包限定与注解文本）。
#[derive(Debug, Clone)]
pub struct Slot {
    /// 键名（`g1.W1.A` 里的 `A`）。
    pub key: String,
    /// 包前缀（`g1.W1`）；裸键为 `None`。
    pub pkg: Option<String>,
    /// `: <类型>` 注解文本；无注解为 `None`（类型由 D2 在 gen 侧解析）。
    pub ty: Option<String>,
    /// `as (...)` 里的别名；缺省 = 键名。
    pub alias: String,
}

/// 一个 exit 输出键。
#[derive(Debug, Clone)]
pub struct OutKey {
    pub key: String,
    /// 实参表达式文本（用于在图上显示「什么值流向这个键」）。
    pub expr: String,
}

/// 图节点 = 一个 Cell。
#[derive(Debug, Clone)]
pub struct Node {
    pub name: String,
    pub line: usize,
    pub col: usize,
    pub is_entry: bool,
    /// 入口可达（`rscel_check::reach` 的结论）。不可达节点在图上应被淡化。
    pub reachable: bool,
    pub receive: Vec<Slot>,
    pub outputs: Vec<OutKey>,
    /// 终结方式描述（`exit go with` / `die` / `jump` / 「非出口」）。
    pub terminal: String,
    /// 图上要标注的风险提示（目前只有「fire-and-forget」这一类**不是诊断**的形状问题；
    /// 真正的错误一律走诊断码，避免同一问题两处定义）。
    pub risks: Vec<String>,
    /// 本 Cell 里 body 语句条数（编辑器用来显示规模）。
    pub stmts: usize,
}

/// 图的一条边。
#[derive(Debug, Clone)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
    pub mode: Option<String>,
    /// 实参键名（顺序 = 书写顺序）。
    pub keys: Vec<String>,
    pub group: Option<String>,
    /// `concurrent_limit` 表达式文本。
    pub limit: Option<String>,
    /// `over` 集合 + 管道文本（`items |> filter(..) |> map(..)`）。
    pub over: Option<String>,
    /// `when` 条件文本。
    pub cond: Option<String>,
    /// `return(<key> -> <别名>)` 的映射描述文本。
    pub returns: Vec<String>,
    /// 源语句在 body 里的下标 + 行列（编辑器点边跳源码用）。
    pub stmt: usize,
    pub line: usize,
    pub col: usize,
    /// 是否落在 Rust 块（if/else/match）内部——影响图的"这条边是否无条件"。
    pub in_block: bool,
}

/// 一个并发组（`jump and` / `jump recurse` 带 group）。
#[derive(Debug, Clone)]
pub struct Group {
    pub name: String,
    pub parent: String,
    pub targets: Vec<String>,
    pub limit: Option<String>,
    pub over: Option<String>,
    /// 父侧多包 receive 声明的槽位上界（D5 静态枚举出的最大 `W<n>`）。
    pub slots: usize,
    /// 同 Cell 内是否有对应的 `jump sync`。
    pub synced: bool,
}

/// 完整拓扑。
#[derive(Debug, Clone, Default)]
pub struct Topology {
    pub entry: Option<String>,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub groups: Vec<Group>,
}

/// 便捷入口：自己算可达性再建图（调用方已经算过时用 [`build`]，省一次 DFS）。
pub fn topology_of(program: &Program) -> Topology {
    let (_, reachable) = crate::reach::check_reachability(program);
    build(program, &reachable.into_iter().collect::<BTreeSet<String>>())
}

/// 构建拓扑图。
///
/// `reachable` 由调用方传入（来自 [`crate::reach::check_reachability]），
/// 避免在这里重做一遍可达性分析。
pub fn build(program: &Program, reachable: &BTreeSet<String>) -> Topology {
    let entry = program
        .entry
        .clone()
        .or_else(|| program.cells().find(|c| c.name == "main").map(|c| c.name.clone()));
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut groups = Vec::new();

    for cell in program.cells() {
        nodes.push(node_of(cell, entry.as_deref(), reachable));
        collect_edges(cell, &mut edges);
        collect_groups(program, cell, &mut groups);
    }
    Topology { entry, nodes, edges, groups }
}

fn node_of(cell: &CellDef, entry: Option<&str>, reachable: &BTreeSet<String>) -> Node {
    let receive: Vec<Slot> = cell
        .receive
        .keys
        .iter()
        .enumerate()
        .map(|(i, k)| Slot {
            key: k.name.clone(),
            pkg: k.pkg.clone(),
            ty: k.ty.as_ref().map(|t| t.text.trim().to_string()),
            alias: cell
                .receive
                .aliases
                .get(i)
                .cloned()
                .unwrap_or_else(|| k.name.clone()),
        })
        .collect();
    let mut outputs: Vec<OutKey> = Vec::new();
    for stmt in &cell.body {
        if let Stmt::Exit { args, .. } = stmt {
            for a in args {
                let key = a.effective_key();
                if key.is_empty() {
                    continue;
                }
                if !outputs.iter().any(|o| o.key == key) {
                    outputs.push(OutKey { key, expr: a.expr.text.trim().to_string() });
                }
            }
        }
    }
    let terminal = match cell.body.last() {
        Some(Stmt::Exit { .. }) => "exit go with".to_string(),
        Some(Stmt::Die { .. }) => "die".to_string(),
        Some(Stmt::Jump(j)) => format!("{:?}", j.kind).to_lowercase(),
        _ => "（末尾非出口，fallthrough）".to_string(),
    };
    let mut risks = Vec::new();
    for stmt in &cell.body {
        if let Stmt::Jump(j) = stmt {
            // `jump and` 不命名 group = 结果没有人接收（fire-and-forget）。
            // 这不是错误（并发发通知就是合法用法），但拓扑图上必须标出来，
            // 否则读图的人会以为父 Cell 会等到子实例收敛。
            if j.kind == JumpKind::And && j.group.is_none() {
                risks.push("fire-and-forget：jump and 未命名 group，子实例结果不会被接收".to_string());
            }
            if j.kind == JumpKind::Recurse && j.group.is_none() && !j.ret_map.is_empty() {
                risks.push("recurse 带 return 但没有 group：按内联 await 生成".to_string());
            }
        }
    }
    Node {
        name: cell.name.clone(),
        line: cell.span.line as usize,
        col: cell.span.col as usize,
        is_entry: entry == Some(cell.name.as_str()),
        reachable: reachable.contains(&cell.name),
        receive,
        outputs,
        terminal,
        risks,
        stmts: cell.body.len(),
    }
}

/// 收集全部边（含 RustBlock 嵌套块内的语句，递归展开）。
///
/// `stmt` 记录语句在所属 body 里的下标，编辑器点边跳源码要用它；嵌套块里的边
/// 记的是**所在块语句**的下标 + `in_block = true`（块内下标对上层没有意义）。
fn collect_edges(cell: &CellDef, out: &mut Vec<Edge>) {
    fn push_jump_edges(cell: &CellDef, j: &rscel_parser::JumpStmt, stmt: usize, in_block: bool, out: &mut Vec<Edge>) {
        let kind = match j.kind {
            JumpKind::Plain => EdgeKind::Plain,
            JumpKind::Loop => EdgeKind::Loop,
            JumpKind::Recurse => EdgeKind::Recurse,
            JumpKind::And => EdgeKind::And,
        };
        let over = j.over.as_ref().map(|o| {
            let mut s = o.collection.text.trim().to_string();
            for op in &o.ops {
                use rscel_parser::OverOp::*;
                let (name, c) = match op {
                    Filter(c) => ("filter", c),
                    Map(c) => ("map", c),
                    FlatMap(c) => ("flat_map", c),
                    Take(c) => ("take", c),
                    Skip(c) => ("skip", c),
                };
                s.push_str(&format!(" |> {name}({})", c.text.trim()));
            }
            s
        });
        for t in &j.targets {
            out.push(Edge {
                from: cell.name.clone(),
                to: t.clone(),
                kind,
                mode: mode_text(j.mode).map(str::to_string),
                keys: j
                    .args
                    .iter()
                    .map(|a| a.effective_key())
                    .filter(|k| !k.is_empty())
                    .collect(),
                group: j.group.clone(),
                limit: j.concurrent_limit.as_ref().map(|l| l.text.trim().to_string()),
                over: over.clone(),
                cond: j.condition.as_ref().map(|c| c.text.trim().to_string()),
                returns: j
                    .ret_map
                    .iter()
                    .map(|(k, a)| format!("{} -> {a}", k.name))
                    .collect(),
                stmt,
                line: j.span.line as usize,
                col: j.span.col as usize,
                in_block,
            });
        }
    }
    fn walk(cell: &CellDef, body: &[Stmt], base: usize, in_block: bool, out: &mut Vec<Edge>) {
        for (i, stmt) in body.iter().enumerate() {
            match stmt {
                Stmt::Jump(j) => push_jump_edges(cell, j, base + i, in_block, out),
                Stmt::Sync { group, span } => out.push(Edge {
                    from: cell.name.clone(),
                    to: format!("⏚{group}"),
                    kind: EdgeKind::Sync,
                    mode: None,
                    keys: vec![group.clone()],
                    group: Some(group.clone()),
                    limit: None,
                    over: None,
                    cond: None,
                    returns: Vec::new(),
                    stmt: base + i,
                    line: span.line as usize,
                    col: span.col as usize,
                    in_block,
                }),
                Stmt::RustBlock { block } => {
                    // 块内语句同样是真实控制边（§3.5.2 的嵌套分派）。`FlatMap` 之类
                    // 只是文本差异，边形态照旧，只是标 `in_block`，让图上能区分
                    // "无条件走" 与 "分支里才走"。
                    let inner: Vec<Stmt> = block
                        .parts
                        .iter()
                        .filter_map(|p| match p {
                            rscel_parser::RawBlockPart::Stmt(s) => Some(s.clone()),
                            _ => None,
                        })
                        .collect();
                    walk(cell, &inner, base + i, true, out);
                }
                _ => {}
            }
        }
    }
    walk(cell, &cell.body, 0, false, out);
}

fn collect_groups(program: &Program, cell: &CellDef, out: &mut Vec<Group>) {
    for stmt in &cell.body {
        let j = match stmt {
            Stmt::Jump(j) if matches!(j.kind, JumpKind::And | JumpKind::Recurse) => j,
            _ => continue,
        };
        let Some(name) = &j.group else { continue };
        let slots = cell
            .receive
            .keys
            .iter()
            .filter_map(|k| {
                let pkg = k.pkg.as_deref()?;
                let mut it = pkg.split('.');
                if it.next()? != name.as_str() {
                    return None;
                }
                let n: usize = it.next()?.strip_prefix('W')?.parse().ok()?;
                if n == 0 { None } else { Some(n) }
            })
            .max()
            .unwrap_or(0);
        let synced = cell
            .body
            .iter()
            .any(|s| matches!(s, Stmt::Sync { group: sg, .. } if sg == name));
        if out.iter().any(|g| g.name == *name && g.parent == cell.name) {
            continue;
        }
        out.push(Group {
            name: name.clone(),
            parent: cell.name.clone(),
            targets: j.targets.clone(),
            limit: j.concurrent_limit.as_ref().map(|l| l.text.trim().to_string()),
            over: j.over.as_ref().map(|o| o.collection.text.trim().to_string()),
            slots,
            synced,
        });
    }
    let _ = program;
}

/// §16.4 mermaid 导出：`graph TD`，节点标 receive/exit，边标形态 + 键 + group。
pub fn to_mermaid(t: &Topology) -> String {
    fn q(s: &str) -> String {
        // mermaid 节点标签里 `(`、`[`、`"`、`|` 都是特殊字符，必须引号包裹
        format!("\"{}\"", s.replace('"', "'"))
    }
    let mut s = String::from("graph TD\n");
    for n in &t.nodes {
        let recv = if n.receive.is_empty() {
            "—".to_string()
        } else {
            n.receive
                .iter()
                .map(|r| match &r.ty {
                    Some(ty) => format!("{}: {ty}", r.key),
                    None => r.key.clone(),
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let outs = if n.outputs.is_empty() {
            "—".to_string()
        } else {
            n.outputs.iter().map(|o| o.key.clone()).collect::<Vec<_>>().join(", ")
        };
        let entry_mark = if n.is_entry { "⟵entry " } else { "" };
        let dead = if n.reachable { "" } else { "（不可达）" };
        s.push_str(&format!(
            "  {}[{}{}<br/>⇩ {}<br/>⇧ {}{}]\n",
            q(&n.name),
            entry_mark,
            n.name,
            recv,
            outs,
            dead
        ));
    }
    for e in &t.edges {
        let label = {
            let mut parts = vec![e.kind.as_str().to_string()];
            if let Some(m) = &e.mode {
                parts.push(m.clone());
            }
            if !e.keys.is_empty() {
                parts.push(e.keys.join("+"));
            }
            if let Some(g) = &e.group {
                parts.push(format!("group {g}"));
            }
            if let Some(l) = &e.limit {
                parts.push(format!("lim={l}"));
            }
            if let Some(c) = &e.cond {
                parts.push(format!("when {c}"));
            }
            if e.in_block {
                parts.push("块内".to_string());
            }
            parts.join(" · ")
        };
        let arrow = match e.kind {
            EdgeKind::Loop | EdgeKind::Recurse => "-.->",
            EdgeKind::Sync => "-.->",
            _ => "-->",
        };
        s.push_str(&format!("  {} {arrow}|{}| {}\n", q(&e.from), label.replace('|', "/"), q(&e.to)));
    }
    s
}

/// §16.4 文本报告：入口 → 子 Cell → exit 流向的缩进树 + group 清单。
pub fn to_text_report(t: &Topology) -> String {
    let mut s = String::new();
    s.push_str(&format!("入口: {}\n", t.entry.clone().unwrap_or_else(|| "<无>".to_string())));
    let by_parent: Vec<&Group> = t.groups.iter().collect();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let entry = t.entry.clone();
    if let Some(e) = entry {
        walk_report(t, &e, 0, &mut seen, &mut s);
    }
    let unreached: Vec<&Node> = t
        .nodes
        .iter()
        .filter(|n| !seen.contains(&n.name))
        .collect();
    if !unreached.is_empty() {
        s.push_str("\n未从入口抵达的 Cell:\n");
        for n in unreached {
            s.push_str(&format!("  {}（{} 条语句）\n", n.name, n.stmts));
        }
    }
    if !by_parent.is_empty() {
        s.push_str("\n并发组:\n");
        for g in &by_parent {
            s.push_str(&format!(
                "  {} ← {} 内 {} 派生 [目标 {}{}]",
                g.name,
                g.parent,
                if g.over.is_some() { "over" } else { "单实例" },
                g.targets.join(", "),
                if let Some(l) = &g.limit { format!(", limit={l}") } else { String::new() },
            ));
            s.push_str(&format!(
                ", 父侧槽位 {} 个, sync {}",
                g.slots,
                if g.synced { "有" } else { "无（结果不回收）" }
            ));
            s.push('\n');
        }
    }
    s
}

fn walk_report(t: &Topology, name: &str, depth: usize, seen: &mut BTreeSet<String>, out: &mut String) {
    let pad = "  ".repeat(depth);
    if !seen.insert(name.to_string()) {
        out.push_str(&format!("{pad}{name} ↻（回边，已展开过）\n"));
        return;
    }
    let Some(n) = t.nodes.iter().find(|n| n.name == name) else {
        out.push_str(&format!("{pad}{name} <未声明>\n"));
        return;
    };
    out.push_str(&format!(
        "{pad}{}{}  ⇩{} ⇧{}\n",
        n.name,
        if n.is_entry { " ⟵entry" } else { "" },
        n.receive.len(),
        n.outputs.len()
    ));
    for r in &n.risks {
        out.push_str(&format!("{pad}  ⚠ {r}\n"));
    }
    let mut edges: Vec<&Edge> = t.edges.iter().filter(|e| e.from == name && e.kind != EdgeKind::Sync).collect();
    // 同一目标可能有多条边（不同 when 分支），保持源顺序即可
    edges.sort_by_key(|e| e.stmt);
    for e in edges {
        let keys = if e.keys.is_empty() {
            String::new()
        } else {
            format!("({})", e.keys.join(", "))
        };
        let group = e
            .group
            .as_ref()
            .map(|g| format!(" group {g}"))
            .unwrap_or_default();
        let label = format!("{} {}{}{}", e.kind.as_str(), e.to, keys, group);
        out.push_str(&format!("{pad}  → {label}\n"));
        walk_report(t, &e.to, depth + 2, seen, out);
    }
}
