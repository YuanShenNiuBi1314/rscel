//! 检查 C4：所有权 / 借用（§11.4）。
//!
//! 错误码：
//! - RSC-E060：变量已 move 重复使用；
//! - RSC-E061：冻结变量被 move；
//! - RSC-E062：move 后/冻结变量被透传区引用；
//! - RSC-E063：小 POD 禁 read with；
//! - RSC-E064：Arc 句柄不能出现在 go with 实参。
//!
//! **简化策略**（§11.4 + 主会话任务书 §3.2）：
//! - 状态机 `Active | Moved | Frozen | Copy`；受跟踪的名字 = **receive 别名 + 透传区里
//!   深度 0 的 `let` 绑定**（§10.1 的三类状态原本只定义在 receive 别名上，`let` 变量
//!   入表是 docs/05 P1-2 补齐的那半截：`over items` 之后 `items.len()` 必须在契约期就报，
//!   而不是等 rustc 的 E0382）；
//! - `jump loop` 的 fallthrough 用**语句之前**的状态（见下方控制流说明）；
//! - 不做完整 Rust 借用分析。
//!
//! ## 为什么 Copy 是一个状态而不是"查类型表"
//!
//! §10.2 说 POD 是 Copy、不走 move。判据来自**声明处的类型注解**：
//! `receive(n: i64)` 与 `let n: i64 = ..` 都在登记时直接把该名字标成 [`State::Copy`]，
//! 于是 `note_move` / 引用扫描 / `jump loop` 的状态回滚三处都只问一句"这个状态是什么"，
//! 不必在每条路径上回头去查 `receive` 的下标对齐（那条查找曾在 `let` 变量上永远返回
//! `None`，于是 `let items: Vec<i64>` 与 `let n: i64` 被同等对待，Copy 变量被误报 E060）。
//! **推不出 Copy 就照非 Copy 跟踪**（与"未注解的 `receive` 别名"同口径，D2 的
//! "推不出就要求注解"在这里的表达是：想要 POD 豁免就写 `let n: i64 = ..`）。
//!
//! ## 控制流处理是保守过近似（over-approximation）
//!
//! `jump`/`exit`/`die` 在运行时终结控制流，但本检查**不在这些语句处截断**状态推进，
//! 一律继续分析后续语句。理由有二：
//! 1. 规格未定义"终结语句之后还有语句"的诊断（附录 B 无此错误码），
//!    所以这类代码对本检查器而言是**语法合法**的；
//! 2. 若在 `jump`/`exit` 处截断，则线性 cell（无回边、无 `when` 分支汇合）里
//!    E060/E061 永远不会被触发，所有权检查将退化为空检查。
//!
//! 代价：对真正的死代码（`jump`/`exit` 之后的语句）可能报 E060/E061 假阳性。
//! 这是刻意选择的"宁可误报、不可漏报"方向——所有权违规应在死代码里同样被指出。
//!
//! 两个**方向相反**的保守选择（都是"宁可漏报，不可误报"，因为误报会拦住合法程序，
//! 而漏报仍有 rustc E0382 兜底）：
//! - **引用点只认花括号深度 0**：透传文本里没有类型信息，模式绑定（`match` 分支、
//!   `if let Some(x)`、`fn` 形参）与"读一个变量"在文本上同形。`{}` 体内的东西一律
//!   不扫，括号/方括号里的照扫（`println!("{}", items.len())` 这种真实踩坑形状必须能报）。
//!   `let`/`for`/`fn` 的绑定区按词法跳过，所以 `let items = items.len()` 里等号左边的
//!   `items` 不算引用、等号右边的算。
//! - **`read with` 的冻结只登记在 receive 别名上**：§8.2 说冻结后"只读"，而 §10.3 又把
//!   "透传区引用已 Frozen"定为 E062，两条对 `let` 变量同时成立的唯一安全读法是**不扩**。
//!   给 `let` 变量加冻结会让 `read with(payload)` 之后继续读 `payload`（合法！）报 E062。

use std::collections::{BTreeSet, HashMap};

use rscel_parser::ast::{is_plain_ident, RawBlock, RawBlockPart};
use rscel_parser::diag::{code, Diagnostic};
use rscel_parser::{Arg, CellDef, Ident, JumpKind, JumpStmt, OverOp, Span, Stmt, TransMode};

use crate::analysis::codes as ccode;

/// 变量状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Active,
    Moved,
    Frozen,
    /// 已知 Copy（§10.2：小 POD）——不走 move，可反复使用。
    Copy,
}

