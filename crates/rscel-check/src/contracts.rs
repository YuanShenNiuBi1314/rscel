//! 检查 C1：契约表构建与校验（§11.1）。
//!
//! 错误码：
//! - RSC-E040：非标识符实参必须显式 `expr -> key`（§9.1）；
//! - RSC-E041：`jump loop` 实参个数 != receive 键数（§9.2）；
//! - RSC-E042：receive 引用了未传入的键（§11.1）；
//! - RSC-E043：多源裸键（必须 `包.变量`）（§11.1）；
//! - RSC-E044：包名/键名不在输出契约（§11.1）；
//! - RSC-E045：`over` 派生实例数与多包 receive 静态槽位不符（§11.1 + D5）；
//! - RSC-E011：`jump loop` 目标必须为当前 Cell（§11.1）；
//! - RSC-E035：`jump sync <group>` 未在本 Cell 创建 / 已 sync 过（§11.1）；
//! - RSC-E036：`jump recurse` 的 return 别名未声明为 `let mut 别名: Option<T>`（§6.3）；
//! - RSC-E037：`read with` 边的接收端注解不是 `Arc<T>`（§8.1）；
//! - RSC-E039：`concurrent_limit` 字面量无法转 usize（§7.1）；
//! - RSC-E046：出口式 `jump` 上挂了 `return` / `group` / `over` / `concurrent_limit`（§6.1）；
//! - RSC-E047：同一个 recurse 返回值既用 `return(...)` 又用多包 receive 读取（§6.3）；
//! - RSC-W002：`over` 元素数 > 多包 receive 槽位，多余结果按 D5 丢弃（§7.1）。

use std::collections::HashSet;

use rscel_parser::diag::{code, Diagnostic};
use rscel_parser::{
    Arg, CellDef, Ident, JumpKind, JumpStmt, OverOp, PacketKey, Program, Stmt, TransMode,
};

use crate::analysis::{codes as ccode, ContractTable, EdgeKeys};

/// 构建契约表（§11.1）。同时返回诊断列表——若存在错误则 check 终止。
pub fn build_contract_table(program: &Program) -> ContractTable {
    let mut table = ContractTable::default();
    for cell in program.cells() {
        let inputs: Vec<Ident> = cell
            .receive
            .keys
            .iter()
            .map(|k| strip_pkg(&k.name))
            .collect();
        table.cell_inputs.insert(cell.name.clone(), inputs);
    }
    table
}

/// 检查 C1 契约（§11.1）。
///
/// 返回诊断列表（错误 + 警告）。
pub fn check_contracts(program: &Program, table: &ContractTable) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    let cell_names: HashSet<&str> = program.cells().map(|c| c.name.as_str()).collect();

    for cell in program.cells() {
        for (idx, stmt) in cell.body.iter().enumerate() {
            if let Stmt::Jump(j) = stmt {
                check_jump_stmt(program, cell, j, idx, table, &cell_names, &mut diags);
            } else if let Stmt::Sync { group, span } = stmt {
                check_sync_stmt(cell, group, *span, idx, &mut diags);
            }
        }
    }
    // 把 jump and 创建的 group 信息写入 table 的副本（不修改原 table）
    // —— 由 caller 处理（lib.rs 在 build_contract_table 后再 fill group_members）。
    diags
}

/// 把 jump and / jump recurse 的 group 子实例集合回填到 `table`。
///
/// 调用方在 `check_contracts` 之后调用，把创建 group 的 jump 信息写入。
///
/// §6.3 明确 `jump recurse ... group <g>` 同样创建 group（附录 A.7 就是这么用的），
/// 所以两类都要登记；只登记 `and` 会让 recurse 组的多包 receive 校验整体失效。
pub fn fill_group_members(program: &Program, table: &mut ContractTable) {
    for cell in program.cells() {
        for stmt in &cell.body {
            if let Stmt::Jump(j) = stmt {
                if matches!(j.kind, JumpKind::And | JumpKind::Recurse) {
                    if let Some(g) = &j.group {
                        let entry = table.group_members.entry(g.clone()).or_default();
                        for t in &j.targets {
                            if !entry.contains(t) {
                                entry.push(t.clone());
                            }
                        }
                    }
                }
            }
        }
    }
}

/// 把每条 jump 边的传入键集回填到 `table.edge_keys`（供 gen / 拓扑报告复用）。
///
/// 之前的实现是在 `check_jump_stmt` 里 `table.edge_keys.clone()` → `insert` → 丢弃，
/// 既白拷贝整张表又什么都没留下；改为由本函数一次性填充。
pub fn fill_edge_keys(program: &Program, table: &mut ContractTable) {
    for cell in program.cells() {
        for (idx, stmt) in cell.body.iter().enumerate() {
            if let Stmt::Jump(j) = stmt {
                let edge = EdgeKeys {
                    keys: j
                        .args
                        .iter()
                        .map(|a| a.effective_key())
                        .filter(|k| !k.is_empty())
                        .collect(),
                    kind_is_loop: j.kind == JumpKind::Loop,
                };
                table.edge_keys.insert((cell.name.clone(), idx), edge);
            }
        }
    }
}

// ---------------------------------------------------------------- jump

fn check_jump_stmt(
    program: &Program,
    cell: &CellDef,
    j: &JumpStmt,
    stmt_idx: usize,
    table: &ContractTable,
    cell_names: &HashSet<&str>,
    diags: &mut Vec<Diagnostic>,
) {
    // E011：jump loop 目标必须为当前 Cell
    if j.kind == JumpKind::Loop
        && (j.targets.len() != 1 || j.targets[0] != cell.name)
    {
        diags.push(
            Diagnostic::error(
                code::E011,
                "jump loop 目标必须为当前 Cell",
                j.span,
            )
            .with_detail(format!(
                "§9.2 / §11.1：jump loop 原地循环，目标应为 `{}`，实际 `{}`",
                cell.name,
                j.targets.join(", ")
            ))
            .with_cell(cell.name.clone()),
        );
    }

    // E041：jump loop 实参个数 == receive 键数
    if j.kind == JumpKind::Loop {
        let n_recv = cell.receive.keys.len();
        if j.args.len() != n_recv {
            diags.push(
                Diagnostic::error(
                    code::E041,
                    "jump loop 实参个数与 receive 键数不一致",
                    j.span,
                )
                .with_detail(format!(
                    "§9.2 / §11.1：receive 键数 = {n_recv}，jump loop 实参数 = {}",
                    j.args.len()
                ))
                .with_cell(cell.name.clone()),
            );
        }
    }

    // E040：非标识符实参必须显式 `expr -> key`
    if j.kind != JumpKind::Loop {
        for arg in &j.args {
            if arg.needs_explicit_key() {
                diags.push(
                    Diagnostic::error(
                        code::E040,
                        "非标识符实参必须显式指定键名 `expr -> key`",
                        arg.span,
                    )
                    .with_detail(format!(
                        "§9.1 规则 1：表达式 `{}` 不是简单标识符，必须写 `-> <key>`",
                        arg.expr.text
                    ))
                    .with_cell(cell.name.clone()),
                );
            }
        }
    }

    // 处理 plain / recurse / and 三类的键名校验
    match j.kind {
        JumpKind::Plain | JumpKind::Recurse => {
            for target in &j.targets {
                if !cell_names.contains(target.as_str()) {
                    diags.push(
                        Diagnostic::error(
                            code::E042,
                            format!("jump 目标 Cell 不存在：`{target}`"),
                            j.span,
                        )
                        .with_detail("§11.1：jump 目标必须是已声明的 Cell".to_string())
                        .with_cell(cell.name.clone()),
                    );
                    continue;
                }
                check_single_target(cell, j, target, table, diags);
            }
        }
        JumpKind::And => {
            for target in &j.targets {
                if !cell_names.contains(target.as_str()) {
                    diags.push(
                        Diagnostic::error(
                            code::E042,
                            format!("jump and 目标 Cell 不存在：`{target}`"),
                            j.span,
                        )
                        .with_detail("§11.1：jump 目标必须是已声明的 Cell".to_string())
                        .with_cell(cell.name.clone()),
                    );
                }
            }
            // 多包场景下父侧 receive 检查需要等 sync 之后——由 check_multisrc_receive 处理
            check_and_arity(program, cell, j, diags);
        }
        JumpKind::Loop => {
            // loop 按位置刷新（已在 E041 处理）；不查键名
        }
    }

    // 记录 edge_keys：由 `fill_edge_keys` 在 check_contracts 之后统一回填到 table
    // （之前的实现在这里 clone 整张 edge_keys、插入后再丢弃，白拷贝且什么都没留下）。
    let _ = stmt_idx;

    // ---- 出口式 jump 的段落合法性（RSC-E046）----
    // `jump T`（plain）是**转移式出口**：本实例终止，目标自己收敛。因此
    // `return(...)` / `group` / `over` / `concurrent_limit` 挂在其上没有任何语义，
    // 而 gen 会**静默丢弃**它们（历史行为）——那是"以成功姿态交付错误代码"。
    // 现在直接报错，让用户的 `return(...)` 拼写错误在契约期暴露。
    if j.kind == JumpKind::Plain {
        let mut bad: Vec<&str> = Vec::new();
        if !j.ret_map.is_empty() { bad.push("return(...)"); }
        if j.group.is_some() { bad.push("group"); }
        if j.over.is_some() { bad.push("over"); }
        if j.concurrent_limit.is_some() { bad.push("concurrent_limit"); }
        if !bad.is_empty() {
            diags.push(
                Diagnostic::error(
                    ccode::E046,
                    format!("plain `jump` 是出口语句，不能带 {}", bad.join(" / ")),
                    j.span,
                )
                .with_detail(
                    "§6.1/§6.6：plain jump 之后本实例终止，目标 Cell 的返回包无人接收；\
                     需要拿返回值请改写 `jump recurse <T> ... return(<key> -> <别名>) group <g>;` \
                     并在随后 `jump sync <g>;`"
                        .to_string(),
                )
                .with_cell(cell.name.clone()),
            );
        }
    }

    // ---- jump recurse 的 return 契约（RSC-E044 / RSC-E036）----
    if j.kind == JumpKind::Recurse {
        check_recurse_return(cell, j, table, diags);
    }

    // ---- read with 的接收端类型（RSC-E037）----
    check_read_with_receiver(program, cell, j, diags);

    // ---- over 派生数 vs 多包 receive 静态槽位（D5 → RSC-E045 / W002）----
    if j.over.is_some() {
        check_over_slots(program, cell, j, diags);
    }

    // ---- concurrent_limit 字面量必须是 usize（RSC-E039）----
    if let Some(lim) = &j.concurrent_limit {
        let t = lim.text.trim();
        if is_numeric_literal(t) && t.parse::<usize>().is_err() {
            diags.push(
                Diagnostic::error(
                    ccode::E039,
                    format!("`concurrent_limit = {t}` 不是合法的实例数"),
                    lim.span,
                )
                .with_detail("§7.1：concurrent_limit 必须是可转成 usize 的非负整数表达式".to_string())
                .with_cell(cell.name.clone()),
            );
        }
    }
}