/// 检查 C4：所有权（§11.4）。
pub fn check_ownership(cell: &CellDef) -> Vec<Diagnostic> {
    let mut diags = Vec::new();

    // 初始状态：receive 别名 = Active，带 POD 注解的 = Copy（§10.2）
    let initial: HashMap<Ident, State> = cell
        .receive
        .aliases
        .iter()
        .map(|a| {
            let st = if alias_declared_ty(cell, a).as_deref().is_some_and(is_pod) {
                State::Copy
            } else {
                State::Active
            };
            (a.clone(), st)
        })
        .collect();

    let mut stack: Vec<(HashMap<Ident, State>, usize)> = vec![(initial, 0)];
    let mut visited: BTreeSet<(Vec<u64>, usize)> = BTreeSet::new();

    while let Some((state, idx)) = stack.pop() {
        if idx >= cell.body.len() {
            continue; // 到 body 末尾（合法或被 C2 catch）
        }
        let key = state_key(&state, idx);
        if !visited.insert(key) {
            continue;
        }

        let stmt = &cell.body[idx];
        // `jump loop` 的 fallthrough 需要"这条语句之前"的状态，所以留一份副本。
        let before = state.clone();
        let mut after = state;
        apply_stmt(cell, stmt, &mut after, &mut diags);

        match stmt {
            Stmt::Jump(j) if j.kind == JumpKind::Loop => {
                // 回边不重走：body 里的语句已经从入口分析过一遍，而 §10.4 保证 `let`
                // 变量每轮重新绑定、`jump loop` 的实参按位置刷新 receive 别名——两者在
                // 下一轮走的是**同一条语句的文本**，再走一遍只会把同一条诊断报两次。
                //
                // `when` 为假的 fallthrough：本轮的 `go with(...)` 根本没有执行，
                // 所以移动不能记账，用 `before` 而不是 `after`。
                if j.condition.is_some() {
                    stack.push((before, idx + 1));
                }
                // 没有 `when` 的 `jump loop` 是无条件回边，后续语句永不执行
                // （那种写法由 C2 的 E010 负责报），这里不推进。
            }
            Stmt::Die { .. } => { /* die: 出口 */ }
            _ => stack.push((after, idx + 1)),
        }
    }

    diags
}

/// 一条语句对所有权状态的**效果**：引用诊断 + `let` 绑定 + move/freeze 转移。
///
/// 控制流不在这里处理（由 [`check_ownership`] 的循环决定推进哪一份状态）。
fn apply_stmt(cell: &CellDef, stmt: &Stmt, state: &mut HashMap<Ident, State>, diags: &mut Vec<Diagnostic>) {
    match stmt {
        Stmt::RawRust(r) => apply_text(cell, &r.text, r.span, state, diags),
        Stmt::Out { args, .. } => {
            for arg in args {
                apply_text(cell, &arg.text, arg.span, state, diags);
            }
        }
        Stmt::Jump(j) => apply_jump(cell, j, state, diags),
        Stmt::Exit { args, span } => {
            for arg in args {
                apply_arg(cell, arg, state, *span, diags);
            }
        }
        Stmt::RustBlock { block } => {
            // 块内是 Rust 的作用域：里面的 `let` 绑定与 move **不外泄**到块后语句
            // （外泄会把"只在某条分支里发生的 move"记成无条件发生，那是误报）。
            let mut scratch = state.clone();
            apply_block(cell, block, &mut scratch, diags);
        }
        Stmt::Die { .. } | Stmt::Sync { .. } | Stmt::ExternRustBlock { .. } => {}
    }
}

/// 含嵌套 DSL 语句的 Rust 块：逐段递归（`extern rust{}` 由 C8 管，不在这里）。
fn apply_block(cell: &CellDef, block: &RawBlock, state: &mut HashMap<Ident, State>, diags: &mut Vec<Diagnostic>) {
    for part in &block.parts {
        match part {
            RawBlockPart::Text(t) => apply_text(cell, &t.text, t.span, state, diags),
            RawBlockPart::Stmt(s) => apply_stmt(cell, s, state, diags),
        }
    }
}