/// 数字字面量判定（`12`、`1_000`、`8usize`，以及**非法的** `-4` —— 负号也要认出来，
/// 否则 `concurrent_limit = -4` 会被当成"运行期表达式"而放过 E039）。
/// 含标识符的一律返回 false（运行期值，不校验）。
fn is_numeric_literal(t: &str) -> bool {
    let s = t.trim_end_matches("usize").trim_end_matches("u32").trim();
    let digits = s.strip_prefix('-').unwrap_or(s);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit() || c == '_')
}

/// `jump recurse ... return(<key> -> <别名>)` 的契约校验。
///
/// - RSC-E044：`key` 必须在目标 Cell 的输出契约里；
/// - RSC-E036：`别名` 必须是本 Cell 里已经声明的 `let mut 别名: Option<T> = ...;`
///   —— 子实例可能 `die`（返回 `None`），所以别名只能是 `Option<T>`；且 gen 生成的是
///   **赋值**（不是声明），没有 `mut` 会撞 rustc E0594。
///
/// gen 侧据此生成 `别名 = match __rsc_sub { Some(o) => Some(o.key), None => None }`，
/// 类型完全由用户注解决定，编译器不猜（D2 精神）。
fn check_recurse_return(cell: &CellDef, j: &JumpStmt, table: &ContractTable, diags: &mut Vec<Diagnostic>) {
    if j.ret_map.is_empty() {
        return;
    }
    // RSC-E047：`return(k -> 别名)` 与 `receive(g.W1.k)` 是**同一个返回包的两个消费者**。
    // 子实例的 `Ret_T` 只能被移动一次（Rust 的所有权），gen 无论怎么排都会出现
    // "第二个读者拿到默认值"这种静默错误数据 —— 所以两种写法只能选一种。
    if let Some(g) = &j.group {
        let slots = group_slot_count(cell, g);
        if slots > 0 {
            diags.push(
                Diagnostic::error(
                    ccode::E047,
                    format!("group `{g}` 的子实例返回值同时被 `return(...)` 和多包 receive 读取"),
                    j.span,
                )
                .with_detail(format!(
                    "§6.3 / §9.4：返回包只能被消费一次。请二选一——\
                     要么只用 `return({} -> 别名)`，要么只用 `receive({g}.W1..W{slots}.<key>)`",
                    j.ret_map.iter().map(|(k, _)| k.name.clone()).collect::<Vec<_>>().join(", ")
                ))
                .with_cell(cell.name.clone()),
            );
        }
    }
    let Some(target) = j.targets.first() else { return };
    let outs: Vec<String> = table.cell_outputs.get(target).cloned().unwrap_or_default();
    for (key, alias) in &j.ret_map {
        let key_name = key.name.clone();
        if !outs.iter().any(|o| o == &key_name) {
            diags.push(
                Diagnostic::error(
                    code::E044,
                    format!("return 键 `{key_name}` 不在子 Cell `{target}` 的输出契约中"),
                    j.span,
                )
                .with_detail(format!(
                    "§6.3：`{target}` 的输出键集 = {outs:?}；`exit go with(<expr> -> {key_name})` 才能被 return 映射取到"
                ))
                .with_cell(cell.name.clone()),
            );
        }
        if find_option_let(cell, alias).is_none() {
            diags.push(
                Diagnostic::error(
                    ccode::E036,
                    format!("return 别名 `{alias}` 必须先在 Cell 内声明为 `let mut {alias}: Option<T> = ...;`"),
                    j.span,
                )
                .with_detail(format!(
                    "§6.3：子实例可能 die（无返回包），故别名类型必须是 Option<T>；\
                     gen 生成的是对 `{alias}` 的赋值，声明必须带 mut。请在 jump recurse 之前补一行 \
                     `let mut {alias}: Option<T> = None;`"
                ))
                .with_cell(cell.name.clone()),
            );
        }
    }
}