/// 透传文本：先按**当前**状态报引用（E062），再把其中的 `let` 绑定登记进状态表。
///
/// 顺序不能反：`let items = items.len()` 里等号左边的 `items` 是**新绑定**（扫描器已把它
/// 排除在引用之外），等号右边的 `items` 才是真正的引用，必须用移动前的状态来判。
fn apply_text(
    cell: &CellDef,
    text: &str,
    span: Span,
    state: &mut HashMap<Ident, State>,
    diags: &mut Vec<Diagnostic>,
) {
    let scan = scan_passthrough(text);

    for name in &scan.refs {
        let st = match state.get(name.as_str()).copied() {
            Some(st) => st,
            None => continue,
        };
        if st == State::Active || st == State::Copy {
            continue;
        }
        diags.push(
            Diagnostic::error(ccode::E062, format!("变量 `{name}` 在透传区被引用"), span)
                .with_detail(format!(
                    "§11.4：变量状态 = {}，透传区文本 `{text}` 命中 `{name}`",
                    state_name(st)
                ))
                .with_cell(cell.name.clone()),
        );
    }

    for binding in scan.bindings {
        // 同名 `let` 是 Rust 的遮蔽：不管是 Moved 还是 Frozen，重新绑定后就该当作可用
        // （遮蔽 receive 别名由 C5 的 E070 单独报，这里不重复定罪）。
        let st = if binding.ty.as_deref().is_some_and(is_pod) {
            State::Copy
        } else {
            State::Active
        };
        state.insert(binding.name, st);
    }
}

fn apply_jump(cell: &CellDef, j: &JumpStmt, state: &mut HashMap<Ident, State>, diags: &mut Vec<Diagnostic>) {
    // `when` 条件在本条语句的实参移动**之前**求值，所以引用判在旧状态上。
    if let Some(cond) = &j.condition {
        apply_text(cell, &cond.text, cond.span, state, diags);
    }
    apply_jump_args(j, state, cell, diags);
    if let Some(over) = &j.over {
        // §10.1 / §7.1：`over <coll>` 把集合 move 进组，此后引用它是 use-after-move。
        let collection = over.collection.text.trim();
        if is_plain_ident(collection) {
            note_move(cell, collection, over.collection.span, state, diags, "`over` 集合");
        } else {
            // 复合表达式（`over (1..=4i64).collect()`）本身不是被移出的名字，
            // 但表达式里读到的变量该照常判。
            apply_text(cell, &over.collection.text, over.collection.span, state, diags);
        }
        for op in &over.ops {
            let text = match op {
                OverOp::Filter(r) | OverOp::Map(r) | OverOp::FlatMap(r) | OverOp::Take(r) | OverOp::Skip(r) => r,
            };
            apply_text(cell, &text.text, text.span, state, diags);
        }
    }
    if let Some(limit) = &j.concurrent_limit {
        apply_text(cell, &limit.text, limit.span, state, diags);
    }
}