/// 在本 Cell 的透传语句里找 `let mut <name>: Option<...> = ...;`，返回其内部类型文本。
///
/// 只认**显式注解**的 Option 声明——没有注解时无法判定 `Option<T>`（D2：绝不猜）。
fn find_option_let(cell: &CellDef, name: &str) -> Option<String> {
    for stmt in &cell.body {
        let Stmt::RawRust(r) = stmt else { continue };
        for line in r.text.lines() {
            let t = line.trim();
            let Some(rest) = t.strip_prefix("let ") else { continue };
            let Some(rest) = rest.strip_prefix("mut ") else { continue };
            let ident: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if ident != name {
                continue;
            }
            let after = rest[ident.len()..].trim_start();
            let Some(ty) = after.strip_prefix(':') else { continue };
            let Some(inner) = ty.trim_start().strip_prefix("Option<") else { continue };
            // 取 `Option<` 之后到匹配的 `>` 为止的内层类型文本
            let Some(end) = find_matching_angle(inner, '<') else { continue };
            let inner_ty = inner[..end].trim();
            if inner_ty.is_empty() {
                continue;
            }
            return Some(inner_ty.to_string());
        }
    }
    None
}

/// 在 `s` 中找到与已消费的 `<` 匹配的 `>` 的下标（支持嵌套 `<`/`>`）。
fn find_matching_angle(s: &str, open: char) -> Option<usize> {
    let close = match open { '<' => '>', _ => '>' };
    let mut depth = 1i32;
    for (i, c) in s.char_indices() {
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// `read with` 边的接收端类型检查（RSC-E037）。
///
/// §8.1：`read with(x)` 生成 `Arc::clone(&x)`，因此接收端 `receive(k: T)` 的**注解**
/// 必须写成 `Arc<T>`。历史上 gen 完全忽略 mode，把原值直接按位置传 → 接收端注解
/// `Arc<String>` 时撞 rustc 的 "expected Arc, found String"，报错信息指不到 DSL。
/// 现在在契约期就报出来。
///
/// 只在接收键**有显式注解**时判定；无注解时类型由 gen 的 D2 传播决定，这里不猜。
fn check_read_with_receiver(program: &Program, cell: &CellDef, j: &JumpStmt, diags: &mut Vec<Diagnostic>) {
    if !matches!(j.mode, Some(TransMode::ReadWith) | Some(TransMode::ForceReadWith)) {
        return;
    }
    for target in &j.targets {
        let Some(tc) = program.cell(target) else { continue };
        for arg in &j.args {
            let key = arg.effective_key();
            if key.is_empty() { continue }
            let Some(pk) = tc.receive.keys.iter().find(|k| k.name == key) else { continue };
            let Some(ty) = &pk.ty else { continue };
            let ty_text = ty.text.trim();
            if !starts_with_arc(ty_text) {
                diags.push(
                    Diagnostic::error(
                        ccode::E037,
                        format!("`read with({key})` 要求接收端 `{target}` 的键 `{key}` 注解为 Arc<T>"),
                        arg.span,
                    )
                    .with_detail(format!(
                        "§8.1：read with 生成 `Arc::clone(..)`，接收端应写 `receive({key}: Arc<..>)`，\
                         当前是 `{ty_text}`；只读共享请保持 Arc，独占所有权请改 `go with`"
                    ))
                    .with_cell(cell.name.clone()),
                );
            }
        }
    }
}

/// 类型文本是否以 `Arc <` 开头（允许任意空白、`std::sync::Arc`、以及 `&Arc<` 之外的形式）。
fn starts_with_arc(ty: &str) -> bool {
    let t: String = ty.chars().filter(|c| !c.is_whitespace()).collect();
    t.starts_with("Arc<") || t.ends_with("::Arc<") || t.contains("::Arc<")
}

/// D5：`over` 派生数与多包 receive 静态槽位的匹配检查。
///
/// - 槽位 > 静态元素数 → RSC-E045（多出来的 `g.Wk.key` 永远取不到值；gen 会用
///   `Default::default()` 兜底，那是静默的错误数据）；
/// - 静态元素数 > 槽位 → RSC-W002（"取前 N 并告警"，D5 裁定的告警分支）；
/// - 集合大小运行期未知 → 不报（否则每个真实并行程序都自带警告）。
fn check_over_slots(program: &Program, cell: &CellDef, j: &JumpStmt, diags: &mut Vec<Diagnostic>) {
    let Some(group) = &j.group else { return };
    let Some(over) = &j.over else { return };
    let slots = group_slot_count(cell, group);
    if slots == 0 {
        return; // 父侧不读多包 → 无槽位可比
    }
    let Some(n) = static_collection_len(program, cell, over.collection.text.trim()) else { return };
    if slots > n {
        diags.push(
            Diagnostic::error(
                ccode::E045,
                format!("group `{group}` 的多包 receive 声明了 {slots} 个槽位，但 over 集合只有 {n} 个元素"),
                over.collection.span,
            )
            .with_detail(
                "§7.1 / D5：槽位多于元素时，超出的 `g.Wk.key` 取不到真实值。\
                 请把 receive 的 W 槽位减到元素个数，或让 over 集合至少提供 slots 个元素"
                    .to_string(),
            )
            .with_cell(cell.name.clone()),
        );
    } else if n > slots {
        diags.push(
            Diagnostic::warning(
                ccode::W002,
                format!("over 集合有 {n} 个元素，group `{group}` 只接收前 {slots} 个包"),
                over.collection.span,
            )
            .with_detail("§7.1 / D5：多出来的实例结果按 D4 增量回收直接丢弃".to_string())
                .with_cell(cell.name.clone()),
        );
    }
}

/// `group` 在本 Cell 多包 receive 里占用的最大槽位下标 + 1（与 gen 的 `group_slots` 同语义）。
fn group_slot_count(cell: &CellDef, group: &str) -> usize {
    cell.receive
        .keys
        .iter()
        .filter_map(|k| {
            let pkg = k.pkg.as_deref()?;
            let mut it = pkg.split('.');
            if it.next()? != group { return None; }
            let n: usize = it.next()?.strip_prefix('W')?.parse().ok()?;
            if n == 0 { None } else { Some(n) }
        })
        .max()
        .unwrap_or(0)
}

/// 静态可知的集合长度：字面数组 `[a, b, c]`（空数组 = 0）；`(a..=b)` 数字闭区间。
///
/// 其他形态（标识符、`collect()`、函数调用…）返回 `None` = 运行期未知。
fn static_collection_len(_program: &Program, _cell: &CellDef, text: &str) -> Option<usize> {
    let t = text.trim();
    if let Some(inner) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        let inner = inner.trim();
        if inner.is_empty() {
            return Some(0);
        }
        return count_top_level_commas(inner).map(|c| c + 1);
    }
    if let Some(inner) = t.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
        let inner = inner.trim();
        if let Some(idx) = inner.find("..=") {
            let lo = inner[..idx].trim().parse::<i128>().ok()?;
            let hi = inner[idx + 3..].trim().parse::<i128>().ok()?;
            return if hi >= lo { Some((hi - lo + 1) as usize) } else { Some(0) };
        }
    }
    None
}

/// 数**顶层**逗号（忽略嵌套 `[] () {} <>` 与字符串字面量内部）。
fn count_top_level_commas(inner: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut commas = 0usize;
    let mut in_str = false;
    let mut prev_backslash = false;
    for c in inner.chars() {
        if in_str {
            if c == '"' && !prev_backslash {
                in_str = false;
            }
            prev_backslash = c == '\\' && !prev_backslash;
            continue;
        }
        match c {
            '"' => in_str = true,
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth -= 1,
            ',' if depth == 0 => commas += 1,
            _ => {}
        }
    }
    if depth != 0 || in_str {
        return None; // 不合法/不可解析 → 当作运行期未知
    }
    Some(commas)
}

/// 单源跨边校验（Plain / Recurse）。
fn check_single_target(
    cell: &CellDef,
    j: &JumpStmt,
    target: &str,
    table: &ContractTable,
    diags: &mut Vec<Diagnostic>,
) {
    let recv_keys = match table.cell_inputs.get(target) {
        Some(v) => v,
        None => return, // 已在上层报 E042
    };

    // jump.args 的键集 → 目标 cell 的 receive 必须被覆盖
    let edge_keys: HashSet<String> = j
        .args
        .iter()
        .map(|a| a.effective_key())
        .filter(|k| !k.is_empty())
        .collect();

    // 单源场景：每个 recv_keys 必须在 edge_keys 里
    for k in recv_keys {
        if !edge_keys.contains(k) {
            diags.push(
                Diagnostic::error(
                    code::E042,
                    format!("目标 Cell `{target}` 的 receive 键 `{k}` 未在 jump 实参中出现"),
                    j.span,
                )
                .with_detail(format!(
                    "§11.1：jump 实参键集 = {:?}",
                    edge_keys.iter().collect::<Vec<_>>()
                ))
                .with_cell(cell.name.clone()),
            );
        }
    }
    let _ = cell;
}