fn apply_jump_args(
    j: &JumpStmt,
    state: &mut HashMap<Ident, State>,
    cell: &CellDef,
    diags: &mut Vec<Diagnostic>,
) {
    for arg in &j.args {
        // E064 / E063 判定需要 mode
        let is_arc_arg = arg_looks_like_arc(&arg.expr.text);
        let bare = is_plain_ident(arg.expr.text.trim());
        match j.mode {
            Some(TransMode::GoWith) | Some(TransMode::ForceGoWith) => {
                if is_arc_arg {
                    diags.push(
                        Diagnostic::error(
                            ccode::E064,
                            "Arc 句柄不能出现在 go with 实参",
                            arg.span,
                        )
                        .with_detail("§11.4：Arc 是只读共享；go with 要求独占所有权".to_string())
                            .with_cell(cell.name.clone()),
                    );
                    continue;
                }
                if !bare {
                    // 复合表达式（`go with(items.len() -> n)`）不移动任何东西——但它是
                    // 一次**读取**，读一个已经移出去的名字同样要报。
                    apply_text(cell, &arg.expr.text, arg.span, state, diags);
                    continue;
                }
                apply_arg(cell, arg, state, arg.span, diags);
            }
            Some(TransMode::ReadWith) | Some(TransMode::ForceReadWith) => {
                if !bare {
                    apply_text(cell, &arg.expr.text, arg.span, state, diags);
                    continue;
                }
                let name = arg.expr.text.trim();
                // 读一个已经移出去的名字同样是 use-after-move（§10.3：使用已 Moved 的
                // 名字 → E060）。**冻结只对 receive 别名登记**，理由见模块文档
                // "两个方向相反的保守选择"。
                if state.get(name).copied() == Some(State::Moved) {
                    diags.push(
                        Diagnostic::error(code::E060, format!("变量 `{name}` 已 move，重复使用"), arg.span)
                            .with_detail(
                                "§11.4：状态机 `Active → Moved` 后不可再用；本次来源 = `read with` 实参"
                                    .to_string(),
                            )
                            .with_cell(cell.name.clone()),
                    );
                    continue;
                }
                // E063：小 POD 禁 read with（仅非 force 时报，force 走 W060）。
                //
                // 判据是**变量的声明类型**（`receive(x: i64)` 的注解），
                // 不是实参表达式文本——§11.4 与《语言规范》§10.2 的标准样例都是
                // `receive(x: i64); jump other read with(x);`。
                if j.mode == Some(TransMode::ReadWith) {
                    if let Some(alias) = state_alias_for_arg(arg, &cell.receive.aliases) {
                        let decl_ty = alias_declared_ty(cell, &alias);
                        if decl_ty.as_deref().is_some_and(is_pod) {
                            let ty = decl_ty.as_deref().unwrap_or_default();
                            diags.push(
                                Diagnostic::error(
                                    ccode::E063,
                                    format!("小 POD 类型 `{alias}: {ty}` 禁止 read with"),
                                    arg.span,
                                )
                                .with_detail(format!(
                                    "§11.4：`{ty}` 是 Copy 类型，`read with` 的 Arc 转发得不偿失；请改用 `go with`，或显式 `force read with`（RSC-W060）"
                                ))
                                .with_cell(cell.name.clone()),
                            );
                        }
                    }
                }
                // §11.4 规则 3：`read with(x)` → state[x] = Frozen（除非已 Frozen，幂等）。
                // 实参是简单标识符且为本 Cell receive 别名时才迁移；其它（表达式/非别名）
                // 不动 state（与 rustc 借用分析对齐）。
                if let Some(alias) = state_alias_for_arg(arg, &cell.receive.aliases) {
                    let prev = state.get(&alias).copied().unwrap_or(State::Active);
                    if prev == State::Active {
                        state.insert(alias, State::Frozen);
                    }
                    // Frozen → 仍是 Frozen（幂等）；Copy → 不动（POD 无需转发）。
                }
            }
            None => {
                // 无 mode：按 plain 调用，不改 state
            }
        }
    }
}

fn apply_arg(
    cell: &CellDef,
    arg: &Arg,
    state: &mut HashMap<Ident, State>,
    span: Span,
    diags: &mut Vec<Diagnostic>,
) {
    // 仅对**简单标识符**实参（裸别名）做状态迁移；复合表达式是读取，
    // 由 apply_text 的引用扫描负责（`go with(items.len() -> n)` 之后仍可 `go with(items)`）。
    let text = arg.expr.text.trim();
    if !is_plain_ident(text) {
        return;
    }
    note_move(cell, text, span, state, diags, "`go with` 实参");
}

/// `Active → Moved` 转移，外加 §10.3 的两条违规诊断。
///
/// `source` 只进 detail 文案：`go with` 与 `over` 都会把值移走，但用户该看到的是
/// 自己写错的那一段，而不是抽象的"某处移动"。
fn note_move(
    cell: &CellDef,
    alias: &str,
    span: Span,
    state: &mut HashMap<Ident, State>,
    diags: &mut Vec<Diagnostic>,
    source: &str,
) {
    let st = state.get(alias).copied().unwrap_or(State::Active);
    match st {
        // Copy（§10.2）不走 move：`receive(n: i64)` / `let n: i64 = ..` 在登记时已标好。
        State::Copy => {}
        State::Moved => {
            diags.push(
                Diagnostic::error(code::E060, format!("变量 `{alias}` 已 move，重复使用"), span)
                    .with_detail(format!(
                        "§11.4：状态机 `Active → Moved` 后不可再用；本次来源 = {source}"
                    ))
                    .with_cell(cell.name.clone()),
            );
        }
        State::Frozen => {
            diags.push(
                Diagnostic::error(ccode::E061, format!("冻结变量 `{alias}` 被 move"), span)
                    .with_detail("§11.4：`read with` 后状态 = Frozen，不可再 move".to_string())
                    .with_cell(cell.name.clone()),
            );
        }
        State::Active => {
            state.insert(alias.to_string(), State::Moved);
        }
    }
}

/// 透传文本里一个 `let` 绑定。
struct Binding {
    name: Ident,
    /// `: <类型>` 注解文本；`None` = 未标注（按非 Copy 处理）。
    ty: Option<Ident>,
}