/// `jump and` 的实参元数检查（§7.1 等价展开：over 元素 = 那一个 `go with(arg)` 实参）。
///
/// 规则：目标 Cell 的 receive 键中，**恰好一个** 由 over 元素供给，其余必须由
/// `go with` / `read with` 显式给出。历史上 gen 只会 `{callee}(item)`（单参），
/// 多参目标要么撞 rustc 元数错误、要么静默丢参数。现在契约期就报清。
fn check_and_arity(program: &Program, cell: &CellDef, j: &JumpStmt, diags: &mut Vec<Diagnostic>) {
    let edge_keys: HashSet<String> = j
        .args
        .iter()
        .map(|a| a.effective_key())
        .filter(|k| !k.is_empty())
        .collect();
    for target in &j.targets {
        let Some(tc) = program.cell(target) else { continue };
        let unfilled: Vec<String> = tc
            .receive
            .keys
            .iter()
            .map(|k| k.name.clone())
            .filter(|k| !edge_keys.contains(k))
            .collect();
        if j.over.is_some() {
            if unfilled.len() > 1 {
                diags.push(
                    Diagnostic::error(
                        code::E042,
                        format!("`jump and {target}` 的 over 只能供给 1 个实参，`{target}` 还有 {} 个 receive 键没有来源", unfilled.len()),
                        j.span,
                    )
                    .with_detail(format!(
                        "§7.1：over 元素对应单个实参；其余键请用 `go with(..)` / `read with(..)` 给出。缺：{unfilled:?}"
                    ))
                    .with_cell(cell.name.clone()),
                );
            }
        } else if !unfilled.is_empty() {
            diags.push(
                Diagnostic::error(
                    code::E042,
                        format!("`jump and {target}` 没有 over，且 {target} 的 receive 键 {unfilled:?} 无来源"),
                    j.span,
                )
                .with_detail("§7.1：无 over 时所有 receive 键都必须由 go with / read with 显式传入".to_string())
                .with_cell(cell.name.clone()),
            );
        }
    }
}

/// 处理 sync 的 E035。
///
/// `stmt_idx` 是当前 sync 在 cell body 中的下标；只扫描 **它之前的** 语句来判断：
/// - group 是否在更早的 `jump and ... group <g>` **或** `jump recurse ... group <g>`
///   中创建过（§6.3 明确 `jump recurse` 也创建 group——spec §6.3 / 附录 A.7 用例）
/// - 该 group 是否已经被更早的 `jump sync <g>` sync 过
///
/// 之前的实现 bug：把整个 body 的所有 sync 都算入 synced 集合，导致合法程序误报 E035
/// "已 sync 过"。修法是只看当前 sync 之前的部分。
fn check_sync_stmt(cell: &CellDef, group: &Ident, span: rscel_parser::Span, stmt_idx: usize, diags: &mut Vec<Diagnostic>) {
    let mut created: HashSet<String> = HashSet::new();
    let mut synced: HashSet<String> = HashSet::new();
    for (i, stmt) in cell.body.iter().enumerate() {
        if i >= stmt_idx { break; }
        match stmt {
            Stmt::Jump(j) if matches!(j.kind, JumpKind::And | JumpKind::Recurse) => {
                if let Some(g) = &j.group {
                    if g == group {
                        created.insert(g.clone());
                    }
                }
            }
            Stmt::Sync { group: g, .. } if g == group => {
                synced.insert(g.clone());
            }
            _ => {}
        }
    }
    if !created.contains(group) {
        diags.push(
            Diagnostic::error(
                ccode::E035,
                format!("`jump sync {group}` 引用的 group 未在本 Cell 创建"),
                span,
            )
            .with_detail(
                "§6.3/§6.5：`jump and ... group <g>` 或 `jump recurse ... group <g>` \
                 必须先于 `jump sync <g>` 出现"
                    .to_string(),
            )
            .with_cell(cell.name.clone()),
        );
        return;
    }
    if synced.contains(group) {
        diags.push(
            Diagnostic::error(
                ccode::E035,
                format!("`jump sync {group}` 已 sync 过"),
                span,
            )
            .with_detail("§5.5：同一 group 只能 sync 一次".to_string())
            .with_cell(cell.name.clone()),
        );
    }
}