/// 透传文本的一次词法扫描结果。
#[derive(Default)]
struct Passthrough {
    /// 作为**值**被读取的标识符。
    refs: BTreeSet<Ident>,
    /// `let` 引入的绑定（含 tuple/`Some(x)` 这类嵌套模式里的名字）。
    bindings: Vec<Binding>,
}

/// 扫描一段 Rust 透传文本，分出"引用"与"绑定"。
///
/// 这不是 Rust 解析器，只做词法级判定，取舍写在模块文档里：
/// - 字符串/字符字面量与注释的内容一律跳过（`"items"` 不是引用）；
/// - **花括号深度 > 0 的内容不扫**：`match` 分支、块内 `let`、`fn` 体内都是绑定重灾区，
///   而本层没有类型信息可以把"模式里的 `x`"和"读一个 `x`"分开；
/// - `.` / `::` 之后的标识符不是变量（字段、方法、路径段）；
/// - `|` 之后的是闭包形参；`let` / `for` / `fn` 后面按词法跳过各自的绑定区
///   （`fn` 跳形参列表，`let`/`for` 跳模式区——模式区里没有读取）。
fn scan_passthrough(text: &str) -> Passthrough {
    let mut out = Passthrough::default();
    let mut i = 0usize;
    let mut brace = 0usize;
    while i < text.len() {
        let b = text.as_bytes()[i];

        // --- 注释 ---
        if b == b'/' {
            if text[i..].starts_with("//") {
                i = text[i..].find('\n').map(|off| i + off + 1).unwrap_or(text.len());
                continue;
            }
            if text[i..].starts_with("/*") {
                let end = text[i + 2..]
                    .find("*/")
                    .map(|off| i + 2 + off + 2)
                    .unwrap_or(text.len());
                i = end;
                continue;
            }
        }

        // --- 字面量 ---
        if let Some(end) = skip_string(text, i) {
            i = end;
            continue;
        }
        if let Some(end) = skip_char_or_lifetime(text, i) {
            i = end;
            continue;
        }

        // --- 括号深度 ---
        if b == b'{' {
            brace += 1;
            i += 1;
            continue;
        }
        if b == b'}' {
            brace = brace.saturating_sub(1);
            i += 1;
            continue;
        }

        // --- 标识符 ---
        if !is_ident_start(b) {
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i;
        while end < text.len() && is_ident_byte(text.as_bytes()[end]) {
            end += 1;
        }
        let name = &text[start..end];
        let prev = prev_significant(text, start);
        let is_member = matches!(prev, Some(b'.') | Some(b':'));
        if name == "let" && brace == 0 && !is_member {
            let (next, bindings) = scan_let(text, end);
            out.bindings.extend(bindings);
            i = next;
            continue;
        }
        if name == "for" && brace == 0 && !is_member {
            let (next, binders, _) = scan_pattern(text, end, &["in"]);
            out.bindings.extend(binders);
            i = next;
            continue;
        }
        if name == "fn" && brace == 0 && !is_member {
            // 跳过函数名与形参列表：`(a, b)` 里的都是绑定，不是读取。
            i = skip_fn_signature(text, end);
            continue;
        }
        if brace == 0 && !is_member && prev != Some(b'|') && !follows_path(text, end) {
            out.refs.insert(name.to_string());
        }
        i = end;
    }
    out
}

/// 模式区扫到什么位置停下来。
#[derive(Clone, Copy, PartialEq, Eq)]
enum PatStop {
    /// 顶层 `:`（后面是类型注解）。
    Colon,
    /// 顶层 `=`（后面是初始值，那才是读取发生的地方）。
    Eq,
    /// `;`（无初始值的 `let x;`）。
    Semi,
    /// `{`（`if let PAT {}` 这类没有初始值的模式）。
    Brace,
    /// 文本结束，或 `for` 的停止词 `in`。
    End,
}

/// `let` 之后：模式区 + 可选的 `: <类型>` 注解。
///
/// 返回（交回主循环继续扫描的偏移，绑定列表）。模式区里的标识符**一律不算读取**——
/// 那里没有求值发生；等号右边的初始值才由主循环扫成引用，所以
/// `let items = items.len()` 能在契约期报出 use-after-move，而等号左边的 `items`
/// 只是重新绑定（Rust 的遮蔽），恢复成可用。
fn scan_let(text: &str, from: usize) -> (usize, Vec<Binding>) {
    let (pos, mut binders, stop) = scan_pattern(text, from, &[]);
    // 注解只在该名字是单个标识符时才登记（`let (a, b): (i64, i64) = ..` 无法逐名对应，
    // 按未标注处理 = 照非 Copy 跟踪，与未注解的 receive 别名同口径）。
    let mut next = pos;
    if stop == PatStop::Colon && binders.len() == 1 {
        let ann_start = skip_ws(text, pos + 1);
        let ann_end = annotation_end(text, ann_start);
        binders[0].ty = Some(text[ann_start..ann_end].trim().to_string());
        next = ann_end;
    }
    if next < text.len() && text.as_bytes()[next] == b'=' {
        next += 1;
    }
    (next, binders)
}

/// 扫一段**模式**（`let` 的左半边 / `for` 的循环变量），返回绑定名与停止位置。
///
/// `stops` 是额外的顶层停止词（`for` 用 `["in"]`）。`(`、`[` 之内出现的标识符视为嵌套
/// 模式里的绑定（`Some(x)`、`(a, b)`、`[p, q]`），同样登记成绑定 —— 宁可多登记几个
/// "可用"的名字（Active 状态不产生任何诊断），也不能把模式里的名字当成引用去报 E062，
/// 那是误报、会拦住合法程序。
fn scan_pattern(text: &str, from: usize, stops: &[&str]) -> (usize, Vec<Binding>, PatStop) {
    let mut binders: Vec<Binding> = Vec::new();
    let mut nest = 0usize;
    let mut i = skip_ws(text, from);
    while i < text.len() {
        if let Some(end) = skip_string(text, i) {
            i = end;
            continue;
        }
        let b = text.as_bytes()[i];
        if matches!(b, b'(' | b'[') {
            nest += 1;
            i += 1;
            continue;
        }
        if matches!(b, b')' | b']') {
            nest = nest.saturating_sub(1);
            i += 1;
            continue;
        }
        if b == b'{' {
            return (i, binders, PatStop::Brace);
        }
        if b == b';' {
            return (i + 1, binders, PatStop::Semi);
        }
        if nest == 0 {
            if b == b':' {
                return (i, binders, PatStop::Colon);
            }
            if b == b'=' && !is_relop_start(text, i) {
                return (i, binders, PatStop::Eq);
            }
        }
        if is_ident_start(b) {
            let start = i;
            let mut end = i;
            while end < text.len() && is_ident_byte(text.as_bytes()[end]) {
                end += 1;
            }
            let name = &text[start..end];
            if nest == 0 && stops.contains(&name) {
                return (skip_ws(text, end), binders, PatStop::End);
            }
            if !matches!(name, "mut" | "ref") && prev_significant(text, start) != Some(b'.') {
                binders.push(Binding { name: name.to_string(), ty: None });
            }
            i = skip_ws(text, end);
            continue;
        }
        i += 1;
    }
    (text.len(), binders, PatStop::End)
}

/// 类型注解的结束位置：顶层 `=` / `;` 之前。
///
/// 尖括号不记账（`HashMap<u64, [i64; 4]>` 里的 `;` 已经被方括号记账挡住），
/// 而类型文本里不会出现顶层 `=`，所以这条停止条件够用。
fn annotation_end(text: &str, from: usize) -> usize {
    let mut nest = 0usize;
    let mut i = from;
    while i < text.len() {
        if let Some(end) = skip_string(text, i) {
            i = end;
            continue;
        }
        let b = text.as_bytes()[i];
        if matches!(b, b'(' | b'[' | b'{') {
            nest += 1;
        } else if matches!(b, b')' | b']' | b'}') {
            nest = nest.saturating_sub(1);
        } else if nest == 0 && matches!(b, b'=' | b';') {
            return i;
        }
        i += 1;
    }
    text.len()
}

/// `fn` 之后：跳过函数名与形参列表，返回 `)` 之后的偏移。
fn skip_fn_signature(text: &str, from: usize) -> usize {
    let mut i = skip_ws(text, from);
    // 函数名
    while i < text.len() && is_ident_byte(text.as_bytes()[i]) {
        i += 1;
    }
    i = skip_ws(text, i);
    // 泛型参数 `<T, U>`：逐个尖括号配平地跳过
    if i < text.len() && text.as_bytes()[i] == b'<' {
        i = skip_balanced(text, i, b'<', b'>');
        i = skip_ws(text, i);
    }
    if i < text.len() && text.as_bytes()[i] == b'(' {
        return skip_balanced(text, i, b'(', b')');
    }
    i
}

/// 从 `from` 处的 `open` 开始跳过一对配平括号，返回右括号之后的偏移。
fn skip_balanced(text: &str, from: usize, open: u8, close: u8) -> usize {
    let mut depth = 0usize;
    let mut i = from;
    while i < text.len() {
        let b = text.as_bytes()[i];
        if let Some(end) = skip_string(text, i) {
            i = end;
            continue;
        }
        if b == open {
            depth += 1;
        } else if b == close {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return i + 1;
            }
        }
        i += 1;
    }
    i
}