// ---------------------------------------------------------------- receive 多包校验（E043 / E044）

/// 校验多包 receive（E043 / E044）。
///
/// 包名形态是 `<group>.W<n>`（§9.4 / D5），所以：
/// 1. 在本 Cell 里找到创建 `group` 的那条 jump（`jump and` 或 `jump recurse`），
///    拿到它的目标列表与是否有 `over`；
/// 2. 槽位 `W<n>` → 子 Cell：多目标按**入组顺序**（`W1` = 第 1 个目标，§6.4）；
///    单目标（含 `over` 派生 N 个同 Cell 实例）时所有槽位都指向该目标；
/// 3. 键名必须在所选子 Cell 的输出契约里，否则 E044。
///
/// 修前的两个缺陷：
/// - 用**全局** `table.group_members`（按 group 名合并了不同 Cell 的同名 group），
///   且把 `pkg`（`g1.W1`）直接与 Cell 名比较 → 规范 §6.4 的合法样例被判 E044；
/// - `children.len() >= 2` 的门槛让**单目标** group（最常见形态）完全不校验键名，
///   于是 `g1.W1.不存在的键` 一路溜到 rustc 才炸。
pub fn check_multisrc_receive(
    cell: &CellDef,
    table: &ContractTable,
    diags: &mut Vec<Diagnostic>,
) {
    // 本 Cell 创建过的 group（`jump and` / `jump recurse` 带 group 的）。
    let mut groups_created: Vec<String> = Vec::new();
    for stmt in &cell.body {
        if let Stmt::Jump(j) = stmt {
            if matches!(j.kind, JumpKind::And | JumpKind::Recurse) {
                if let Some(g) = &j.group {
                    if !groups_created.contains(g) {
                        groups_created.push(g.clone());
                    }
                }
            }
        }
    }

    // 1) 包名**形状**校验：`<group>.W<n>` 必须是本 Cell 某个已创建 group 的槽位。
    //    这一步与 `jump sync` 无关 —— `receive(z.key)` / `receive(g9.W1.a)`（g9 没创建）
    //    这类拼写错误要无条件报出来，而不是因为"本地找不到创建者"就整段跳过。
    for k in &cell.receive.keys {
        let Some(pkg) = &k.pkg else { continue };
        let matched = groups_created.iter().any(|g| parse_slot(pkg, g).is_some());
        if !matched {
            diags.push(
                Diagnostic::error(
                    code::E044,
                    format!("包名 `{pkg}` 不是本 Cell 任何 group 的合法槽位"),
                    k.span,
                )
                .with_detail(format!(
                    "§9.4 / D5：形态必须是 `<group>.W<n>`（n 从 1 开始）。本 Cell 创建的 group = {groups_created:?}"
                ))
                .with_cell(cell.name.clone()),
            );
        }
    }

    // 2) 逐个 `jump sync`：槽位 → 子 Cell，键名必须在该子 Cell 的输出契约里。
    for stmt in &cell.body {
        let Stmt::Sync { group: g, .. } = stmt else { continue };
        let Some(creator) = find_group_creator(cell, g) else { continue };
        let targets = &creator.targets;
        // 多源（≥2 个目标且非 over 派生）时，裸键必须包限定（E043）。
        let multi_source = targets.len() >= 2;
        for k in &cell.receive.keys {
            match &k.pkg {
                None => {
                    if multi_source {
                        diags.push(
                            Diagnostic::error(
                                code::E043,
                                "多源并行组 receive 必须使用 `包.变量` 限定",
                                k.span,
                            )
                            .with_detail(format!(
                                "§9.4：group `{g}` 的目标为 {:?}，裸键 `{}` 无法判定来源",
                                targets, k.name
                            ))
                            .with_cell(cell.name.clone()),
                        );
                    }
                }
                Some(pkg) => {
                    // 不属于本 group 的包名由第 1 步（形状校验）或它自己的 sync 处理，
                    // 这里直接跳过，避免同一个键被双重诊断。
                    let Some(slot) = parse_slot(pkg, g) else { continue };
                    let Some(child) = slot_to_child(targets, slot, multi_source) else {
                        diags.push(
                            Diagnostic::error(
                                code::E044,
                                format!("槽位 `{pkg}` 超出 group `{g}` 的目标数 {}", targets.len()),
                                k.span,
                            )
                            .with_detail(format!(
                                "§6.4：多目标 `jump and` 的 W 编号按入组顺序对应目标 {:?}；\
                                 over 派生的 N 个实例共用一个目标",
                                targets
                            ))
                            .with_cell(cell.name.clone()),
                        );
                        continue;
                    };
                    let outs = table.cell_outputs.get(child).cloned().unwrap_or_default();
                    if !outs.iter().any(|o| o == &k.name) {
                        diags.push(
                            Diagnostic::error(
                                code::E044,
                                format!("键 `{}` 不在子 Cell `{child}` 的输出契约中", k.name),
                                k.span,
                            )
                            .with_detail(format!(
                                "§9.4：子 Cell `{child}` 输出键集 = {outs:?}"
                            ))
                            .with_cell(cell.name.clone()),
                        );
                    }
                }
            }
        }
    }
}