/// `=` 处是否是复合运算符（`==` / `=>`），或左边是 `!` / `<` / `>`。
fn is_relop_start(text: &str, at: usize) -> bool {
    let next = text.as_bytes().get(at + 1).copied();
    if matches!(next, Some(b'=') | Some(b'>')) {
        return true;
    }
    let prev = prev_significant(text, at);
    matches!(prev, Some(b'!') | Some(b'<') | Some(b'>'))
}

/// 标识符之后紧跟 `::` 说明它是路径前缀（`Order::total`），不是变量读取。
fn follows_path(text: &str, end: usize) -> bool {
    let after = &text[end..];
    after.starts_with("::")
}

fn prev_significant(text: &str, at: usize) -> Option<u8> {
    let head = text[..at].trim_end();
    head.bytes().last()
}

fn skip_ws(text: &str, from: usize) -> usize {
    let mut i = from;
    while i < text.len() && matches!(text.as_bytes()[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// 从 `i` 跳过一段字符串字面量（含 `b".."` / `r".."` / `r#".."#` / `br#".."#`）。
fn skip_string(text: &str, i: usize) -> Option<usize> {
    // 主循环按字节推进，可能停在多字节字符中间（DSL 里常见中文注释），
    // 而 `&text[i..]` 要求字符边界。
    if !text.is_char_boundary(i) {
        return None;
    }
    let rest = &text[i..];
    let after_prefix = rest.strip_prefix("br").or_else(|| rest.strip_prefix("b")).unwrap_or(rest);
    let raw_offset = rest.len() - after_prefix.len();
    if let Some(after_r) = after_prefix.strip_prefix('r') {
        let hashes = after_r.len() - after_r.trim_start_matches('#').len();
        let body = after_r[hashes..].strip_prefix('"')?;
        let closer = format!("\"{}", "#".repeat(hashes));
        let at = body.find(&closer)?;
        return Some(i + raw_offset + 1 + hashes + 1 + at + closer.len());
    }
    let body = after_prefix.strip_prefix('"')?;
    let bytes = body.as_bytes();
    let mut j = 0usize;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => j += 2,
            b'"' => return Some(i + raw_offset + 1 + j + 1),
            _ => j += 1,
        }
    }
    None // 未闭合：交回主循环按普通字符处理
}

/// 从 `i` 跳过字符字面量；`'a` 这种生命周期前缀只跳过引号本身。
fn skip_char_or_lifetime(text: &str, i: usize) -> Option<usize> {
    if text.as_bytes().get(i) != Some(&b'\'') {
        return None;
    }
    let rest = &text[i + 1..];
    if let Some(body) = rest.strip_prefix('\\') {
        // 转义：`'\n'` / `'\''` / `'\u{1F600}'` —— 收尾的 `'` 之前都算字面量内容。
        let at = body.find('\'')?;
        return Some(i + 2 + at + 1);
    }
    let mut it = rest.chars();
    let first = it.next()?;
    if it.next() == Some('\'') {
        return Some(i + 1 + first.len_utf8() + 1);
    }
    Some(i + 1) // 生命周期
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// 类型文本是否属于 POD / Copy（§11.4）。
fn is_pod(ty: &str) -> bool {
    matches!(
        ty,
        "i8" | "i16" | "i32" | "i64" | "i128" | "isize"
            | "u8" | "u16" | "u32" | "u64" | "u128" | "usize"
            | "f32" | "f64" | "bool" | "char"
    )
}

fn state_alias_for_arg(arg: &Arg, aliases: &[Ident]) -> Option<Ident> {
    let text = arg.expr.text.trim();
    if rscel_parser::ast::is_plain_ident(text) {
        let a = text.to_string();
        if aliases.iter().any(|x| x == &a) {
            return Some(a);
        }
    }
    None
}

/// 取 receive 别名 `alias` 在 `receive(<key>: <ty>)` 里声明的类型文本。
///
/// 别名与键按下标一一对应（`receive(a, b) as (x, y)` → `x<->a`、`y<->b`）。
/// 未写类型注解时返回 `None`——此时不做 POD 判定（交给 rustc 的 Copy/move 规则）。
fn alias_declared_ty(cell: &CellDef, alias: &Ident) -> Option<String> {
    let pos = cell.receive.aliases.iter().position(|a| a == alias)?;
    cell.receive
        .keys
        .get(pos)?
        .ty
        .as_ref()
        .map(|t| t.text.trim().to_string())
}

fn arg_looks_like_arc(text: &str) -> bool {
    // 简化判定：含 `Arc<` 或 `Arc::` 子串即为 Arc 句柄。
    text.contains("Arc<") || text.contains("Arc::")
}

fn state_name(s: State) -> &'static str {
    match s {
        State::Active => "Active",
        State::Moved => "Moved",
        State::Frozen => "Frozen",
        State::Copy => "Copy",
    }
}

fn state_key(state: &HashMap<Ident, State>, idx: usize) -> (Vec<u64>, usize) {
    let mut entries: Vec<_> = state.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let mut codes: Vec<u64> = Vec::new();
    for (_, s) in entries {
        codes.push(*s as u64);
    }
    (codes, idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把扫描结果里的引用名取出来，方便断言。
    fn refs(text: &str) -> Vec<String> {
        scan_passthrough(text).refs.into_iter().collect()
    }

    fn bindings(text: &str) -> Vec<(String, Option<String>)> {
        scan_passthrough(text)
            .bindings
            .into_iter()
            .map(|b| (b.name, b.ty))
            .collect()
    }

    #[test]
    fn only_brace_depth_zero_occurrences_count_as_reads() {
        // 花括号里的东西（块作用域、`match` 分支）没有类型信息可分辨"绑定 vs 读取"，
        // 一律不扫 —— 这是"宁可漏报不误报"的那一侧。
        let text = r#"let n = a.len(); if c { let m = b.len(); }"#;
        assert!(refs(text).contains(&"a".to_string()));
        assert!(!refs(text).contains(&"b".to_string()), "实际 {:?}", refs(text));
        assert_eq!(bindings(text).len(), 1, "块内的 `let m` 不该进状态表");
    }

    #[test]
    fn let_pattern_names_are_bindings_not_reads() {
        // `let items = items.len()` 只有一个引用：等号右边。等号左边是重新绑定。
        assert_eq!(bindings("let items: Vec<i64> = v;"), vec![("items".to_string(), Some("Vec<i64>".to_string()))]);
        assert_eq!(bindings("let n: i64 = 5;")[0].1.as_deref(), Some("i64"));
        assert_eq!(bindings("let mut acc = 0;")[0].0, "acc");
        assert_eq!(bindings("let x;")[0].0, "x");
        // tuple / 路径模式里的名字全部登记成绑定，一个都不算读取
        assert!(!refs("let (p, q) = pair;").contains(&"p".to_string()));
        assert_eq!(bindings("let (p, q) = pair;").len(), 2);
        assert!(!refs("let Some(items) = picked;").contains(&"items".to_string()));
        assert!(refs("let Some(items) = picked;").contains(&"picked".to_string()));
    }

    #[test]
    fn members_paths_and_closure_params_are_not_reads() {
        assert!(!refs("let y = holder.x + std::time::Instant::now();").contains(&"x".to_string()));
        assert!(!refs("let y = holder.x;").contains(&"holder::".to_string()));
        // `|f| ..` 的形参是绑定
        assert!(!refs("|f| f.size > limit").contains(&"f".to_string()));
        assert!(refs("|f| f.size > limit").contains(&"limit".to_string()));
        // `fn` 的形参列表整段跳过；函数体在花括号里，本来也不扫
        assert!(!refs("fn helper(a: i64) -> i64 { a }").contains(&"a".to_string()));
    }

    #[test]
    fn strings_and_comments_are_not_reads() {
        // DSL 里中文注释常见，扫描器按字节推进时必须跳过字面量与注释的内容。
        assert!(!refs(r#"out("items"); // 这里提到 items 也不算引用"#).contains(&"items".to_string()));
        assert!(!refs(r#"let s = "他说\"items\"然后走";"#).contains(&"items".to_string()));
        assert!(refs("let s = items; /* items */").contains(&"items".to_string()));
    }

    #[test]
    fn non_ascii_passthrough_does_not_panic() {
        // `&text[i..]` 要求字符边界：按字节推进的扫描器在多字节字符中间切开会直接 panic。
        let text = "let 名称 = 计算(items); println!(\"结果={}\", 名称);";
        assert!(refs(text).contains(&"items".to_string()));
    }
}