/// 在本 Cell 里找到创建 `group` 的 jump 语句（`jump and` / `jump recurse`）。
fn find_group_creator<'a>(cell: &'a CellDef, group: &str) -> Option<&'a JumpStmt> {
    cell.body.iter().find_map(|s| match s {
        Stmt::Jump(j) if matches!(j.kind, JumpKind::And | JumpKind::Recurse) => {
            if j.group.as_deref() == Some(group) { Some(j) } else { None }
        }
        _ => None,
    })
}

/// `g1.W3` + group `g1` → `Some(3)`；不匹配 group 或非 W 形态 → `None`。
fn parse_slot(pkg: &str, group: &str) -> Option<usize> {
    let mut it = pkg.split('.');
    if it.next()? != group {
        return None;
    }
    let n: usize = it.next()?.strip_prefix('W')?.parse().ok()?;
    if n == 0 || it.next().is_some() {
        return None;
    }
    Some(n)
}

/// 槽位 → 子 Cell 名。多目标按入组顺序（`W1` = targets[0]）；单目标全部指向它。
fn slot_to_child(targets: &[Ident], slot: usize, multi_source: bool) -> Option<&str> {
    if !multi_source {
        return targets.first().map(String::as_str);
    }
    targets.get(slot - 1).map(String::as_str)
}

/// 收集 cell 的输出契约（exit go with 键集并集）。
pub fn collect_output_keys(cell: &CellDef) -> Vec<Ident> {
    use std::collections::BTreeSet;
    let mut set: BTreeSet<Ident> = BTreeSet::new();
    for stmt in &cell.body {
        if let Stmt::Exit { args, .. } = stmt {
            for a in args {
                let k = a.effective_key();
                if !k.is_empty() {
                    set.insert(k);
                }
            }
        }
    }
    set.into_iter().collect()
}

/// 收集 cell 是否只能 die（无 exit go with）。
pub fn cell_only_dies(cell: &CellDef) -> bool {
    !cell.body.iter().any(|s| matches!(s, Stmt::Exit { .. }))
}

/// 收集 `over ...` 中过滤算子是否含 DSL 关键字（用于 C7）。
pub fn over_op_dsl_kinds(op: &OverOp) -> Vec<&'static str> {
    let text = match op {
        OverOp::Filter(r) | OverOp::Map(r) | OverOp::FlatMap(r) | OverOp::Take(r) | OverOp::Skip(r) => &r.text,
    };
    let mut kinds = Vec::new();
    for kw in &["jump", "exit go with", "go with", "read with", "die", "receive"] {
        if text.contains(kw) {
            kinds.push(*kw);
        }
    }
    kinds
}

/// 去掉包前缀后的键名（g1.W1.A → A）。
pub fn strip_pkg(name: &str) -> String {
    match name.rfind('.') {
        Some(i) => name[i + 1..].to_string(),
        None => name.to_string(),
    }
}

/// 简化：判定 receive 键是否裸键（无包前缀）。
pub fn is_bare_key(k: &PacketKey) -> bool {
    k.pkg.is_none()
}

/// 工具：从 arg 提取"有效键名"（E040 检查已用，这里做"是否提供键"判断）。
#[allow(dead_code)]
pub fn arg_key(arg: &Arg) -> String {
    arg.effective_key()
}

/// 收口：把 cell.body 中所有 exit 的 args 转成 Ident 列表。
#[allow(dead_code)]
pub fn exit_arg_keys(cell: &CellDef) -> Vec<Ident> {
    let mut out = Vec::new();
    for s in &cell.body {
        if let Stmt::Exit { args, .. } = s {
            for a in args {
                let k = a.effective_key();
                if !k.is_empty() {
                    out.push(k);
                }
            }
        }
    }
    out
}

/// 是否存在强制 read with（小 POD 报 E063 的入口在 ownership.rs）。
pub fn is_force_read_with(mode: Option<TransMode>) -> bool {
    matches!(mode, Some(TransMode::ForceReadWith))
}

// 注：`Severity::Error` 通过 Diagnostic::error() 隐式使用；导出以便外部模块引用。
pub use rscel_parser::diag::Severity as _DiagSeverity;
#[doc(hidden)]
pub use rscel_parser::diag::Severity as SeverityReexport;
// 公开 CellId 别名供其他模块使用
pub use crate::analysis::CellId as _CellIdReexport;