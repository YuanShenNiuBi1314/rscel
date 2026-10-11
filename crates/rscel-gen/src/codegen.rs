//! 主代码生成器：把 `Program` 翻译为单个 .rs 文件源码（S2 类型化调用约定）。
//!
//! 生成物结构：
//! ```text
//! // === 顶部：嵌入 block_on ===
//! fn __rsc_block_on(...) { ... }
//!
//! // === Ret 结构体（每 Cell 一个） ===
//! #[derive(Default)]
//! struct Ret_fact { result: i64 }
//!
//! // === Cell 定义 ===
//! async fn cell_<name>(<typed_recv_args>) -> Option<Ret_<name>> {
//!     let mut v_<id>: T = <typed_recv_arg>;
//!     // ... body（透传 + DSL 生成）...
//! }
//!
//! // === main ===
//! fn main() { let _ = __rsc_block_on(async { let _ = cell_<entry>().await; }); }
//! ```

use crate::mangle::{cell_fn_name, ret_struct_name};
use crate::rewrite::{rewrite, var_id};
use crate::runtime::RUNTIME_SOURCE;
use crate::srcmap::{
    extern_flag_for_stmt, next_line, span_of_stmt, written_lines, DslPos, MapKind, SrcMap,
    SrcMapBuilder,
};
use crate::ty::TypeEnv;
use rscel_parser::{Arg, CellDef, JumpKind, JumpStmt, OverOp, OverSpec, Program, Span, Stmt, TransMode};
use std::fmt::Write as _;

/// 代码生成选项（§16.1 编译模式里与生成器有关的那一维）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GenOptions {
    /// §15.3：`build --debug` 的运行期追踪埋点。
    ///
    /// **release 路径必须传 `false`**：性能门禁比的是生成物的运行耗时，而埋点会往
    /// 生成物里加语句；所以这个开关只允许在 `--debug` 为真。
    ///
    /// `false` 时的两条不变式（都由 `crates/rscel-gen/tests/gen_trace_test.rs` 钉住）：
    /// 1. **release 生成物与本功能存在之前逐字相同** —— 本批用 HEAD worktree 各建一份
    ///    二进制、对 9 份 DSL（4 个门禁场景 + `loop_sum_two_sites` + 3 份 examples）
    ///    的 `.rs` 与 `.map.json` 做了 diff，全部逐字相同，数据记在 `docs/05` P2 那一条。
    ///    这条是门禁分母不漂移的依据，所以新增埋点位置时必须继续走 `if ctx.trace`。
    /// 2. **debug 生成物 = release 生成物 + 整行新增**，唯一的例外是并发令牌的闭包：
    ///    release 是 `|__rsc_it| 调用`，debug 把它换成 `|__rsc_it| { 申请 / 调用 / 归还 }`
    ///    的多行块（per-element 的 `died` 只有在闭包里才拿得到）。所以"删掉含 `[rsc] `
    ///    的整行就相等"这句话**不成立**，成立的是测试
    ///    `debug_output_adds_only_whole_lines_apart_from_the_permit_closure` 断言的
    ///    「release 的每一行都还在 debug 里、顺序不变，缺的那几行只有 `__rsc_group_run_sync`
    ///    那一处」——把埋点写成内联子表达式（从而改动 release 那一行）是禁做的，
    ///    因为那一行属于性能门禁的生成物。
    pub trace: bool,
}

/// 单 Cell 的输入契约（receive 类型化参数）。
struct CellInputs {
    /// 参数名 → Rust 类型文本（来自 `receive(n: i64)` 的注解）。
    /// `as` 别名（`cur`/`sum`）就是形参名。
    typed_params: Vec<(String, String)>,
    /// 别名（as） → `v_<id>`（形如 `v_a`），供 rewrite 用。
    alias_to_var: Vec<(String, String)>,
    /// `let X = <rhs>;` 的集合（body RawRust 中），供 jump loop 重新求值用。
    let_bindings: Vec<(String, String)>,
}

/// 单 Cell 的输出契约（exit go with 键集合）。
/// 字段：(键名, Rust 类型)。`Some(Ret_<name>)` 用此构造 `Ret_<name> { key: <expr> }`。
struct CellOutputs {
    fields: Vec<(String, String)>, // (key, type)
}

/// 一个被「实体展开」的 Cell 的调用点素材（见 [`inline_expansion_targets`]）。
///
/// 里面**只有数据、没有预生成的文本**：语句必须在调用点直接发进最终的 `out`，
/// 这样 §12.9 的映射区间天然是全文件行号 —— 被展开 Cell 的每条 DSL 语句都还各自
/// 有落点。若改成「先发射到临时缓冲再抄进文件」，诊断反映射会整体错位（曾经实测：
/// 用临时缓冲的版本让 `errmap_test` 的 `out` 语句映射直接消失）。
struct InlineBody {
    /// 形参 `(名字, 类型文本)`，顺序 = `cell_x(..)` 的位置顺序。
    /// 未被使用的键带 `_` 前缀，与 [`emit_cell`] 的签名约定逐字一致，
    /// 这样 [`emit_receive_bindings`] 生成的 `v_<id>` 绑定行能对上名字。
    params: Vec<(String, String)>,
    /// 被展开 Cell 的输入契约。`collect_inputs` 需要 `TypeEnv`，而 `TypeEnv` 只在
    /// `generate` 里可得（`emit_jump` 拿不到），所以在计划阶段算好带过来。
    inputs: CellInputs,
    /// `jump loop` 已接管的 `let` 名（[`emit_stmt`] 的 `suppressed_lets` 入参）。
    suppressed_lets: Vec<String>,
    /// 被展开 Cell 是否有 `jump loop`（receive 绑定要不要 `mut`）。
    has_jump_loop: bool,
    /// 未被引用的 receive 别名（`_` 前缀判定，与 [`emit_cell`] 同源）。
    unused: Vec<bool>,
}

/// 生成期上下文：跨语句共享的判定结果与一次性状态。
struct Ctx<'a> {
    program: &'a Program,
    /// 需要 `async fn` 的 Cell 集合（fixpoint，见 [`needs_async_set`]）。
    async_cells: &'a std::collections::HashSet<String>,
    /// 已经做过 `Arc::new` 提升的变量名（§8.1 read with：同一变量只提升一次）。
    promoted: std::collections::HashSet<String>,
    /// 候选 1「热 Cell 真实体展开」的计划表（键 = 被展开的 Cell 名）。
    /// plain jump 命中这里就展开成块表达式，而不是调用 `fn cell_x(..)`。
    inline_bodies: &'a std::collections::HashMap<String, InlineBody>,
    /// `exit go with` 发成**块尾值**（`Some(Ret_x { .. })`）而不是 `return Some(..);`。
    ///
    /// 只在预生成展开体时置位。安全性由 [`inline_expansion_targets`] 的形状闸门保证：
    /// 被展开的 body 里除这条尾置 Exit 之外没有任何其他会产生 `return` 的语句，
    /// 所以「最后一次 Exit 即块尾」是唯一的可能。
    exit_as_tail: bool,
    /// 本条 DSL 语句里展开体占用的行区间 `[lo, hi]`（由 [`emit_inlined_block`] 填写）。
    ///
    /// [`emit_cell`] 据此把**调用方**那条 jump 的映射区间劈成头尾两段（块头 + 形参绑定 /
    /// 收尾行），让展开体内部的行归被调方自己的条目。这样整张表仍然「升序且互不重叠」
    /// —— 那是 §12.9 的既有不变式（`srcmap_test` 逐条验），不是可以为了展开放宽的东西。
    inline_cover: Option<(usize, usize)>,
    /// §15.3 埋点开关（来自 [`GenOptions::trace`]）。
    trace: bool,
}

impl<'a> Ctx<'a> {
    fn new(
        program: &'a Program,
        async_cells: &'a std::collections::HashSet<String>,
        inline_bodies: &'a std::collections::HashMap<String, InlineBody>,
        trace: bool,
    ) -> Self {
        Ctx {
            program,
            async_cells,
            promoted: std::collections::HashSet::new(),
            inline_bodies,
            exit_as_tail: false,
            inline_cover: None,
            trace,
        }
    }

    /// 目标 Cell 是否需要 async（决定调用点是否 `.await` / `__rsc_block_on`）。
    fn is_async(&self, cell: &str) -> bool {
        self.async_cells.contains(cell)
    }
}

// ---------------------------------------------------------------- §15.3 埋点
//
// `--debug` 生成物打到 stderr 的事件词汇表（§15.3，全部以 `[rsc] ` 开头）：
//
//   [rsc] cell <名> spawn @ 行:列 recv=[键名表]      Cell 实例创建 + 收到 Packet
//   [rsc] cell <名> jump -> <目标> @ 行:列 send=[..]  plain jump 发包
//   [rsc] cell <名> recurse -> <目标> @ 行:列 ...     jump recurse 内联路径发包
//   [rsc] cell <名> exit @ 行:列 send=[键名表]        exit go with 交回结果
//   [rsc] cell <名> implicit exit @ 行:列 send=[]     末尾 `when` 分支没命中时的兜底收敛
//   [rsc] cell <名> die @ 行:列                       die 触发的 DSL 行号
//   [rsc] group <组名> derive <调用方> -> <目标> limit={} items={} @ 行:列 send=[..]
//                                                      jump and / recurse 派生：整组派生规模
//   [rsc] group <组名> permit cell=<目标> acquire      并发令牌申请（每元素一次）
//   [rsc] group <组名> permit cell=<目标> release died=<是否 die>   令牌归还
//   [rsc] group <组名> done cell=<目标> slots={} died_in_slots={} @ 行:列
//                                                      jump sync 处的组完成状态
//
// 无组名（fire-and-forget，没有 `jump sync` 可等）时组名格子给 `(anon)`，
// 免得追踪流里出现空字段看起来像解析失败。
//
// **`done` 那一格只看得见被保留的槽位**：D5 让 `slots` 之外的结果在生成侧就被丢弃，
// 所以没有 `g1.Wk` 接收方时这里恒是 `slots=0 died_in_slots=0`，哪怕真有实例 die。
// 「哪个实例死了」的唯一真相来源是 `permit ... release died=`。
//
// 三处**有意不插**：
// - `extern rust{}` 块与顶层 Rust item 内部（§15.3「块内不插」）：埋点只由 codegen 在
//   自己发射的行里插入，用户 Rust 文本逐字透传，一行都不加；
// - `jump loop`：§9.2 明文它是按键名契约的**例外**（原地按位置刷新 `v_<id>`，不搬 Packet），
//   而它在热循环里每轮一次，埋上就是每条循环一行 stderr —— 那不是「追踪」是刷屏；
// - release 模式：一行都不发（`GenOptions::trace` = false）。
//
// 位置文本 `@ 行:列` 是 **DSL** 侧的行列（不是生成 `.rs` 的行号），与 §15.1 诊断、
// §12.9 映射表同源，所以编辑器可以拿一条追踪行直接高亮到 DSL 源。
// 这份词汇表在 `crates/rscel-gen/tests/gen_trace_test.rs::trace_event` 里有一份
// **可执行的同份拷贝**（每条埋点都必须落进这些形状之一）。

/// 发一条埋点行：`eprintln!("[rsc] <msg>", <实参…>;`。
///
/// **为什么直接写 `eprintln!` 而不是在内嵌运行时里放一个 `__rsc_trace`**：
/// `RUNTIME_SOURCE` 是**无条件**嵌进每个生成物的，往里加函数会改变 release 生成物的字节
/// （性能门禁的分母因此漂移）；埋点只该存在于 `--debug` 的生成物里。
/// 另一个好处是对类型零要求：`msg` 里只有编译期文本（键名列表）和 `{}` 位置的
/// `usize`/`bool`，不需要给 `Ret_x` 的字段加 `Debug` 约束。
fn emit_trace(out: &mut String, ind: &str, msg: &str, args: &[&str]) {
    write!(out, "{ind}eprintln!(\"[rsc] {msg}\"").unwrap();
    for a in args {
        write!(out, ", {a}").unwrap();
    }
    writeln!(out, ");").unwrap();
}

/// 埋点里的 DSL 位置文本（` @ 行:列`）。
///
/// AST 没有位置时（`line == 0`，见 [`crate::srcmap::DslPos::has_position`] 的同一判据）
/// 返回空串：埋点里出现 `@ 0:0` 会被编辑器当成真的行号。
fn trace_pos(span: Span) -> String {
    if span.line == 0 {
        String::new()
    } else {
        format!(" @ {}:{}", span.line, span.col)
    }
}

/// 埋点里的发送键名列表（§15.3「Packet 收发：键名列表」）。
///
/// 名字按 DSL 写法取（§9.1 的 [`Arg::effective_key`]）；推不出键名的实参用生成物里
/// `Ret_x` 的同名字段 `_{i}`，这样追踪文本能直接对上结构体字段。
fn trace_send_keys(args: &[Arg]) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(args.len());
    for (i, a) in args.iter().enumerate() {
        let k = a.effective_key();
        parts.push(if k.is_empty() { format!("_{i}") } else { k });
    }
    format!("[{}]", parts.join(", "))
}

/// 埋点里的接收键名列表：`receive(...)` 声明的键，多包键带包前缀（`g1.W1.A`），
/// 与 DSL 写法逐字一致 —— 拓扑调试要的是「这条边送来了哪些键」，不是内部的 `v_<id>`。
fn trace_recv_keys(cell: &CellDef) -> String {
    let parts: Vec<String> = cell
        .receive
        .keys
        .iter()
        .map(|k| match &k.pkg {
            Some(pkg) => format!("{pkg}.{}", k.name),
            None => k.name.clone(),
        })
        .collect();
    format!("[{}]", parts.join(", "))
}

/// `--debug` 埋点的组名文本：无组名（fire-and-forget）时给 `(anon)`，
/// 否则追踪流里会出现空字段名，看起来像解析失败。
fn trace_group_label(group: &str) -> String {
    if group.is_empty() {
        "(anon)".to_string()
    } else {
        group.to_string()
    }
}

/// §15.3：调用点（Packet 发送侧）的一行埋点。
///
/// `verb` 区分两种转移：`jump`（plain，本实例终止）与 `recurse`（子实例回来后父实例继续）。
/// 两者都在**实际被取用的那条分支内**发射 —— 带 `when` 的 jump 条件不成立时一行都不打，
/// 否则追踪流会显示一次根本没有发生的传输（那是比刷屏更糟的误导）。
fn trace_send(
    ctx: &Ctx,
    out: &mut String,
    ind: &str,
    caller: &str,
    verb: &str,
    target: &str,
    j: &JumpStmt,
) {
    if !ctx.trace {
        return;
    }
    emit_trace(
        out,
        ind,
        &format!(
            "cell {caller} {verb} -> {target}{} send={}",
            trace_pos(j.span),
            trace_send_keys(&j.args)
        ),
        &[],
    );
}

// ---------------------------------------------------------------- async 判定

/// 透传 Rust 文本里是否出现 `.await`。
///
/// 这是**唯一**让一个 Cell 变成 `async fn` 的原因：
/// - DSL 自己的构造都不需要 await —— `jump and` 交给 OS 线程池（`__rsc_run_group`）、
///   `jump sync` 是阻塞 `Condvar::wait`、`jump recurse` 按 D3 走**内联直接调用**；
/// - 反过来，若把含 recurse 的 Cell 也标成 async，自递归调用就会产生无限大小的
///   future（rustc E0733），只能装箱绕——而装箱正是 §17.1 要消灭的开销。
///
/// 所以默认同步、按需 async：既修了「调用点用 caller 的 async 位决定 await」这个
/// 老 bug（sync Cell 调 async Cell 会生成 `cell_x(..).await` 之外的错误形状），
/// 也让绝大多数程序跑在零 future 状态机的纯同步路径上。
fn text_has_await(t: &str) -> bool {
    t.contains(".await")
}

/// 收集 `cell` 自身透传文本里出现的全部 `.await` 来源。
fn cell_passthrough_has_await(cell: &CellDef) -> bool {
    use rscel_parser::{RawBlock, RawBlockPart};

    fn scan_block(b: &RawBlock, found: &mut bool) {
        for p in &b.parts {
            match p {
                RawBlockPart::Text(t) => *found |= text_has_await(&t.text),
                RawBlockPart::Stmt(s) => scan_stmt(s, found),
            }
        }
    }
    fn scan_stmt(s: &Stmt, found: &mut bool) {
        match s {
            Stmt::RawRust(r) => *found |= text_has_await(&r.text),
            Stmt::Jump(j) => *found |= jump_texts_have_await(j),
            Stmt::Exit { args, .. } => *found |= args.iter().any(|a| text_has_await(&a.expr.text)),
            Stmt::Out { args, .. } => *found |= args.iter().any(|a| text_has_await(&a.text)),
            Stmt::ExternRustBlock { body, .. } => *found |= text_has_await(&body.text),
            Stmt::RustBlock { block } => scan_block(block, found),
            Stmt::Sync { .. } | Stmt::Die { .. } => {}
        }
    }
    let mut found = false;
    for stmt in &cell.body {
        scan_stmt(stmt, &mut found);
    }
    found
}

/// 一条 jump 的所有透传文本（实参 / 条件 / over 管道 / concurrent_limit）是否含 `.await`。
fn jump_texts_have_await(j: &JumpStmt) -> bool {
    let mut found = j.args.iter().any(|a| text_has_await(&a.expr.text));
    found |= j.condition.as_ref().is_some_and(|c| text_has_await(&c.text));
    found |= j.concurrent_limit.as_ref().is_some_and(|c| text_has_await(&c.text));
    if let Some(o) = &j.over {
        found |= text_has_await(&o.collection.text);
        found |= o.ops.iter().any(|op| match op {
            rscel_parser::OverOp::Filter(c)
            | rscel_parser::OverOp::Map(c)
            | rscel_parser::OverOp::FlatMap(c)
            | rscel_parser::OverOp::Take(c)
            | rscel_parser::OverOp::Skip(c) => text_has_await(&c.text),
        });
    }
    found
}

/// Cell → 需要 async 的集合（自底向上 fixpoint）。
///
/// 传播规则：Cell 里有 plain / recurse（内联路径）/ jump loop 之外的**调用式** jump
/// 指向 async Cell 时，caller 也必须 async（要在调用点 `.await`）。
/// 例外：caller 是同步函数时不必 async —— 调用点改用 `__rsc_block_on(..)` 驱动，
/// 语义等价且不引入 future。所以 **本集合的 fixpoint 不沿 jump 边传播**，
/// 只有透传 `.await` 会让一个 Cell 变 async。
pub(crate) fn needs_async_set(program: &Program) -> std::collections::HashSet<String> {
    program
        .cells()
        .filter(|c| cell_passthrough_has_await(c))
        .map(|c| c.name.clone())
        .collect()
}

/// 主入口：emit 整个程序为一个 .rs 文件（不需要源映射时用这个）。
///
/// `.map.json` 是 §15.2 错误反映射的输入，`rscel-cli` 需要它，所以走
/// [`emit_program_mapped`]；本包装只给「只要源码」的调用方（测试、基准脚本）用。
/// 映射表照算、算完即丢——这是「用不上的副产物」，不是占位实现。
pub fn emit_program(program: &Program) -> String {
    emit_program_mapped(program, crate::srcmap::DEFAULT_FILE_NAME, GenOptions::default()).0
}

/// 主入口：emit 整个程序为一个 .rs 文件，**同时**产出 §12.9 的源映射表。
///
/// 插桩方式（**为什么这样最安全**）：每个记录点都是「调 `emit_*` 之前数一次 `out` 里
/// 已写完的行数、之后数一次」，再把 `[start, end]` 连同 DSL 侧的 `Span` 交给
/// [`SrcMapBuilder`]。生成逻辑本身（`emit_jump` / `emit_sync` / read with 提升 /
/// 组派生 / `out(...)` 的形状）一行都没改，252 个既有测试守着的行为不受影响；
/// 而行区间天然覆盖一条 DSL 语句展开出的任意多行（详见 [`crate::srcmap`] 模块注释）。
///
/// `file_name` 只写进 `.map.json` 的 `file` 字段：rustc 报的 `file_name` 以它结尾，
/// `errmap` 用这个后缀确认「这条诊断确实来自我们的生成文件」。
///
/// `opts.trace` = §15.3 的 `--debug` 埋点开关；`false`（release）时本函数一行埋点都不发，
/// 生成物与本功能存在之前逐字相同。埋点行是**在语句自己的发射区间内**多出来的行，
/// 所以既有条目只受「区间变长」这一种影响、顺序与 DSL 侧位置都不动。
/// 一个实测出来的例外：`SrcMapBuilder::push` 对零行区间不记条目，而空 `receive()` 的
/// 绑定块就是零行 —— 开了埋点后那里多出一行 spawn，于是 debug 会**多**一条
/// `receive(...)` 条目（映射的正是这条埋点自己）。两条都由
/// `gen_trace_test::debug_srcmap_keeps_the_sorted_non_overlapping_invariant` 钉住。
pub fn emit_program_mapped(
    program: &Program,
    file_name: &str,
    opts: GenOptions,
) -> (String, SrcMap) {
    let mut map = SrcMapBuilder::new(file_name);
    // `cells` 表先填：即使后面因类型推不出而提前返回，Cell 的 DSL 位置依然成立。
    for cell in program.cells() {
        map.register_cell(&cell.name, cell.span);
    }

    // 0. D2 类型解析（注解优先 → 入边传播 → 字面推导）。
    //    任何一项推不出时，S2 的类型化签名就写不出来（Rust item 签名不允许 `_`，E0121），
    //    此时不做任何生成，只把问题原样交给 rustc 的 `compile_error!` 报给用户。
    let env = TypeEnv::build(program);
    let type_errors = collect_type_errors(program, &env);
    if !type_errors.is_empty() {
        let mut out = String::new();
        for e in &type_errors {
            writeln!(out, "compile_error!({e:?});").unwrap();
        }
        // 补一行 `fn main() {}`：**不是**为了让这条路径能编译（`compile_error!` 已经
        // 保证必失败），而是为了让 rustc 别再白送一条 `error[E0601]`（main 找不到）。
        // 那条错误跟用户的 DSL 没有任何关系——用户既写不出也不需要 main——
        // 但它和真正该看的那条问题陈述同样醒目，于是诊断被稀释成「2 errors」。
        //
        // 会不会和「用户在顶层 `extern rust{}` 里自己写的 `fn main`」撞名？不会：
        // 这条提前返回发生在任何 item 落地之前，顶层 Rust item 一行都没生成，
        // 所以文件里 `fn main` 只有我们这一处。
        out.push_str("fn main() {}\n");
        // 这条路径的生成物只有几条 `compile_error!` 加这一行空 main，它们都是**类型求解器
        // 的问题陈述**、不是任何 DSL 语句的翻译，所以 `entries` 为空是事实而不是缺失：
        // errmap 会把它们归入「编译器自身生成区间」原样输出，不做反映射。
        // （也因此这里不调 `map.set_runtime_lines`：那行数记的是内嵌运行时的规模，
        //  而这条路径一行运行时都没有。）
        return (out, map.build());
    }

    let mut out = String::new();

    // 1. 顶部：嵌入运行时（block_on + 并发组）。
    //
    // P1-5：先 push 一行 `#![allow(dead_code)]`（内层 attribute），让
    // 没用到 jump and 的程序不会因 `__RscGroup::{new,finish,wait}` 或
    // `__rsc_run_group` 引用 0 次而触发 dead_code 告警。
    //
    // 这是 inner attribute（`#!`）而非 outer（`#`），作用域是本文件
    // /本模块；不会影响用户代码的告警。
    //
    // 为什么带上 `unused_assignments`：§6.3 要求 `return(<key> -> <别名>)` 的别名
    // 先声明成 `let mut 别名: Option<T> = None;`，而 recurse 一定会重新赋值它，
    // 那个 `= None` 初始值永远读不到 → rustc 报 unused_assignments。
    // 这是 **DSL 契约要求的写法**带来的必然告警（不是用户写错），而 P1-5 要求
    // 生成物零告警，所以在文件级允许它。（别名的声明形状由 E036 在 check 层把关。）
    //
    // `non_snake_case`：`Ret_<Cell>` 的字段名就是 DSL 的**包键名**（§5.1 / §6.4 里
    // 就是 `A` 这种大写名）。键名空间与 Rust 命名规范无关，把 `A` 改成 `a`
    // 会让 `receive(g1.W1.A)` 与生成字段对不上号。
    out.push_str("#![allow(dead_code, unused_assignments, non_snake_case)]\n");
    out.push_str(RUNTIME_SOURCE);
    out.push('\n');
    // 记下运行时头部占用的行数：`1..=runtime_lines` 是**编译器自身生成区间**，
    // 永远不进映射表（§15.2「查不到映射」分支靠这个边界解释诊断为什么没有 DSL 位置）。
    map.set_runtime_lines(written_lines(&out));

    // 2. Ret 结构体（每 Cell 一个）
    for cell in program.cells() {
        let ret_start = next_line(&out);
        let outs = collect_outputs(cell, &env);
        if outs.fields.is_empty() {
            writeln!(out, "#[derive(Default)]").unwrap();
            writeln!(out, "struct {} {{}}", ret_struct_name(&cell.name)).unwrap();
        } else {
            writeln!(out, "#[derive(Default)]").unwrap();
            writeln!(out, "struct {} {{", ret_struct_name(&cell.name)).unwrap();
            for (k, ty) in &outs.fields {
                writeln!(out, "    pub {k}: {ty},").unwrap();
            }
            writeln!(out, "}}").unwrap();
        }
        // 字段类型来自 `exit go with` 的实参：报错时最该指向该 Cell 的定义行。
        map.push(
            MapKind::RetStruct,
            DslPos::item(&cell.name, cell.span),
            false,
            ret_start,
            written_lines(&out),
        );
        out.push('\n');
    }

    // 3. 顶层 item 按文档顺序：cell / extern rust{} / entry / 顶层 Rust item（§3.5.4）
    for item in &program.items {
        match item {
            rscel_parser::Item::GlobalRust(r) => {
                // 顶层 extern rust { ... } 或顶层 Rust item（裸写）。
                // DSL 的 `extern rust { ... }` wrapper 不是 Rust 语法，必须剥掉。
                let g_start = next_line(&out);
                writeln!(out, "{}", strip_extern_rust(&r.text)).unwrap();
                // §3.5.4：顶层 Rust item 等价于「隐含的 extern rust」，块内全是用户 Rust
                // ⇒ in_extern = true，按 §15.2 走 RSC-E901（不做反映射）。
                map.push(
                    MapKind::GlobalExtern,
                    DslPos::item("", r.span),
                    true,
                    g_start,
                    written_lines(&out),
                );
                out.push('\n');
            }
            rscel_parser::Item::Cell(_) => {
                // 已在步骤 4 emit。
            }
        }
    }

    // 4. Cell 定义
    //
    // async 判定先算好（`needs_async_set`）：调用点要按**被调 Cell** 的 async 位
    // 决定形状，而不是按 caller 的 —— 修前的老 bug：`is_async`（caller 的）
    // 决定 await 后缀，sync Cell 调 async Cell 会漏 `.await`、
    // async Cell 调 sync Cell 会写出 `cell_x(..).await`（sync fn 不返回 future）。
    let async_cells = needs_async_set(program);

    // 4a. 候选 1「热 Cell 真实体展开」：先定计划（哪些同步 Cell 的每一个 plain 调用点都
    // 安全且总量在预算内），并在计划阶段备齐素材 —— `collect_inputs` 要 `TypeEnv`，而 `TypeEnv`
    // 只在这里可得，`emit_jump` 拿不到。展开体本身在**调用点**直接发射（见
    // [`emit_inlined_block`]），这样 §12.9 的映射行号不会被「抄进文件」弄错。
    let expansion_targets = inline_expansion_targets(program, &async_cells, &env);
    let mut inline_bodies: std::collections::HashMap<String, InlineBody> =
        std::collections::HashMap::new();
    for name in &expansion_targets {
        // 计划里的名字必然来自 `program.cells()`，见 `inline_expansion_targets`。
        let cell = program.cells().find(|c| &c.name == name).expect("展开计划里的 Cell 必存在");
        let inputs = collect_inputs(cell, &env);
        inline_bodies.insert(name.clone(), prepare_inline_body(cell, inputs));
    }

    // 4b. 正常发射每个 Cell。被展开的 Cell **不再发射独立函数**：
    // 留着它就是死代码，而且「消灭函数边界」正是这次优化的全部内容（数据见
    // `docs/06-控制变量实验.md`：同一份循环，函数边界值 2 倍）。
    for cell in program.cells() {
        if inline_bodies.contains_key(&cell.name) {
            continue;
        }
        let inputs = collect_inputs(cell, &env);
        let outs = collect_outputs(cell, &env);
        let mut ctx = Ctx::new(program, &async_cells, &inline_bodies, opts.trace);
        emit_cell(&mut out, cell, &inputs, &outs, &mut ctx, &mut map);
    }

    // 5. main
    emit_main(&mut out, program, &env, &async_cells, &mut map);

    (out, map.build())
}

/// 收集全程序的类型解析失败项（D2 的"推不出则报错要求注解"）。
///
/// 覆盖两处必须写具体类型的地方：
/// - Cell 签名参数（S2：`fn cell_x(<typed>) -> Option<Ret_x>`）；
/// - `Ret_x` 结构体字段（`exit go with` 的键类型）。
fn collect_type_errors(program: &Program, env: &TypeEnv) -> Vec<String> {
    let mut errs = Vec::new();
    for cell in program.cells() {
        for (i, key) in cell.receive.keys.iter().enumerate() {
            if key.ty.is_some() {
                continue; // 注解优先，无需推导
            }
            let alias = cell.receive.aliases.get(i).map(String::as_str).unwrap_or(&key.name);
            if env.declared_type(&cell.name, &key.name).is_none() {
                errs.push(format!(
                    "rscel: cell `{}` 的 receive 键 `{}`（别名 `{}`）无法确定类型；\
                     请写成 `receive({}: <类型>)`，或确保入边实参 / 调用点的 `fn` 签名\
                     足以推导出类型",
                    cell.name, key.name, alias, key.name
                ));
            }
        }
        for stmt in &cell.body {
            let Stmt::Exit { args, .. } = stmt else { continue };
            for a in args {
                let k = a.effective_key();
                if k.is_empty() {
                    continue; // 无键名 → E040 由 check 层报，不在此重复
                }
                if env.resolve_in(cell, &a.expr.text).is_none() {
                    errs.push(format!(
                        "rscel: cell `{}` 的 `exit go with` 实参 `{}` 无法确定类型；\
                         请给目标键写注解 `receive({k}: <类型>)`，\
                         或在表达式上写 `<表达式> as <基本类型>` 消歧\
                         （`as` 只覆盖 i64/f64/bool/String 这类基本类型名，\
                         `as Vec<i64>` 不是可用的注解形式）",
                        cell.name,
                        a.expr.text.trim()
                    ));
                }
            }
        }
    }
    errs
}

// ---------------------------------------------------------------- Cell 收集

/// 在 `cell` 上下文里解析 receive 键 `key` 的类型（D2）。
///
/// 调用前必须已通过 [`collect_type_errors`] 的校验——推不出的键不会走到这里。
fn resolve_key_type(cell: &CellDef, key: &str, env: &TypeEnv) -> String {
    if let Some(ty) = env.declared_type(&cell.name, key) {
        return ty.to_string();
    }
    // `collect_type_errors` 已保证每个无注解键都能解析；走到这里说明内部不一致。
    unreachable!("cell `{}` 的键 `{key}` 未解析类型（collect_type_errors 应已拦截）", cell.name)
}

fn collect_inputs(cell: &CellDef, env: &TypeEnv) -> CellInputs {
    let aliases: Vec<String> = if cell.receive.aliases.len() == cell.receive.keys.len() {
        cell.receive.aliases.clone()
    } else {
        cell.receive.keys.iter().map(|k| k.name.clone()).collect()
    };
    let mut typed_params = Vec::new();
    let mut alias_to_var = Vec::new();
    for (i, key) in cell.receive.keys.iter().enumerate() {
        let alias = &aliases[i];
        // D2：注解优先，其次入边传播（`TypeEnv::build` 已把两者合并进 `declared`）。
        let ty = resolve_key_type(cell, &key.name, env);
        let vid = format!("v_{}", var_id(i));
        typed_params.push((alias.clone(), ty));
        alias_to_var.push((alias.clone(), vid.clone()));
    }
    let let_bindings = collect_let_bindings(cell);
    CellInputs { typed_params, alias_to_var, let_bindings }
}

/// 扫描 cell.body 的 RawRust，提取所有 `let X = <rhs>;`（单行声明）。
/// 注：忽略 `let X: T = ...;` 注解（emit `let mut X;`，类型由首次赋值推断）。
fn collect_let_bindings(cell: &CellDef) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for stmt in &cell.body {
        if let Stmt::RawRust(r) = stmt {
            for line in r.text.lines() {
                let t = line.trim();
                let rest = match t.strip_prefix("let ") {
                    Some(r) => r,
                    None => continue,
                };
                // 找第一个 `=`（非 `==`）；简单做法：跳过 `name [: type] = `。
                let chars: Vec<char> = rest.chars().collect();
                let mut i = 0;
                // 跳过 ident
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                // 跳过可选 `: type`
                if i < chars.len() && chars[i] == ':' {
                    i += 1;
                    while i < chars.len() && chars[i] != '=' {
                        i += 1;
                    }
                }
                // 跳过空格
                while i < chars.len() && chars[i].is_whitespace() {
                    i += 1;
                }
                // 期望 =
                if i + 1 < chars.len() && chars[i] == '=' && chars[i + 1] != '=' {
                    let name: String = chars[..i].iter().collect();
                    let name = if name.contains(':') {
                        // 含类型注解，仅取 ident 部分
                        let n: String = name.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
                        n
                    } else {
                        name.trim().to_string()
                    };
                    let rhs_start = i + 1;
                    let rhs: String = chars[rhs_start..].iter().collect();
                    let rhs = rhs.trim_end_matches(';').trim().to_string();
                    if !name.is_empty() && !rhs.is_empty() {
                        out.push((name, rhs));
                    }
                }
            }
        }
    }
    out
}

fn collect_outputs(cell: &CellDef, env: &TypeEnv) -> CellOutputs {
    use std::collections::BTreeSet;
    let mut fields: Vec<(String, String)> = Vec::new(); // (key, type)
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for stmt in &cell.body {
        if let Stmt::Exit { args, .. } = stmt {
            for a in args {
                let k = a.effective_key();
                if !k.is_empty() && seen.insert(k.clone()) {
                    // D2：`collect_type_errors` 已保证此处可解析。
                    let ty = env
                        .resolve_in(cell, &a.expr.text)
                        .expect("collect_type_errors 应已拦截不可解析的 exit 实参");
                    fields.push((k, ty));
                }
            }
        }
    }
    CellOutputs { fields }
}

// ---------------------------------------------------------------- Cell emit

/// Cell 是否有显式出口（`exit go with` / `die` / 末尾的无条件 plain jump）。
///
/// 没有则补兜底 `return Some(Ret{})`。
fn cell_has_explicit_exit(cell: &CellDef) -> bool {
    if cell
        .body
        .iter()
        .any(|stmt| matches!(stmt, Stmt::Exit { .. } | Stmt::Die { .. }))
    {
        return true;
    }
    // **无条件的 plain jump 同样是终结语句**（§6.1：本实例转移给目标后即终止）。
    // 修前只对 Exit/Die 认账，于是在 `jump fact go with(..);` 之后又补一行
    // `return Some(Ret_main {});` —— rustc 立刻报 `unreachable_code`
    // （P1-5 要求生成物零告警）。
    // 带 `when` 的 plain jump 不算终结（条件不成立时还要往下跑），兜底仍需要。
    matches!(
        cell.body.last(),
        Some(Stmt::Jump(j)) if j.kind == JumpKind::Plain && j.condition.is_none()
    )
}

/// 把 `extern rust { ... }` 的外层 wrapper 剥掉，只保留花括号内的 Rust 代码。
///
/// 顶层 `Item::GlobalRust` 与 cell 内 `Stmt::ExternRustBlock` 的 `RawRust.text` 都包含
/// `extern rust { ... }` 整段；Rust 没有 `extern rust` 关键字（它是 rscel DSL 的），
/// 所以 emit 时必须剥外层。
fn strip_extern_rust(text: &str) -> String {
    let t = text.trim();
    // 匹配 "extern rust [read] {" 前缀
    let after_prefix = if let Some(idx) = t.find('{') {
        let prefix = &t[..idx];
        // 验证 prefix 是 `extern rust [read]`（可带尾随空白）
        let p = prefix.trim();
        if p == "extern rust" || p == "extern rust read" {
            &t[idx + 1..]
        } else {
            // 不是 extern rust 包裹——按原样返回（兜底）
            return t.to_string();
        }
    } else {
        return t.to_string();
    };
    // 找匹配的 `}`（最外层花括号；保守找最后一个 `}`）
    // 因为 text 是逐字切片，最外层 `}` 通常是最后一个字符之前
    let trimmed = after_prefix.trim_end().trim_end_matches('}').trim_end();
    trimmed.to_string()
}

/// `alias`（receive 别名或键名）在本 Cell 体内是否被引用。
///
/// 用于决定生成 `let v_<id>` 还是 `let _v_<id>`（零告警要求，见 [`emit_cell`]）。
/// 判定保守：只要任何透传文本 / 实参 / 条件 / return 别名里出现同名**词**就算已使用
/// —— 宁可少加下划线（可能留一条 rustc 告警），也不要把还在用的绑定改错名。
fn alias_is_used(cell: &CellDef, alias: &str, has_jump_loop: bool) -> bool {
    // `jump loop` 按**位置**刷新每个 receive 绑定（`v_<id> = <expr>;`），
    // 赋值语句里不写别名名，但绑定确实被读写 → 一律算已使用。
    if has_jump_loop {
        return true;
    }
    let mut texts = Vec::new();
    for stmt in &cell.body {
        texts.extend(stmt_rust_texts(stmt));
    }
    texts.iter().any(|t| contains_word(t, alias))
}

/// 一条 DSL 语句里所有**透传的 Rust 文本**（RawRust / out / exit / extern rust /
/// jump 的实参与条件 / Rust 块的全部内容）。
///
/// 两个调用方都靠它，而且都取保守方向：
/// [`alias_is_used`] 用它做词匹配，[`expansion_shape_ok`] 用它扫 `return` / `?`
/// （展开后这两个都会把「从本 Cell 返回」变成「从调用方返回」）。
fn stmt_rust_texts(stmt: &Stmt) -> Vec<String> {
    match stmt {
        Stmt::RawRust(r) => vec![r.text.clone()],
        Stmt::Exit { args, .. } => args.iter().map(|a| a.expr.text.clone()).collect(),
        Stmt::Out { args, .. } => args.iter().map(|a| a.text.clone()).collect(),
        Stmt::ExternRustBlock { body, .. } => vec![body.text.clone()],
        Stmt::RustBlock { block } => block_rust_texts(block),
        Stmt::Jump(j) => {
            let mut out: Vec<String> = Vec::new();
            out.extend(j.args.iter().map(|a| a.expr.text.clone()));
            if let Some(c) = &j.condition {
                out.push(c.text.clone());
            }
            if let Some(l) = &j.concurrent_limit {
                out.push(l.text.clone());
            }
            if let Some(o) = &j.over {
                out.push(o.collection.text.clone());
                for op in &o.ops {
                    let t = match op {
                        rscel_parser::OverOp::Filter(c)
                        | rscel_parser::OverOp::Map(c)
                        | rscel_parser::OverOp::FlatMap(c)
                        | rscel_parser::OverOp::Take(c)
                        | rscel_parser::OverOp::Skip(c) => &c.text,
                    };
                    out.push(t.clone());
                }
            }
            // return(k -> 别名) 的别名也可能是 receive 别名本身（用户就地复用）
            out.extend(j.ret_map.iter().map(|(_, a)| a.clone()));
            out
        }
        Stmt::Sync { .. } | Stmt::Die { .. } => Vec::new(),
    }
}

/// 递归收集 [`Stmt::RustBlock`] 内的全部透传文本（嵌套 DSL 语句一并展开）。
fn block_rust_texts(block: &rscel_parser::RawBlock) -> Vec<String> {
    use rscel_parser::RawBlockPart;
    let mut out = Vec::new();
    for p in &block.parts {
        match p {
            RawBlockPart::Text(t) => out.push(t.text.clone()),
            RawBlockPart::Stmt(s) => out.extend(stmt_rust_texts(s)),
        }
    }
    out
}

/// 词边界匹配：把文本按标识符字符切开，任一 token 与 `word` 全等即为已引用。
fn contains_word(text: &str, word: &str) -> bool {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|tok| tok == word)
}

/// 收集 jump loop 实参键集中涉及的 let 绑定名（这些 let 在 jump loop emit 时会被
/// "外声明 + 内重算"，RawRust 流程不应再重复 emit 它们的 `let X = ...;` 行）。
fn jump_loop_consumed_let_names(cell: &CellDef) -> Vec<String> {
    let mut out = Vec::new();
    for stmt in &cell.body {
        if let Stmt::Jump(j) = &stmt {
            if j.kind == JumpKind::Loop {
                for a in &j.args {
                    let k = a.effective_key();
                    if !k.is_empty() && !out.contains(&k) {
                        out.push(k);
                    }
                }
            }
        }
    }
    out
}

// -------------------------------------------------- 候选 1：热 Cell 真实体展开
//
// 数据依据 `docs/06-控制变量实验.md`（8 格控制变量矩阵 + 4 个展开探针，同一时间窗交错）：
//   · 循环在独立 `fn` 里 → 20.6ms；加 `#[inline(always)]` → 12.9ms；
//     **真实生成物 → 12.6ms**；同一段循环写在调用方作用域（手写基线）→ 6.3ms。
//   · `--emit=asm` 对照：手写基线 7 条 `padd`（向量化），生成物与 `#[inline(always)]`
//     变体 **0 条** —— LLVM 已经把 Cell 函数内联进调用方，但向量化器没有接手。
//   · 探针 EXP5（块表达式展开）6.40ms = 手写基线的 99.1%；
//     探针 EXP4（立即调用的闭包包装）20.83ms **无效** —— 包一层闭包等于再造函数边界。
// 结论：有效的只有「把 body 逐字抄进调用点、且不留任何函数/闭包边界」。

/// 展开的体积预算：一个 Cell 的**全部** plain 调用点抄完之后，总共发射的 DSL 语句条数上限。
///
/// 为什么需要它而不是「只允许一个调用点」：展开是把 body 逐字抄进调用点，N 个调用点抄 N 份；
/// `docs/06` 实验七实测「多一个调用点 → 退回 `fn cell_x(..)`」这一格值 **2.00 倍**
/// （GEN_two / GEN_real 两窗 2.006x / 1.998x，复现带 ≤0.8%），所以多调用点必须一起展开。
/// 但生成物是一个要给人读的 `.rs`（终极目标 2 的编辑器就挂在它上面），因此按抄完后的
/// 总语句数封顶，档位沿用 48cbb79 的「小 body 才 force-inline」（单调用点时等价于 ≤12 条）。
const MAX_EXPANDED_STATEMENTS: usize = 24;

/// 判定哪些 Cell 应当在**它的每一个 plain 调用点**被实体展开，返回目标 Cell 名集合。
///
/// 每一条闸门都是**保守**的：不满足就退回今天的 `fn cell_x(..)` 调用形状，
/// 语义一字不变，只是那一处拿不到优化。
fn inline_expansion_targets(
    program: &Program,
    async_cells: &std::collections::HashSet<String>,
    env: &TypeEnv,
) -> std::collections::HashSet<String> {
    // entry Cell 由 `fn main` 直接调用（见 [`emit_main`]），那条调用点不在这里的统计里。
    // 若不排除，entry 会被当成「0 个 plain 调用点」而永远不展开（正确但绕），
    // 更糟的是它一旦被展开，`fn main` 仍会去调用那个已经不存在的 `fn cell_main`。
    let entry = program
        .entry
        .clone()
        .or_else(|| program.cells().find(|c| c.name == "main").map(|c| c.name.clone()));

    // 只有 plain jump 与 recurse / and 会**发射对 `cell_x(..)` 的调用**；
    // `jump loop` 是原地循环（§12.4），不产生调用，所以不计入调用点。
    //
    // plain 的表值 = **每个调用点的实参数**（长度 = 调用点个数）。只统计
    // `targets.first()`：plain jump 是转移式出口，[`emit_jump`] 只对第一个目标发射调用，
    // 后面的目标属于 P3 记的「多目标语义未定义」。把它们也算成调用点会让那个 Cell
    // 「被判成可展开」，于是既不发射 `fn cell_x(..)`、又没有展开体落点 ——
    // **静默丢代码**（2026-10-11 实测：`jump a, b` 的生成物里没有 `fn cell_b`，
    // 只剩一个没人引用的 `struct RetB {}`，`cell b` 的 `out` 从未执行）。
    let mut plain_sites: std::collections::HashMap<String, Vec<usize>> =
        std::collections::HashMap::new();
    let mut called_otherwise: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut self_jumped: std::collections::HashSet<String> = std::collections::HashSet::new();
    for cell in program.cells() {
        for stmt in &cell.body {
            let Stmt::Jump(j) = stmt else { continue };
            match j.kind {
                JumpKind::Plain => {
                    let Some(t) = j.targets.first() else { continue };
                    plain_sites.entry(t.clone()).or_default().push(j.args.len());
                    if t == &cell.name {
                        self_jumped.insert(t.clone());
                    }
                }
                JumpKind::Recurse | JumpKind::And => {
                    for t in &j.targets {
                        called_otherwise.insert(t.clone());
                    }
                }
                JumpKind::Loop => {}
            }
        }
    }

    let mut targets = std::collections::HashSet::new();
    for cell in program.cells() {
        if entry.as_deref() == Some(cell.name.as_str()) {
            continue;
        }
        // 一个 plain 调用点都没有 → 没有地方抄（recurse/and/loop 指向它也不算）。
        let Some(site_arities) = plain_sites.get(&cell.name) else {
            continue;
        };
        // 还有其他调用形态指向它 → 那些点仍需要 `fn cell_x(..)` 存在。
        if called_otherwise.contains(&cell.name) {
            continue;
        }
        // 自跳（`cell a` 里 `jump a`）：展开会生成一个引用自身的块，今天的形状是
        // 无限递归调用，两者都不该被优化改变 → 保持原样。
        if self_jumped.contains(&cell.name) {
            continue;
        }
        // async Cell 不能展开：块表达式不是 async 上下文，`.await` 会直接编译失败
        // （而且 S2/批次 A 的前提就是「只有透传出现 .await 的 Cell 才是 async fn」）。
        if async_cells.contains(&cell.name) {
            continue;
        }
        // 体积预算（每个调用点抄一份 body）。
        if cell.body.len() * site_arities.len() > MAX_EXPANDED_STATEMENTS {
            continue;
        }
        if !expansion_shape_ok(cell) {
            continue;
        }
        let inputs = collect_inputs(cell, env);
        // **每个**调用点的实参数都要等于形参数：展开靠「按位置逐个发射 `let 形参 = 实参;`」，
        // 少一个会在取实参时越界，多一个则静默丢掉那个表达式的副作用
        // （旧实现只看了**第一个**调用点的 arity，多调用点时这个假设根本不存在）。
        // arity 本应由 check 层的 E042 把关，但生成器不假设调用方先跑过 check。
        if !site_arities
            .iter()
            .all(|n| *n == inputs.typed_params.len())
        {
            continue;
        }
        targets.insert(cell.name.clone());
    }
    targets
}

/// 展开的形状闸门：body 必须以 `exit go with` 收尾，且除它以外不含任何
/// 会发射 `return` / `?` 的形态。
///
/// 为什么这两个字符是致命的：展开后 body 住在**调用方**的函数体里，
/// `return` / `?` 会从调用方返回，跳过调用方剩下的语句并换掉返回类型 ——
/// 那是语义改变，不是优化。
///
/// 「多调用点抄多份」不需要改名，这一点值得写下来（`docs/05` P0-5 第 2 项登记时把它
/// 记成了硬前提）：每份展开体都住在调用点自己的**就地块**里
/// （[`emit_inlined_block`] 发的 `{ let _ = { .. }; return ..; }`），被调方的 `v_<id>`、
/// 形参别名、`let` 绑定全部只在那个块内可见，因此第 2、3、N 份只是嵌套作用域里的
/// **遮蔽**，不是同名冲突；调用方在 jump 之后没有语句会执行（转移式出口），
/// 所以被遮蔽的那一侧永远读不到。真正的约束只剩体积预算与逐点 arity。
fn expansion_shape_ok(cell: &CellDef) -> bool {
    // 与 48cbb79 的内联启发同档：只展开小 body，避免生成物体积失控（一个 .rs 要能读）。
    if cell.body.is_empty() || cell.body.len() > 12 {
        return false;
    }
    // 展开块的值 = 被调方的 `Option<Ret_x>`，所以必须以 Exit 收尾（块尾值是表达式）。
    if !matches!(cell.body.last(), Some(Stmt::Exit { .. })) {
        return false;
    }
    for stmt in &cell.body {
        match stmt {
            // plain jump 是转移式出口，recurse/and 也会发终结性 `return`：
            // 展开后那些 `return` 归属调用方 → 不展开。
            Stmt::Jump(j) if j.kind != JumpKind::Loop => return false,
            // die 发 `return None;`。
            Stmt::Die { .. } => return false,
            // 用户 Rust 块可以携带任意控制流（`return` / `break` / 嵌套 item），
            // 逐条判定不划算，整个形态不参与展开。
            Stmt::RustBlock { .. } => return false,
            _ => {
                for text in stmt_rust_texts(stmt) {
                    if contains_word(&text, "return") || text.contains('?') {
                        return false;
                    }
                }
            }
        }
    }
    true
}

/// 备齐一个被展开 Cell 在调用点需要的素材（形状判定已在计划阶段做过）。
///
/// `inputs` 按值传入：`collect_inputs` 要 `TypeEnv`，而 `TypeEnv` 只在 `generate` 里可得。
fn prepare_inline_body(cell: &CellDef, inputs: CellInputs) -> InlineBody {
    let has_jump_loop = cell_has_jump_loop(cell);
    let unused = unused_input_flags(cell, &inputs, has_jump_loop);
    let params: Vec<(String, String)> = inputs
        .typed_params
        .iter()
        .enumerate()
        .map(|(i, (name, ty))| {
            (if unused[i] { format!("_{name}") } else { name.clone() }, ty.clone())
        })
        .collect();
    InlineBody {
        params,
        suppressed_lets: jump_loop_consumed_let_names(cell),
        has_jump_loop,
        unused,
        inputs,
    }
}

/// Cell 是否有 `jump loop`（它按位置重新赋值 receive 别名，绑定需要 `mut`）。
fn cell_has_jump_loop(cell: &CellDef) -> bool {
    cell.body.iter().any(|s| matches!(s, Stmt::Jump(j) if j.kind == JumpKind::Loop))
}

/// 未被 body 引用到的 receive 别名 → `true`（生成物要加 `_` 前缀才零告警）。
fn unused_input_flags(cell: &CellDef, inputs: &CellInputs, has_jump_loop: bool) -> Vec<bool> {
    inputs
        .typed_params
        .iter()
        .map(|(name, _)| !alias_is_used(cell, name, has_jump_loop))
        .collect()
}

/// receive 形参 → `v_<id>` 绑定块（D1：Cell 环境就是普通局部变量，不生成 Env 结构体）。
///
/// 从 [`emit_cell`] 抽出来，让「独立函数发射」与「展开进调用点」两条路径**逐字同形**：
/// 展开的意义只是换掉作用域，不允许同时改变语句形状。
///
/// 多包 receive 键（`g1.W1.A`）**不**在此绑定：按 S4 late binding，它的值要到
/// `jump sync` 之后才由组结果解包（见 [`emit_sync`]）；在入口绑一个占位值既没意义，
/// 又会与解包产生 shadow 告警。
fn emit_receive_bindings(
    out: &mut String,
    cell: &CellDef,
    inputs: &CellInputs,
    has_jump_loop: bool,
    unused: &[bool],
) {
    for (i, (name, ty)) in inputs.typed_params.iter().enumerate() {
        if cell.receive.keys.get(i).is_some_and(|k| k.pkg.is_some()) {
            continue;
        }
        let vid = format!("v_{}", var_id(i));
        let (vid_out, name_in) = if unused[i] {
            (format!("_{vid}"), format!("_{name}"))
        } else {
            (vid.clone(), name.clone())
        };
        // 只有 `jump loop` 会重新赋值 receive 别名（§9.2 按位置刷新），
        // 其余场景 `mut` 是多余的，会产生 unused_mut 告警。
        if has_jump_loop {
            writeln!(out, "    let mut {vid_out}: {ty} = {name_in};").unwrap();
        } else {
            writeln!(out, "    let {vid_out}: {ty} = {name_in};").unwrap();
        }
    }
}

/// 把展开体拼成调用点的一条语句：
/// `{ let _ = { 形参绑定; 被调 receive 绑定; 被调 body（尾 Exit 作块值） }; return Some(Ret_self {}); }`
///
/// `ind` 是该语句在调用方里的缩进（plain jump 在 body 层是 4 空格；`when` 分支里是 8）。
/// 外层保留 `{ let _ = ..; return Some(..); }` 的**转移式**形状（§6.3：plain jump 终结
/// 本实例），与今天的 `fn cell_x(..)` 调用点只差「调用」换成「就地块」。
///
/// **为什么被调方语句要直接发进 `out`**（而不是先生成文本再抄过来）：§12.9 的映射区间
/// 是按全文件行号记录的，抄进文件的段落行号会变，被展开 Cell 的每条语句就查不到自己的
/// 落点了。直接发射 + 事后统一加缩进（[`reindent_from`] 只改列不改行）两边都保住。
#[allow(clippy::too_many_arguments)]
fn emit_inlined_block(
    out: &mut String,
    map: &mut SrcMapBuilder,
    callee: &CellDef,
    ib: &InlineBody,
    args: &[String],
    ind: &str,
    cell_ret_name: &str,
    ctx: &mut Ctx,
) {
    writeln!(out, "{ind}{{ let _ = {{").unwrap();
    for (i, (pname, pty)) in ib.params.iter().enumerate() {
        writeln!(out, "{ind}    let {pname}: {pty} = {};", args[i]).unwrap();
    }
    // 展开体自己的行区间起点：receive 绑定块（绑定为空时就是第一条语句）。
    let mark = out.len();
    let lo = next_line(out);
    emit_receive_bindings(out, callee, &ib.inputs, ib.has_jump_loop, &ib.unused);
    // §15.3 埋点：被展开的 Cell 同样是**一个实例**，spawn/recv 一行都不能少。
    // 独立函数路径上这行由 [`emit_cell`] 发；展开路径若不补，追踪流里就会凭空少掉
    // 一个实例（而展开恰恰是热 Cell 的默认形状）。
    // 位置取被调 Cell 的定义行，与 [`emit_cell`] 的 spawn 行同源。
    if ctx.trace {
        emit_trace(
            out,
            "    ",
            &format!(
                "cell {} spawn{} recv={}",
                callee.name,
                trace_pos(callee.span),
                trace_recv_keys(callee)
            ),
            &[],
        );
    }
    // 被调方的 receive 绑定块单独记一条：与它作为独立函数发射时的记录点同源
    // （类型不匹配指向 `receive(...)` 子句比指向 `cell x;` 有用）。
    map.push(
        MapKind::CellHead,
        DslPos::item(&callee.name, callee.receive.span),
        false,
        lo,
        written_lines(out),
    );
    // 被调方用自己的 Ctx：`promoted` 是「同一变量只做一次 Arc::new 提升」的每 Cell 状态，
    // 必须与它作为独立函数发射时一致，否则会少一次提升 → 语义漂移。
    // 展开体里不会再套别人的展开（嵌套已被「body 内不得有非 loop 的 jump」闸门排除），
    // 所以计划表给空表。先把两个共享引用复制出来，好让 `sub_ctx` 与 `&mut ctx` 共存。
    let program = ctx.program;
    let async_cells = ctx.async_cells;
    let trace = ctx.trace;
    let no_expansions: std::collections::HashMap<String, InlineBody> =
        std::collections::HashMap::new();
    let mut sub_ctx = Ctx::new(program, async_cells, &no_expansions, trace);
    sub_ctx.exit_as_tail = true;
    for (idx, stmt) in callee.body.iter().enumerate() {
        let stmt_start = next_line(out);
        // is_async 恒 false：async 目标已在计划阶段排除（块表达式不是 async 上下文）。
        emit_stmt(
            out,
            stmt,
            callee,
            &ib.inputs,
            false,
            &mut sub_ctx,
            &ib.suppressed_lets,
            idx,
            map,
        );
        map.push(
            MapKind::Stmt,
            DslPos::stmt(&callee.name, idx, span_of_stmt(stmt)),
            extern_flag_for_stmt(stmt),
            stmt_start,
            written_lines(out),
        );
    }
    let hi = written_lines(out);
    reindent_from(out, mark, ind);
    writeln!(out, "{ind}}}; return Some({cell_ret_name} {{}}); }}").unwrap();
    // 告诉调用方：`lo..=hi` 这些行已由被调方自己的条目接管，外层只登记头尾剩下的两截
    // （见 [`Ctx::inline_cover`]）。
    ctx.inline_cover = Some((lo, hi));
}

/// 给 `out[mark..]` 的每一行加 `ind` 缩进。
///
/// 被调方的语句都是按「Cell body = 4 空格」发射的，加上调用点自己的 `ind` 才是最终
/// 层级（plain jump 在 body 层 → 4+4=8；`when` 分支里 → 8+4=12）。
/// 只改列不改行数，所以已经推进 `map` 的区间不受影响（映射按行号查）。
/// 用 `lines()` + 逐行补 `\n`：该段总是由 `writeln!` 写出的（以 `\n` 收尾），
/// 行数因此逐字保持。（第一版这里误用了 `split_once` —— 它返回的是**不含分隔符**的
/// 后半段，于是每行的 `\n` 都被吞掉，整段被压成一行。）
fn reindent_from(out: &mut String, mark: usize, ind: &str) {
    let block = out[mark..].to_string();
    let mut re = String::with_capacity(block.len() + block.lines().count() * (ind.len() + 1));
    for line in block.lines() {
        if !line.trim().is_empty() {
            re.push_str(ind);
        }
        re.push_str(line);
        re.push('\n');
    }
    out.truncate(mark);
    out.push_str(&re);
}

/// 记录点说明（§12.9 映射，**只数行数、不改生成逻辑**）：
/// - Cell 函数头：`fn cell_<name>(..) -> Option<Ret<Name>> {` 一整行；
/// - receive 形参绑定块（`let v_a: i64 = cur;`）：位置取 `receive(...)` 子句的 span；
/// - `cell.body` 的每一条 DSL 语句：`emit_stmt` 前后各数一次行数，得到该语句展开出的
///   行区间；`in_extern` 由 [`srcmap::extern_flag_for_stmt`] 判定（§15.2 的 RSC-E901 判据）。
///
/// 兜底 `return Some(Ret::<Name> {});` 与收尾 `}` 不进映射：它们是 codegen 自己补的
/// 控制流垫块，没有对应的 DSL 语句，落在其中的诊断走「未映射」分支原样输出。
fn emit_cell(
    out: &mut String,
    cell: &CellDef,
    inputs: &CellInputs,
    outs: &CellOutputs,
    ctx: &mut Ctx,
    map: &mut SrcMapBuilder,
) {
    let fname = cell_fn_name(&cell.name);
    let ret = ret_struct_name(&cell.name);
    let is_async = ctx.is_async(&cell.name);
    let _ = outs;
    // `jump loop` 会按位置重新赋值 receive 别名（§9.2），因此形参绑定需要 `mut`。
    let has_jump_loop = cell_has_jump_loop(cell);

    let head_start = next_line(out);
    // §12.8 / §17.1「同步相邻内联」：同步 Cell 生成 `#[inline(always)]`（小 body）
    // 或 `#[inline]`（其余），异步 Cell 不加。
    //
    // 为什么值得动生成器：`bash scripts/bench/run.sh loop_sum` 实测，同样的循环体
    // 放在 Cell 函数里跑 43ms，而手写把它直接写在 `main` 里跑 11ms —— 差的不是
    // DSL 语义，是 LLVM 的循环向量化决策**看函数作用域**：循环在被单独发射出去的
    // 函数里时向量化器不介入。给 Cell 函数加内联提示后，循环体回到调用方作用域，
    // 实测降到 24–32ms（同一台机器同一负载下交错测量）。剩下的差距记在
    // `docs/05-迭代待办.md` P0-1，不假装已经达标。
    //
    // 阈值取"body 语句数 ≤ 12 且不含 jump and"：小任务才 force-inline，
    // 大 Cell 只给 `#[inline]` 提示，避免生成物体积失控（一个 .rs 文件要能读）。
    // 自递归 Cell 也安全：LLVM 对递归调用点自然放弃内联，不会无限展开。
    if is_async {
        write!(out, "async fn {fname}(").unwrap();
    } else {
        let small = cell.body.len() <= 12
            && !cell
                .body
                .iter()
                .any(|s| matches!(s, Stmt::Jump(j) if j.kind == JumpKind::And));
        if small {
            writeln!(out, "#[inline(always)]").unwrap();
        } else {
            writeln!(out, "#[inline]").unwrap();
        }
        write!(out, "fn {fname}(").unwrap();
    }
    // 未被引用的 receive 别名：形参与 `v_<id>` 绑定都加 `_` 前缀。
    //
    // 生成物必须零告警（P1-5），而"收到但不用"是合法业务写法
    // （`cell ack; receive(id: i64); exit go with();` —— 只确认到达）。
    // 与其在文件级 `allow(unused_variables)` 把**用户真正的死变量**一起吞掉，
    // 不如精确地只给没用到的绑定加下划线。
    let unused = unused_input_flags(cell, inputs, has_jump_loop);
    for (i, (name, ty)) in inputs.typed_params.iter().enumerate() {
        if i > 0 { write!(out, ", ").unwrap(); }
        // 多包 receive 键（`g1.W1.A`）的值在 `jump sync` 之后才解包（见 `emit_sync`），
        // 形参本身永远不会被读；加 `_` 前缀避免无意义的 unused 告警。
        if cell.receive.keys.get(i).is_some_and(|k| k.pkg.is_some()) || unused[i] {
            write!(out, "_{name}: {ty}").unwrap();
        } else {
            write!(out, "{name}: {ty}").unwrap();
        }
    }
    writeln!(out, ") -> Option<{ret}> {{").unwrap();
    map.push(
        MapKind::CellHead,
        DslPos::item(&cell.name, cell.span),
        false,
        head_start,
        written_lines(out),
    );

    // 接收参数 → v_<id>（与展开路径共用 [`emit_receive_bindings`]，两条路径逐字同形）
    let bind_start = next_line(out);
    emit_receive_bindings(out, cell, inputs, has_jump_loop, &unused);
    // §15.3：实例创建 + 收到 Packet（键名列表）。发在 receive 绑定块**之内**，
    // 这样这一行的 rustc 诊断（比如键名文本里的格式化错）反映射到该 Cell 的
    // `receive(...)` 子句，而不是掉进未映射区间。
    if ctx.trace {
        emit_trace(
            out,
            "    ",
            &format!(
                "cell {} spawn{} recv={}",
                cell.name,
                trace_pos(cell.span),
                trace_recv_keys(cell)
            ),
            &[],
        );
    }
    // receive 绑定块（`let v_a: i64 = cur;`）：这一段完全由 `receive(...)` 子句生成，
    // 类型不匹配的报错指向 receive 子句比指向 `cell x;` 有用得多。
    // 没有任何绑定时（无 receive / 全是多包键）区间为空，`push` 会自己跳过。
    map.push(
        MapKind::CellHead,
        DslPos::item(&cell.name, cell.receive.span),
        false,
        bind_start,
        written_lines(out),
    );

    // jump loop 接管了某些 let 的声明+重算，RawRust emit 时不再重复输出它们
    let suppressed_lets = jump_loop_consumed_let_names(cell);

    // 主体：一条 DSL 语句 ↔ 一段生成行区间（§12.9 的映射粒度，见 srcmap 模块注释）
    for (idx, stmt) in cell.body.iter().enumerate() {
        let stmt_start = next_line(out);
        ctx.inline_cover = None;
        emit_stmt(out, stmt, cell, inputs, is_async, ctx, &suppressed_lets, idx, map);
        let stmt_end = written_lines(out);
        let ext = extern_flag_for_stmt(stmt);
        // `DslPos` 含 String，不是 Copy，所以两处 push 各算一次（`stmt` 是引用，代价可忽略）。
        let dsl = || DslPos::stmt(&cell.name, idx, span_of_stmt(stmt));
        // 候选 1：这条语句里发生了实体展开 → 展开体的行归被调方自己的条目。调用方只登记
        // 块头（`{ let _ = {` + 形参绑定）和收尾行两截，整张表因此仍然升序且互不重叠
        // （`srcmap_test::entries_are_sorted_non_overlapping_and_cover_cell_bodies` 验的
        // 就是这条不变式，不能为了展开把它放宽）。
        match ctx.inline_cover {
            None => map.push(MapKind::Stmt, dsl(), ext, stmt_start, stmt_end),
            Some((lo, hi)) => {
                if stmt_start < lo {
                    map.push(MapKind::Stmt, dsl(), ext, stmt_start, lo - 1);
                }
                if hi < stmt_end {
                    map.push(MapKind::Stmt, dsl(), ext, hi + 1, stmt_end);
                }
            }
        }
    }

    // 兜底退出：若 body 没有 exit/die/plain jump/recurse/and 等"终结语句"，
    // 补一个 `return Some(Ret_<name> {})`。注意：jump loop / plain jump 已经终结控制流，
    // 但 Rust 不允许跨 `loop` 的"自动 fallthrough"，所以我们仍需在每个 Cell 末尾补一个
    // 兜底的 `return Some(...)`；rustc 会给出 unreachable_code 警告（已知限制）。
    let _ = outs;
    if !cell_has_explicit_exit(cell) {
        // §15.3 的「实例销毁」在这一格**必须**补一行：`jump and ... group g;` 收尾的 body
        // 是**合法且可运行到**的（fire-and-forget，check 的 E010 认 `and` 为终结、
        // 而 `cell_has_explicit_exit` 只认 exit/die/无条件 plain jump），不补就是
        // 「这个实例凭空消失」；`jump loop` 收尾的那一格到不了这里，多一行不执行的
        // 埋点没有代价。位置取 `cell <名>;` 的定义行 —— 兜底 return 不对应任何 DSL 语句，
        // 所以它也不进 §12.9 映射（与它下面那行 `return` 同一待遇）。
        if ctx.trace {
            emit_trace(
                out,
                "    ",
                &format!(
                    "cell {} implicit exit{} send=[]",
                    cell.name,
                    trace_pos(cell.span)
                ),
                &[],
            );
        }
        writeln!(out, "    return Some({ret} {{}});").unwrap();
    }

    writeln!(out, "}}").unwrap();
    out.push('\n');
}

///
/// **为什么 `#[allow(clippy::too_many_arguments)]`**：代码生成器的每个 emit_* 都必然同时携带
/// 「输出缓冲 + 当前语句 + Cell 级上下文（inputs/async 位/别名重写表）+ 本形态专属参数」。
/// 把它们打包成一个结构体只会把同一组字段换一种写法传递（字段仍要逐个可变），
/// 可读性反而下降；参数个数在这里是**形状的直接描述**，不是坏味道。
/// 仓库内同类先例：`rscel-parser/src/ast.rs` 的 `#[allow(clippy::large_enum_variant)]`（就地注释理由）。
#[allow(clippy::too_many_arguments)]
fn emit_stmt(
    out: &mut String,
    stmt: &Stmt,
    cell: &CellDef,
    inputs: &CellInputs,
    is_async: bool,
    ctx: &mut Ctx,
    suppressed_lets: &[String],
    stmt_idx: usize,
    map: &mut SrcMapBuilder,
) {
    let _ = stmt_idx;
    match stmt {
        Stmt::RawRust(r) => {
            let rewritten = rewrite(&r.text, &inputs.alias_to_var);
            for line in rewritten.lines() {
                let t = line.trim();
                // 跳过被 jump loop 接管的 `let X = ...;`
                if let Some(rest) = t.strip_prefix("let ") {
                    let ident: String = rest
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .collect();
                    if !ident.is_empty() && suppressed_lets.contains(&ident) {
                        continue;
                    }
                }
                writeln!(out, "    {line}").unwrap();
            }
        }
        Stmt::Exit { args, span } => {
            // exit 实参是 RawRust 透传，必须按 §10.3 重写 receive 别名——
            // 与 RawRust / `out(...)` / jump 实参走同一套 rewrite 合约。
            //
            // 展开态（[`Ctx::exit_as_tail`]）发成块尾值 `Some(Ret_x { .. })`，
            // 不带 `return`、不带 `;`：这条 Exit 是展开体的最后一条语句
            // （[`expansion_shape_ok`] 要求 body 必须以 Exit 收尾），所以它正好是
            // 块表达式的值。留着 `return` 会从**调用方**返回，那是改语义不是优化。
            if ctx.trace {
                // §15.3「Packet 收发」的发送侧 + 实例销毁：`exit go with` 是本实例的
                // 收敛点，键名列表就是这次交回的 Packet 有哪些字段。
                emit_trace(
                    out,
                    "    ",
                    &format!(
                        "cell {} exit{} send={}",
                        cell.name,
                        trace_pos(*span),
                        trace_send_keys(args)
                    ),
                    &[],
                );
            }
            let head = if ctx.exit_as_tail { "" } else { "return " };
            write!(out, "    {head}Some(").unwrap();
            write!(out, "{} {{ ", ret_struct_name(&cell.name)).unwrap();
            for (i, a) in args.iter().enumerate() {
                if i > 0 { write!(out, ", ").unwrap(); }
                let k = a.effective_key();
                let expr = rewrite(a.expr.text.trim(), &inputs.alias_to_var);
                if k.is_empty() {
                    write!(out, "_{i}: {expr}").unwrap();
                } else {
                    write!(out, "{k}: {expr}").unwrap();
                }
            }
            if ctx.exit_as_tail {
                writeln!(out, " }})").unwrap();
            } else {
                writeln!(out, " }});").unwrap();
            }
        }
        Stmt::Die { span } => {
            if ctx.trace {
                // §15.3 点名要 `die` 的触发**行号**：`return None` 本身不携带任何位置，
                // 而"哪个实例在哪一行死掉"正是拓扑调试最该看见的一件事。
                emit_trace(
                    out,
                    "    ",
                    &format!("cell {} die{}", cell.name, trace_pos(*span)),
                    &[],
                );
            }
            writeln!(out, "    return None;").unwrap();
        }
        Stmt::Out { args, .. } => {
            // out(...)：debug 输出（§12.1 / §15.3）。本批次 release 也保留 println!；
            // 后续 batch 接 --release 时改为条件输出或 eprintln!。
            //
            // out 的实参是 RawRust 表达式透传，必须按 §10.3 重写 receive 别名；
            // 这与 emit `let X = ...;` 走 RawRust 透传路径是同一个 rewrite 合约。
            let rewritten: Vec<String> = args
                .iter()
                .map(|a| rewrite(&a.text, &inputs.alias_to_var))
                .collect();
            if args.len() == 1 {
                writeln!(out, "    println!(\"{{}}\", {});", rewritten[0].trim()).unwrap();
            } else {
                write!(out, "    println!(\"").unwrap();
                for _ in 0..args.len() { write!(out, "{{}} ").unwrap(); }
                write!(out, "\"").unwrap();
                for r in &rewritten {
                    write!(out, ", {}", r.trim()).unwrap();
                }
                writeln!(out, ");").unwrap();
            }
        }
        Stmt::Jump(j) => {
            emit_jump(
                out, j, cell, inputs, is_async, &ret_struct_name(&cell.name), ctx, stmt_idx, map,
            )
        }
        Stmt::Sync { group, span } => emit_sync(out, group, cell, inputs, ctx, *span),
        Stmt::RustBlock { block } => {
            // P1-1 修复：补 `{}`、去掉 `trim()` + 丢空行。
            //
            // 两种 RustBlock 形态：
            //   1. **head+body**（如 `if cond { ... } else { ... }`）—— 首段是 Text，
            //      表示条件/连接文本（如 `if cond ` / ` else `）；后续 Stmt::RustBlock
            //      是 body。**不**在这里包 `{}`（花括号由 body 内的 RustBlock 自己加）。
            //   2. **body**（如 `{ die; }` / `{ fn helper() { ... } }`）—— 首段是
            //      Stmt；需要补 `{}`。
            //
            // 启发：`parts.first()` 是 `RawBlockPart::Text` ⇒ head+body，否则是 body。
            //
            // 缩进规则：cell body 一层 4 空格；block 内 8 空格；Text / 嵌套 Stmt 都加 4。
            let is_body = !matches!(block.parts.first(), Some(rscel_parser::RawBlockPart::Text(_)));
            if is_body {
                writeln!(out, "    {{").unwrap();
            }
            for part in &block.parts {
                match part {
                    rscel_parser::RawBlockPart::Text(t) => {
                        // §3.5.2 逐字还原文本结构（空行 + 原始缩进），但**标识符仍按 §10.3
                        // 重写**：块头如 `if cur > 0 ` 里的 `cur` 是 receive 别名，
                        // 不重写就编译不过（修前的缺陷）。
                        let rewritten = rewrite(&t.text, &inputs.alias_to_var);
                        for line in rewritten.split('\n') {
                            writeln!(out, "    {line}").unwrap();
                        }
                    }
                    rscel_parser::RawBlockPart::Stmt(s) => {
                        // 嵌套 DSL 走 emit_stmt 拿到的是 4 空格缩进；
                        // 这里再缩 4 空格到 8 空格。用临时 buffer 取出来再前缀。
                        let mut sub = String::new();
                        // 临时 buffer 的行号不是全文件行号，所以映射写进一张**用完就丢**的
                        // 累加器：嵌套块里的语句今天本来就没有映射条目，这里不能假装记上了
                        // （记了会把诊断指到别的语句上，比不指更糟）。
                        let mut scratch_map = SrcMapBuilder::new("");
                        // 嵌套块里的 jump：路径选择按"内联"处理（传 usize::MAX，
                        // 块内无法可靠判断紧邻的 jump sync），语义仍是正确的串行 await。
                        emit_stmt(&mut sub, s, cell, inputs, is_async, ctx, &[], usize::MAX, &mut scratch_map);
                        for line in sub.split('\n') {
                            // emit_stmt 的空行（结尾）跳过以免插冗余。
                            if line.is_empty() {
                                continue;
                            }
                            writeln!(out, "    {line}").unwrap();
                        }
                    }
                }
            }
            if is_body {
                writeln!(out, "    }}").unwrap();
            }
        }
        Stmt::ExternRustBlock { body, .. } => {
            // Cell 内 extern rust { ... }：剥外层 wrapper，块内代码按 §10.3 重写别名后插入。
            //
            // 修前原样插入 → receive 别名在块内**完全不可用**：
            // `extern rust { let y: i64 = rsc_slow(x).await; }` 里的 `x` 指向一个不存在的
            // 名字，同时别名形参反而被判 unused 变量（`-D warnings` 下直接编译失败）。
            // §9.3 禁止的是块内出现 DSL **关键字**，与别名重写是正交的。
            let inner = strip_extern_rust(&body.text);
            let rewritten = rewrite(&inner, &inputs.alias_to_var);
            for line in rewritten.lines() {
                writeln!(out, "    {line}").unwrap();
            }
        }
    }
}

// ---------------------------------------------------------------- 调用点生成

/// 目标 Cell 的 receive 键名 → 形参位置下标。
///
/// **修前的 bug**：`emit_args_by_position` 用的是 **caller** 的 `alias_to_pos`。
/// caller 与被调方同名键的顺序毫无关系，于是
/// `jump w go with(b -> y, a -> x)`（w 声明 `receive(x, y)`）会按书写顺序 emit
/// `cell_w(b, a)` —— 键名映射被忽略、两个同类型实参静默对调。
/// 现在按**被调方**的 receive 顺序排序，键名 → 位置才是契约的本意（§9.1）。
fn target_key_positions(program: &Program, target: &str) -> std::collections::HashMap<String, usize> {
    let mut m = std::collections::HashMap::new();
    if let Some(tc) = program.cell(target) {
        for (i, k) in tc.receive.keys.iter().enumerate() {
            m.insert(k.name.clone(), i);
        }
    }
    m
}

/// 本次 jump 里以 `read with` / `force read with` 传输的目标键集合。
fn read_with_keys(j: &JumpStmt) -> std::collections::HashSet<String> {
    let mut s = std::collections::HashSet::new();
    if matches!(j.mode, Some(TransMode::ReadWith) | Some(TransMode::ForceReadWith)) {
        for a in &j.args {
            let k = a.effective_key();
            if !k.is_empty() {
                s.insert(k);
            }
        }
    }
    s
}

/// 变量在 Rust 里是否已经是 `Arc<...>`（决定要不要做 §8.1 的隐式提升）。
///
/// 判定顺序：receive 别名看注解（D2：注解优先，且 check 层 E037 已保证 read with
/// 的**接收端**注解必须是 Arc）；`let` 变量看 `let x: Arc<..>` 注解，
/// 或右值形如 `Arc::new(..)` / `Arc::clone(..)`。都判不出来 → 视为非 Arc，提升。
fn var_is_arc(cell: &CellDef, inputs: &CellInputs, rust_name: &str) -> bool {
    // 1) receive 别名（已 rewrite 成 v_<id>）
    if let Some((_, ty)) = inputs
        .alias_to_var
        .iter()
        .zip(inputs.typed_params.iter())
        .find(|((_, vid), _)| vid == rust_name)
        .map(|(_, t)| t)
    {
        return starts_with_arc(ty);
    }
    // 2) `let` 绑定：看注解，其次看右值形态
    for stmt in &cell.body {
        let Stmt::RawRust(r) = stmt else { continue };
        for line in r.text.lines() {
            let t = line.trim();
            let Some(rest) = t.strip_prefix("let ").or_else(|| t.strip_prefix("let mut ")).or(t.strip_prefix("let ")) else { continue };
            let ident: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if ident != rust_name {
                continue;
            }
            let after = rest[ident.len()..].trim_start();
            if let Some(ty) = after.strip_prefix(':') {
                let ty_text = ty.split('=').next().unwrap_or("").trim();
                return starts_with_arc(ty_text);
            }
            let rhs = after.split('=').nth(1).unwrap_or("").trim();
            return rhs.starts_with("Arc::new") || rhs.starts_with("Arc::clone")
                || rhs.starts_with("std::sync::Arc::new");
        }
    }
    false
}

/// 类型文本是否 `Arc<...>`（允许任意空白与 `std::sync::Arc<...>`）。
fn starts_with_arc(ty: &str) -> bool {
    let t: String = ty.chars().filter(|c| !c.is_whitespace()).collect();
    t.starts_with("Arc<") || t.contains("::Arc<")
}

/// §8.1：`read with(x)` 生成 `Arc::clone(&x)`，第一次发送前把 `x` 提升为 `Arc`。
///
/// 产出 `key → 调用点实参表达式`（值为 `Arc::clone(&名字)`）。
/// - 简单标识符且已是 Arc：直接 clone，不提升；
/// - 简单标识符且非 Arc：`let 名字 = Arc::new(名字);` 提升一次（同一 Cell 内幂等），
///   此后该变量的所有 read with 发送都共享同一个控制块（引用计数 +1，原始数据只有一份）；
/// - 复杂表达式：先 `let __rsc_rwN = Arc::new(<expr>);` 落到临时量，再 clone——
///   这样同一条 jump 派生的多个实例仍然共享一份数据（否则每实例各拷一份，
///   与 §8.1「原始数据只存一份」不符）。
fn promote_read_with(
    out: &mut String,
    j: &JumpStmt,
    cell: &CellDef,
    inputs: &CellInputs,
    ctx: &mut Ctx,
    indent: &str,
) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let keys = read_with_keys(j);
    if keys.is_empty() {
        return map;
    }
    let mut seq = 0usize;
    for a in &j.args {
        let k = a.effective_key();
        if !keys.contains(&k) {
            continue;
        }
        let raw = a.expr.text.trim();
        let name = rewrite(raw, &inputs.alias_to_var);
        let is_ident = rscel_parser::is_plain_ident(raw);
        let target_var = if is_ident {
            name.clone()
        } else {
            let tmp = format!("__rsc_rw{seq}");
            seq += 1;
            writeln!(out, "{indent}let {tmp} = Arc::new({name});").unwrap();
            tmp
        };
        if is_ident && !ctx.promoted.contains(&target_var) && !var_is_arc(cell, inputs, &target_var) {
            writeln!(out, "{indent}let {target_var} = Arc::new({target_var});").unwrap();
        }
        ctx.promoted.insert(target_var.clone());
        map.insert(k, format!("Arc::clone(&{target_var})"));
    }
    map
}

/// 从 `from` 出发沿 jump 边是否回得到 `from`（自递归 / 互递归判定）。
///
/// async Cell 内联递归调用会产生无限大小的 future（rustc E0733），必须装箱；
/// 同步 Cell 不需要（`__rsc_block_on` / 直接调用天然打断类型递归）。
fn reaches(program: &Program, from: &str, to: &str) -> bool {
    let mut stack = vec![to.to_string()];
    let mut seen = std::collections::HashSet::new();
    while let Some(cur) = stack.pop() {
        if cur == from {
            return true;
        }
        if !seen.insert(cur.clone()) {
            continue;
        }
        if let Some(c) = program.cell(&cur) {
            for t in jump_targets_of(c) {
                stack.push(t);
            }
        }
    }
    false
}

/// 一个 Cell 里出现的全部 jump 目标（含嵌套块）。
fn jump_targets_of(cell: &CellDef) -> Vec<String> {
    let mut v = Vec::new();
    for stmt in &cell.body {
        if let Stmt::Jump(j) = stmt {
            v.extend(j.targets.iter().cloned());
        }
    }
    v
}

/// 生成一次 Cell 调用的 Rust 表达式（`cell_t(实参…)`，按需 `.await` / 装箱 / block_on）。
///
/// 形状由**被调方**决定（修前用 caller 的 async 位，是错的）：
/// - 被调方同步 → 直接调用（零 future、零分配，这是 S2 + §17.1 的主路径）；
/// - 被调方 async + caller async → `.await`；被调方在自身可达闭包里时先 `Box::pin`
///   （否则 E0733）；
/// - 被调方 async + caller 同步 → `__rsc_block_on(..)` 驱动（等价于串行等待，
///   正是 `jump recurse` 的内联语义）。
fn build_call(
    j: &JumpStmt,
    target: &str,
    cell: &CellDef,
    inputs: &CellInputs,
    ctx: &Ctx,
    rw: &std::collections::HashMap<String, String>,
) -> String {
    let callee = cell_fn_name(target);
    let raw = format!("{callee}({})", build_args(j, target, inputs, ctx, rw).join(", "));
    if !ctx.is_async(target) {
        return raw;
    }
    if ctx.is_async(&cell.name) {
        if reaches(ctx.program, &cell.name, target) {
            format!("(Box::pin({raw})).await")
        } else {
            format!("({raw}).await")
        }
    } else {
        format!("__rsc_block_on({raw})")
    }
}

/// 按被调方 receive 键的**位置**排好的实参表达式（已过 §10.3 的别名 rewrite）。
///
/// 从 [`build_call`] 拆出来是给实体展开用的：展开要按位置逐个发射
/// `let 形参: 类型 = <实参表达式>;`，必须与调用形式用同一套排序规则，
/// 否则 keyed / 无键实参混写时两条路径会给出不同的参数顺序。
fn build_args(
    j: &JumpStmt,
    target: &str,
    inputs: &CellInputs,
    ctx: &Ctx,
    rw: &std::collections::HashMap<String, String>,
) -> Vec<String> {
    let pos = target_key_positions(ctx.program, target);
    let mut items: Vec<(usize, String)> = j
        .args
        .iter()
        .map(|a| {
            let k = a.effective_key();
            let p = pos.get(&k).copied().unwrap_or(usize::MAX);
            let expr = match rw.get(&k) {
                Some(e) => e.clone(),
                None => rewrite(a.expr.text.trim(), &inputs.alias_to_var),
            };
            (p, expr)
        })
        .collect();
    items.sort_by_key(|(p, _)| *p);
    items.into_iter().map(|(_, e)| e).collect()
}

///
/// **为什么 `#[allow(clippy::too_many_arguments)]`**：代码生成器的每个 emit_* 都必然同时携带
/// 「输出缓冲 + 当前语句 + Cell 级上下文（inputs/async 位/别名重写表）+ 本形态专属参数」。
/// 把它们打包成一个结构体只会把同一组字段换一种写法传递（字段仍要逐个可变），
/// 可读性反而下降；参数个数在这里是**形状的直接描述**，不是坏味道。
/// 仓库内同类先例：`rscel-parser/src/ast.rs` 的 `#[allow(clippy::large_enum_variant)]`（就地注释理由）。
#[allow(clippy::too_many_arguments)]
fn emit_jump(
    out: &mut String,
    j: &JumpStmt,
    cell: &CellDef,
    inputs: &CellInputs,
    is_async: bool,
    cell_ret_name: &str,
    ctx: &mut Ctx,
    stmt_idx: usize,
    map: &mut SrcMapBuilder,
) {
    let _ = is_async;
    match j.kind {
        JumpKind::Plain => {
            // plain jump 是**转移式出口**：本实例终止，目标 Cell 独立运行并以自己的
            // exit 收敛，返回值无人接收（return/group/over/concurrent_limit 挂在它上面
            // 由 check 层 E046 拒绝，不再静默丢弃）。
            let target = j.targets.first().expect("plain jump needs a target");
            let rw = promote_read_with(out, j, cell, inputs, ctx, "    ");
            // 候选 1「热 Cell 真实体展开」：目标在计划表里 → 把 body 就地展开成块表达式，
            // 不再留 `fn cell_x(..)` 这条函数边界（实测值 2 倍，见 docs/06）。
            // `bodies` / `program` 是把 `&'a _` 从 `ctx` 里**复制**出来（这两个字段本身
            // 就是共享引用），这样 `ib` / `callee` 不再按不可变借用锁住 ctx，
            // 下面的 `emit_inlined_block(.., ctx)` 才能拿到 `&mut Ctx` 记 `inline_cover`。
            let bodies = ctx.inline_bodies;
            let program = ctx.program;
            if let Some(ib) = bodies.get(target.as_str()) {
                if let Some(callee) = program.cell(target) {
                    let args = build_args(j, target, inputs, ctx, &rw);
                    // `when` 分支里语句在 `if {` 之内，缩进多一层；其余同 body 层。
                    let ind = if j.condition.is_some() { "        " } else { "    " };
                    if let Some(cond) = &j.condition {
                        let cond = rewrite(&cond.text, &inputs.alias_to_var);
                        writeln!(out, "    if {cond} {{").unwrap();
                        trace_send(ctx, out, ind, &cell.name, "jump", target, j);
                        emit_inlined_block(out, map, callee, ib, &args, ind, cell_ret_name, ctx);
                        writeln!(out, "    }}").unwrap();
                    } else {
                        trace_send(ctx, out, ind, &cell.name, "jump", target, j);
                        emit_inlined_block(out, map, callee, ib, &args, ind, cell_ret_name, ctx);
                    }
                    return;
                }
            }
            let call = build_call(j, target, cell, inputs, ctx, &rw);
            if let Some(cond) = &j.condition {
                // 有 when：转 if-分支（命中即终结），未命中 fallthrough 到下一条
                let cond = rewrite(&cond.text, &inputs.alias_to_var);
                writeln!(out, "    if {cond} {{").unwrap();
                trace_send(ctx, out, "        ", &cell.name, "jump", target, j);
                writeln!(out, "        let _ = {call};").unwrap();
                // P1-10（登记于 2026-10-11，实测复现见 `docs/05`）：plain jump 是**转移式**
                // 出口，本实例到此终止、不再走到自己的 `exit go with`。可本实例的返回类型
                // `Ret_<本 Cell>` 的字段只由那一条 exit 决定，这里没有任何值可拿来构造它。
                // 写成 `Ret<X> { .. }` 的字段填充是**发明数据**，所以这里保留 `Ret<X> {{}}`：
                // 本 Cell 的 Ret 有字段时 rustc 立刻 E0063（`compile_error!` 之外唯一
                // 会出现的编译错，指向的是 DSL 的语义空洞而不是生成器的错）。
                // 不这么做的替代方案都改语义：`Ret<X>::default()` 会把零值当成
                // 「真的算出来了」交给 `jump and` 的结果槽，那是静默降级。
                writeln!(out, "        return Some({cell_ret_name} {{}});").unwrap();
                writeln!(out, "    }}").unwrap();
            } else {
                trace_send(ctx, out, "    ", &cell.name, "jump", target, j);
                writeln!(out, "    {{ let _ = {call}; return Some({cell_ret_name} {{}}); }}").unwrap();
            }
        }
        JumpKind::Recurse => {
            emit_jump_recurse(out, j, cell, inputs, cell_ret_name, ctx, stmt_idx);
        }
        JumpKind::Loop => {
            // jump loop 必须用 sync fn（cell_needs_async 在 jump loop 时返回 false）。
            // §12.4 语义：jump loop 是 body 末尾的"原地循环"；let 变量每轮重新绑定。
            //   - 在循环**外** emit `let mut X;`（保持后续作用域可见性）
            //   - 在循环**内** 第一批 emit `<X> = <rhs>;`（重新赋值）
            //   - 然后 emit if !cond break;
            //   - 最后 emit jump loop 的实参刷新（v_<id> = <arg>;）
            //
            // 备注：本批次简化——jump loop 之前只支持 `let X = <expr>;` 单行声明，
            // 不支持 `let X: T = ...;` 的注解重声明（直接 emit `let mut X;` 让 rustc 推断）。
            let cond = j
                .condition
                .as_ref()
                .map(|c| rewrite(&c.text, &inputs.alias_to_var))
                .unwrap_or_else(|| "true".into());

            // 只对 jump loop 实参键对应的 let 绑定做"外声明 + 内重算"处理。
            // 其他 let（不在 jump loop 实参键集中）原样按 RawRust 流程 emit（emitted earlier）。
            let mut needed: Vec<String> = Vec::new();
            for a in &j.args {
                let k = a.effective_key();
                if !k.is_empty() && !needed.contains(&k) {
                    needed.push(k);
                }
            }
            let assigns: Vec<(String, String)> = inputs
                .let_bindings
                .iter()
                .filter(|(n, _)| needed.contains(n))
                .cloned()
                .collect();

            // 1) 循环外 let mut 声明
            for (name, _) in &assigns {
                writeln!(out, "    let mut {name};").unwrap();
            }
            // 2) 循环
            writeln!(out, "    loop {{").unwrap();
            // 2a) 重算 let
            for (name, rhs) in &assigns {
                let rhs = rewrite(rhs, &inputs.alias_to_var);
                writeln!(out, "        {name} = {rhs};").unwrap();
            }
            // 2b) 条件 break
            writeln!(out, "        if !({cond}) {{ break; }}").unwrap();
            // 2c) jump loop 实参刷新到 v_<id>
            for (i, a) in j.args.iter().enumerate() {
                let vid = format!("v_{}", var_id(i));
                let expr = rewrite(&a.expr.text, &inputs.alias_to_var);
                writeln!(out, "        {vid} = {expr};").unwrap();
            }
            writeln!(out, "    }}").unwrap();
        }
        JumpKind::And => emit_jump_and(out, j, cell, inputs, ctx),
    }
}

/// `jump recurse` 的两条路径（D3 裁定）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RecursePath {
    /// 紧邻下一条语句就是 `jump sync <同组>`（或压根没有 group）→ **内联**：
    /// 直接调用子实例并等它收敛，父 Cell 拿到返回值继续跑。零 spawn、零 future。
    Inline,
    /// 没有紧邻的 sync → **派生**：子实例进组异步跑，父 Cell 立即继续，
    /// 到 `jump sync` 时才回收（D3 明文）。
    Spawn,
}

/// 按 D3 判定这条 recurse 走内联还是派生。
fn recurse_path(cell: &CellDef, j: &JumpStmt, stmt_idx: usize) -> RecursePath {
    let Some(g) = &j.group else { return RecursePath::Inline };
    match cell.body.get(stmt_idx + 1) {
        Some(Stmt::Sync { group: sg, .. }) if sg == g => RecursePath::Inline,
        _ => RecursePath::Spawn,
    }
}

/// `jump recurse <T> ... return(<key> -> <别名>) group <g>;`
///
/// §6.3：创建全新子实例，父实例等子实例完成后继续（**不是**出口语句）。
/// - 内联路径：`let __rsc_subN: Option<Ret_T> = <call>;` 然后按 tuple 解构一次性
///   把各 key 搬到用户声明的 `let mut 别名: Option<T>` 里（子实例 die ⇒ `None`）；
/// - 派生路径：单元素组（tuple 实参）交给 `__rsc_group_run_sync`，别名延迟到
///   `jump sync` 时绑定（值在那之前根本不存在）。
fn emit_jump_recurse(
    out: &mut String,
    j: &JumpStmt,
    cell: &CellDef,
    inputs: &CellInputs,
    _cell_ret_name: &str,
    ctx: &mut Ctx,
    stmt_idx: usize,
) {
    // `when <cond>`：recurse **不是出口**，条件不成立时什么都不做、继续往下跑。
    // 生成体先落到临时缓冲，需要时整体缩进一层套进 `if`。
    let mut body = String::new();
    emit_recurse_body(&mut body, j, cell, inputs, ctx, stmt_idx);
    match &j.condition {
        None => out.push_str(&body),
        Some(cond) => {
            let cond = rewrite(&cond.text, &inputs.alias_to_var);
            writeln!(out, "    if {cond} {{").unwrap();
            for line in body.split('\n') {
                if line.is_empty() {
                    continue;
                }
                writeln!(out, "    {line}").unwrap();
            }
            writeln!(out, "    }}").unwrap();
        }
    }
}

/// `jump recurse` 的生成体（不含 `when` 包裹）。
fn emit_recurse_body(
    out: &mut String,
    j: &JumpStmt,
    cell: &CellDef,
    inputs: &CellInputs,
    ctx: &mut Ctx,
    stmt_idx: usize,
) {
    let target = j.targets.first().expect("jump recurse needs a target");
    let rw = promote_read_with(out, j, cell, inputs, ctx, "    ");
    let ret_ty = ret_struct_name(target);
    match recurse_path(cell, j, stmt_idx) {
        RecursePath::Inline => {
            // §15.3：内联路径就是一个直接调用，子实例的 spawn/exit 由被调 Cell 自己发；
            // 这里只补发送侧一行（派生路径由 [`emit_group_spawn`] 发）。
            trace_send(ctx, out, "    ", &cell.name, "recurse", target, j);
            let call = build_call(j, target, cell, inputs, ctx, &rw);
            let slots = j.group.as_deref().map_or(0, |g| group_slots(cell, g));
            if j.ret_map.is_empty() {
                if slots > 0 {
                    // 父侧用多包 receive（`g.W1.key`）读子实例结果：值先 stash 到
                    // 与 `jump sync` 同名的结果变量，sync 处直接解包、不再等待。
                    let g = j.group.as_ref().expect("slots > 0 蕴含 group 存在");
                    let ret_ty = ret_struct_name(target);
                    let rvar = group_results_var(g, 0);
                    writeln!(
                        out,
                        "        let mut {rvar}: Vec<Option<{ret_ty}>> = vec![{call}];"
                    )
                    .unwrap();
                } else {
                    writeln!(out, "    let _ = {call};").unwrap();
                }
                return;
            }
            let n = j.ret_map.len();
            writeln!(out, "    let __rsc_sub: Option<{ret_ty}> = {call};").unwrap();
            // 一次性解构：多个别名共享同一个 `Option<Ret_T>`，只能移动一次。
            let binds: Vec<String> = (0..n).map(|i| format!("__rsc_b{i}")).collect();
            writeln!(
                out,
                "    let ({}) = match __rsc_sub {{",
                binds.join(", ") + if n == 1 { "," } else { "" }
            )
            .unwrap();
            // 单键也要写成 **1-tuple**：`(Some(o.k))` 只是加了括号的表达式，
            // 与左侧的 `(__rsc_b0,)` 类型不匹配（rustc E0308）。
            let some_fields: Vec<String> = j
                .ret_map
                .iter()
                .map(|(k, _)| format!("Some(__o.{})", k.name))
                .collect();
            let tail = if n == 1 { "," } else { "" };
            let none_fields: Vec<String> = (0..n).map(|_| "None".to_string()).collect();
            writeln!(out, "        Some(__o) => ({}{tail}),", some_fields.join(", ")).unwrap();
            writeln!(out, "        None => ({}{tail}),", none_fields.join(", ")).unwrap();
            writeln!(out, "    }};").unwrap();
            for (i, (_, alias)) in j.ret_map.iter().enumerate() {
                writeln!(out, "    {alias} = {};", binds[i]).unwrap();
            }
        }
        RecursePath::Spawn => {
            let slots = j.group.as_deref().map_or(1, |g| {
                let s = group_slots(cell, g);
                if s == 0 { 1 } else { s }
            });
            emit_group_spawn(out, j, target, 0, cell, inputs, ctx, &rw, slots, /*from_tuple=*/ true);
        }
    }
}

/// §15.3：整组派生那一行（`jump and` / `jump recurse` 的派生路径）。
///
/// `limit` / `items` 是**运行期值**（`concurrent_limit` 可以是透传表达式），所以这两格
/// 走 `eprintln!` 的实参而不是编译期文本；发在 `let __rsc_lim` 之后、
/// `std::thread::spawn` 之前，此时两个局部变量都已就绪且还没被 `move` 进线程。
fn trace_group_derive(
    out: &mut String,
    ctx: &Ctx,
    glabel: &str,
    caller: &CellDef,
    target: &str,
    j: &JumpStmt,
) {
    if !ctx.trace {
        return;
    }
    emit_trace(
        out,
        "        ",
        &format!(
            "group {} derive {} -> {} limit={{}} items={{}}{} send={}",
            glabel,
            caller.name,
            target,
            trace_pos(j.span),
            trace_send_keys(&j.args)
        ),
        &["__rsc_lim", "__rsc_items.len()"],
    );
}

/// §15.3：派生实例闭包的埋点版本 —— 并发令牌申请/释放 + 该实例的 `died` 标记。
///
/// **为什么埋在生成物里而不是内嵌运行时里**：`__rsc_run_group_sync` 的源码属于**无条件**
/// 嵌入的 `RUNTIME_SOURCE`，往里加一行就改变 release 生成物的字节（性能门禁的分母会跟着漂）。
///
/// **为什么这一对事件就是「semaphore 令牌申请/释放」**：本实现用 `min(limit, n)` 个
/// 工作线程跑有界并发（§12.5 防爆内存），一个线程取到一个元素 = 拿到一张令牌、跑完 = 归还，
/// 所以 acquire/release 的配对次数直接给出「峰值并发 = limit」这条用户最关心的事实。
///
/// `!ctx.trace` 时返回**逐字不变的单行闭包**，release 生成物因此一个字节都不动。
fn trace_closure(ctx: &Ctx, call_body: &str, glabel: &str, target: &str) -> String {
    if !ctx.trace {
        return format!("|__rsc_it| {call_body}");
    }
    let mut s = String::new();
    s.push_str("|__rsc_it| {\n");
    emit_trace(
        &mut s,
        "                ",
        &format!("group {glabel} permit cell={target} acquire"),
        &[],
    );
    writeln!(s, "                let __rsc_r = {call_body};").unwrap();
    emit_trace(
        &mut s,
        "                ",
        &format!("group {glabel} permit cell={target} release died={{}}"),
        &["__rsc_r.is_none()"],
    );
    s.push_str("                __rsc_r\n            }");
    s
}

/// 派生型 jump 的公共生成体（`jump and` 的每个目标 / `jump recurse` 的派生路径）。
///
/// `from_tuple = true` 时 items 是**一个 tuple**（每目标一个实例：`jump and` 无 over、
/// 或 recurse 的派生路径）；`false` 时 items 是 over 集合本身（每元素一个实例）。
///
/// **为什么 `#[allow(clippy::too_many_arguments)]`**：代码生成器的每个 emit_* 都必然同时携带
/// 「输出缓冲 + 当前语句 + Cell 级上下文（inputs/async 位/别名重写表）+ 本形态专属参数」。
/// 把它们打包成一个结构体只会把同一组字段换一种写法传递（字段仍要逐个可变），
/// 可读性反而下降；参数个数在这里是**形状的直接描述**，不是坏味道。
/// 仓库内同类先例：`rscel-parser/src/ast.rs` 的 `#[allow(clippy::large_enum_variant)]`（就地注释理由）。
#[allow(clippy::too_many_arguments)]
fn emit_group_spawn(
    out: &mut String,
    j: &JumpStmt,
    target: &str,
    target_idx: usize,
    cell: &CellDef,
    inputs: &CellInputs,
    ctx: &Ctx,
    rw: &std::collections::HashMap<String, String>,
    slots: usize,
    from_tuple: bool,
) {
    let callee = cell_fn_name(target);
    let ret_ty = ret_struct_name(target);
    // §15.3 埋点的组名文本（两个分支共用）。
    let glabel = trace_group_label(j.group.as_deref().unwrap_or(""));
    let pos = target_key_positions(ctx.program, target);
    // 实参按被调方顺序
    let mut ordered: Vec<(usize, String)> = j
        .args
        .iter()
        .map(|a| {
            let k = a.effective_key();
            let p = pos.get(&k).copied().unwrap_or(usize::MAX);
            let expr = match rw.get(&k) {
                Some(e) => e.clone(),
                None => rewrite(a.expr.text.trim(), &inputs.alias_to_var),
            };
            (p, expr)
        })
        .collect();
    ordered.sort_by_key(|(p, _)| *p);

    // 并发上限（P0-2）：显式 concurrent_limit 用 N；不写时用 available_parallelism，
    // 绝不退化成"每个元素一个 OS 线程"（实测 100 万元素 → 47,724 线程 / 828 MB / 150 s）。
    let limit_expr = match &j.concurrent_limit {
        Some(lim) => rewrite(lim.text.trim(), &inputs.alias_to_var),
        None => "std::thread::available_parallelism().map(std::num::NonZeroUsize::get).unwrap_or(1)".to_string(),
    };

    // 组句柄（第 0 个目标沿用 `__rsc_g_<group>` 形状，多目标时后续目标带序号）。
    let group = j.group.clone().unwrap_or_default();
    let gvar = group_var(&group, target_idx);
    writeln!(out, "        let __rsc_g = Arc::new(__RscGroup::<{ret_ty}>::new());").unwrap();
    writeln!(out, "        let __rsc_g2 = Arc::clone(&__rsc_g);").unwrap();

    if from_tuple {
        // recurse 派生：实参打包成 tuple，一个元素 = 一个子实例。
        let tuple: Vec<String> = ordered.iter().map(|(_, e)| e.clone()).collect();
        let tuple_text = match tuple.len() {
            0 => "()".to_string(),
            1 => format!("({},)", tuple[0]),
            _ => format!("({})", tuple.join(", ")),
        };
        let field_list = (0..tuple.len())
            .map(|i| format!("__rsc_it.{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let call_body = if ctx.is_async(target) {
            format!("__rsc_block_on({callee}({field_list}))")
        } else {
            format!("{callee}({field_list})")
        };
        writeln!(out, "        let __rsc_items: Vec<_> = vec![{tuple_text}];").unwrap();
        writeln!(out, "        let __rsc_lim: usize = {limit_expr};").unwrap();
        trace_group_derive(out, ctx, &glabel, cell, target, j);
        writeln!(out, "        std::thread::spawn(move || {{").unwrap();
        let closure = trace_closure(ctx, &call_body, &glabel, target);
        writeln!(
            out,
            "            __rsc_group_run_sync(__rsc_items, __rsc_lim, {slots}, &__rsc_g2, {closure});"
        )
        .unwrap();
        writeln!(out, "        }});").unwrap();
    } else {
        // and + over：每个集合元素派生一个实例。
        //
        // **over 元素填哪个形参**由被调方的 receive 顺序决定，而不是写死在第 0 位：
        // `cell worker; receive(x: i64, cfg: Arc<String>)` 与
        // `receive(cfg: Arc<String>, x: i64)` 是两种不同签名，父侧
        // `jump and worker over items read with(cfg)` 必须把 over 元素送到 `x` 的位置。
        let n_params = ctx
            .program
            .cell(target)
            .map_or(1, |tc| tc.receive.keys.len().max(1));
        let filled: std::collections::HashSet<usize> = ordered
            .iter()
            .filter_map(|(p, _)| if *p < n_params { Some(*p) } else { None })
            .collect();
        let item_at = (0..n_params).find(|i| !filled.contains(i));
        let mut pos_to_extra: std::collections::HashMap<usize, usize> =
            std::collections::HashMap::new();
        let extras: Vec<String> = ordered.iter().map(|(_, e)| e.clone()).collect();
        for (k, (p, _)) in ordered.iter().enumerate() {
            if *p < n_params {
                pos_to_extra.insert(*p, k);
            }
        }
        // extras 必须绑成**逐个局部变量**，不能绑成一个元组再按 `.k` 取字段：
        // 取值发生在 `|__rsc_it| ...` 这个 `Fn` 闭包里，而元组是被闭包**捕获**的，
        // 每次调用都得把字段 move 出去 → rustc E0507「cannot move out of
        // `__rsc_extra.0`, a captured variable in an `Fn` closure」。
        // 2026-10-10 实测：先修掉"只有 1 个 extra 时 `(x)` 少写尾逗号"的 E0609
        // （`(x)` 是括号表达式、不是 1-元组，`.0` 取值报 E0609 no field `0`），
        // 才暴露出这一层 —— `jump and ... over ... read with(...)` 这条路径**从来没有**
        // 端到端跑通过（bench/read_share.rscel 也正是因此跑不起来）。
        //
        // 每个子实例都要拿到**自己的一份**实参（S2 按值传参），所以每实例 `.clone()`
        // 一次：`read with` 的 extra 本身已是 `Arc::clone(&v_x)`，再 clone 就是引用
        // 计数 +1，与 §8.1「每实例一次 Arc::clone」以及手写对照 bench/read_share_hand.rs
        // 里的 `Arc::clone(data)` 完全同形。
        //
        // **为什么不做"没被引用到的 extra 就不发"的裁剪**（独立审查指出过）：实参表达式
        // 可以带副作用（`go with({println!("x"); 1i64} -> cfg)`），裁掉就是**静默改变语义**。
        // 不裁也不会告警：`__rsc_*` 以下划线开头，rustc 的 `unused_variables` 本来就不管
        // 下划线前缀的绑定（实测 `rustc -D warnings` 编译通过），不需要额外 allow。
        let mut parts: Vec<String> = Vec::with_capacity(n_params);
        for i in 0..n_params {
            if Some(i) == item_at {
                parts.push("__rsc_it".to_string());
            } else if let Some(k) = pos_to_extra.get(&i) {
                parts.push(format!("__rsc_extra_{k}.clone()"));
            }
        }
        // over 管道在 supervisor 线程**外面**求值：集合属于父实例，
        // 放进 move 闭包里会让父 Cell 后续再也用不了它。
        // 注意块尾**不能加分号**——`{ v.collect(); }` 的值是 `()`。
        let mut chain = String::new();
        emit_over_chain(&mut chain, j.over.as_ref().expect("and 必带 over"), inputs);
        writeln!(out, "        let __rsc_items: Vec<_> = {{ {chain}.collect() }};").unwrap();
        for (k, expr) in extras.iter().enumerate() {
            writeln!(out, "        let __rsc_extra_{k} = {expr};").unwrap();
        }
        writeln!(out, "        let __rsc_lim: usize = {limit_expr};").unwrap();
        trace_group_derive(out, ctx, &glabel, cell, target, j);
        writeln!(out, "        std::thread::spawn(move || {{").unwrap();
        let call_args = parts.join(", ");
        let inner = format!("{callee}({call_args})");
        let call_body = if ctx.is_async(target) {
            format!("__rsc_block_on({inner})")
        } else {
            inner
        };
        let closure = trace_closure(ctx, &call_body, &glabel, target);
        writeln!(
            out,
            "            __rsc_group_run_sync(__rsc_items, __rsc_lim, {slots}, &__rsc_g2, {closure});"
        )
        .unwrap();
        writeln!(out, "        }});").unwrap();
    }

    if group.is_empty() {
        // 无组名：fire-and-forget，句柄就地丢弃。
        writeln!(out, "        drop(__rsc_g);").unwrap();
    } else {
        writeln!(out, "        let {gvar}: Arc<__RscGroup<{ret_ty}>> = __rsc_g;").unwrap();
    }
}

// ------------------------------------------------------- jump and / jump sync

/// 组句柄局部变量名。
///
/// 一个 `jump and` 的每个目标各自一个子组（`idx = 0` 沿用无前缀名字，保持既有
/// 生成形状与测试断言）；`jump sync <g>` 依次等待全部子组。
fn group_var(group: &str, idx: usize) -> String {
    if idx == 0 {
        format!("__rsc_g_{group}")
    } else {
        format!("__rsc_g_{group}_{}", idx + 1)
    }
}

/// 组结果局部变量名（`jump sync` 处）。
fn group_results_var(group: &str, idx: usize) -> String {
    if idx == 0 {
        format!("__rsc_res_{group}")
    } else {
        format!("__rsc_res_{group}_{}", idx + 1)
    }
}

/// 一个 group 在本 Cell 里的布局：创建它的 jump、目标数、槽位如何映射到子组。
struct GroupLayout {
    /// 创建该组的 jump（`jump and` / `jump recurse`）。
    creator: JumpStmt,
    /// 是否为内联 recurse（子实例已在 jump 处 await 完，`jump sync` 无需等待）。
    inline: bool,
    /// 目标数（多目标 `jump and` 时每个目标一个子组）。
    n_targets: usize,
}

/// 在本 Cell 里找创建 `group` 的 jump 并判定路径。
fn group_layout(cell: &CellDef, group: &str) -> Option<GroupLayout> {
    for (idx, stmt) in cell.body.iter().enumerate() {
        if let Stmt::Jump(j) = stmt {
            if j.group.as_deref() == Some(group)
                && matches!(j.kind, JumpKind::And | JumpKind::Recurse)
            {
                let inline = j.kind == JumpKind::Recurse
                    && recurse_path(cell, j, idx) == RecursePath::Inline;
                return Some(GroupLayout {
                    creator: j.clone(),
                    inline,
                    n_targets: j.targets.len(),
                });
            }
        }
    }
    None
}

/// `W<n>` 槽位 → (子组下标, 子组内槽位下标)。
///
/// - 单目标：所有 W 都落在子组 0，子组内下标 = n-1（`over` 派生的顺序对应集合顺序）；
///   无 over 的单目标（例如 recurse 派生）也走这条：`W1` → (0, 0)。
/// - 多目标（§6.4 `jump and w1, w2`）：`Wk` → 第 k-1 个目标，子组内第 0 个实例。
fn slot_to_sub_slot(layout: &GroupLayout, slot: usize) -> (usize, usize) {
    if layout.n_targets >= 2 && layout.creator.over.is_none() {
        (slot - 1, 0)
    } else {
        (0, slot - 1)
    }
}

/// 当前 Cell 为 `group` 静态枚举出的多包 receive 槽位数（D5）。
///
/// 槽位下标 = `slot_index_of_key` 返回值（0-based），槽位数 =
/// **最大下标 + 1**（不是匹配键的计数）。P1-3 修前用 `count()` 会让
/// 不连续 W 编号（如 `W1`、`W3`）漏算，导致 `results[idx=2]` 越界
/// panic；修后改 max+1，跳号槽位是 `None` 占位，由 `emit_sync` 的
/// `.and_then(...)` 容忍。
///
/// 例：`g1.W1.A`、`g1.W3.A`（漏 W2）⇒ keep = 3，槽位 [0]=W1、[1]=None、[2]=W3。
fn group_slots(cell: &CellDef, group: &str) -> usize {
    cell.receive
        .keys
        .iter()
        .filter_map(|k| slot_index_of_key(k.pkg.as_deref(), group))
        .max()
        .map(|m| m + 1)
        .unwrap_or(0)
}

/// 从包限定前缀 `g1.W3` 解析出槽位下标（`W3` → 2）。
///
/// 契约：`<group>.<W 槽位名>`，槽位名形如 `W1`、`W2`（§9.4 / D5 静态枚举）。
fn slot_index_of_key(pkg: Option<&str>, group: &str) -> Option<usize> {
    let pkg = pkg?;
    let mut it = pkg.split('.');
    if it.next()? != group {
        return None;
    }
    let n: usize = it.next()?.strip_prefix('W')?.parse().ok()?;
    if n == 0 { None } else { Some(n - 1) }
}

/// `jump and <t1, t2, …> [go/read with(…)] [over <coll> |> ops] [concurrent_limit=N] [group g];`
///
/// （段序固定：mode 段在 `over` **之前**，写反了 parser 直接给 RSC-E033，规范 §7.4。）
///
/// 生成形状（§12.5 惰性 spawn + D4 增量回收 + S2 类型化调用）：
///
/// ```text
/// let __rsc_items: Vec<_> = { <coll>.into_iter().filter(..).map(..).collect(); };
/// let __rsc_extra_0 = Arc::clone(&cfg);         // 每个 go/read with 实参一个局部变量
/// let __rsc_g = Arc::new(__RscGroup::<RetWorker>::new());
/// let __rsc_g2 = Arc::clone(&__rsc_g);
/// std::thread::spawn(move || {
///     __rsc_group_run_sync(__rsc_items, __rsc_lim, SLOTS,
///                          |__rsc_it| cell_worker(__rsc_it, __rsc_extra_0.clone()));
/// });
/// let __rsc_g_g1: Arc<__RscGroup<RetWorker>> = __rsc_g;
/// ```
///
/// extra 的形状是**逐个局部变量 + 每实例 `.clone()`**，不是"一个元组 + 按字段 move"：
/// 取值发生在被 supervisor 闭包捕获的 `Fn` 闭包里，每次派生都要把元组字段 move 出去，
/// rustc 直接 E0507。2026-10-10 之前这份文档写的 `(Arc::clone(&v_cfg),)` + `.0`
/// 与代码一致、但**那段代码从来编译不过**，所以文档也是在描述一个没跑通的东西。
///
/// 三条本轮修掉的语义缺陷：
/// 1. **每个目标各自一个子组**：§6.4 的 `jump and worker1, worker2 group g1` 是两种
///    Cell 真并行，修前只 spawn `targets[0]`、把其余目标**静默丢掉**（父侧 `g1.W2.A`
///    取到的是 worker1 的第二个实例或默认值）。
/// 2. **over 元素按被调方 receive 顺序落位**（见 [`emit_group_spawn`]），
///    而不是写死第一个形参。
/// 3. **同步目标不再经过 future**：旧形状 `std::future::ready(..)` +
///    `__rsc_block_on(..)` 让每个元素都要 `Box::pin` 一个 future 并造一个
///    `Waker`（Arc 分配）再 poll 一次；对纳秒级 worker 这就是主要开销（§17.1）。
///
/// **`jump and` 本身不等待**——这正是它与 `jump sync` 的区别；父 Cell 继续往下跑。
fn emit_jump_and(
    out: &mut String,
    j: &JumpStmt,
    cell: &CellDef,
    inputs: &CellInputs,
    ctx: &mut Ctx,
) {
    let rw = promote_read_with(out, j, cell, inputs, ctx, "        ");
    let base_slots = j.group.as_deref().map_or(0, |g| group_slots(cell, g));
    let n = j.targets.len();
    for (idx, target) in j.targets.clone().iter().enumerate() {
        // 多目标：每个子组各 1 个实例，父侧槽位 W(k) 对应第 k 个目标（§6.4 入组顺序）。
        // 单目标 + over：槽位数 = 多包 receive 声明的上界（D5），结果与集合顺序一一对应。
        let slots = if n >= 2 { 1 } else { base_slots };
        let from_tuple = j.over.is_none() || n >= 2;
        emit_group_spawn(out, j, target, idx, cell, inputs, ctx, &rw, slots, from_tuple);
    }
}

/// `jump sync <group>;` —— 等待整组完成，并解包多包 receive（S4 late binding / D5）。
///
/// 三种来源共用一套解包逻辑（[`group_layout`]）：
/// - `jump and` 派生：逐子组 `wait()` 取回 `Vec<Option<Ret_T>>`；
/// - `jump recurse` **派生**路径（D3：紧邻不是 sync）：同上，另外把
///   `return(<key> -> <别名>)` 的别名**延迟到这里**绑定（值在 sync 之前不存在）；
/// - `jump recurse` **内联**路径（D3 默认）：子实例已在 jump 处 await 完，
///   结果已由 jump 语句 stash 到同名结果变量（或已绑定到 return 别名），
///   这里**不再等待**。
fn emit_sync(
    out: &mut String,
    group: &str,
    cell: &CellDef,
    inputs: &CellInputs,
    ctx: &Ctx,
    sync_span: Span,
) {
    let Some(layout) = group_layout(cell, group) else {
        // check 层 E035 已拒绝"未创建就 sync"；走到这里说明上游契约不完整，
        // 但不能静默生成 `undefined variable` 的 Rust —— 发一条注释 + 让
        // 后续 rustc 错误通过反映射指回该 DSL 行。
        writeln!(out, "        // RSC: group `{group}` 未在本 Cell 创建（E035 应由 check 拦截）").unwrap();
        return;
    };
    let j = &layout.creator;
    let n = layout.n_targets.max(1);

    // 1) 等待各子组，拿到结果向量
    for idx in 0..n {
        let Some(target) = j.targets.get(idx) else { continue };
        let ret_ty = ret_struct_name(target);
        let rvar = group_results_var(group, idx);
        if layout.inline {
            // 内联路径：值已在 jump 处 stash（`let mut __rsc_res_g = vec![call];`）。
            continue;
        }
        let gvar = group_var(group, idx);
        writeln!(out, "        let mut {rvar}: Vec<Option<{ret_ty}>> = {gvar}.wait();").unwrap();
        // §15.3「group 完成状态」+ died 标记：`Vec<Option<Ret_x>>` 里的 `None`
        // 就是「该槽位的子实例 die 了」。
        // **这两个格子只统计被保留的槽位**（D5：`slots` 之外的结果在生成侧就被丢弃，
        // 运行时根本拿不到它们），所以 `jump and` 没有 `g1.Wk` 接收方时 `slots=0`、
        // `died_in_slots=0`，即使真的有实例 die 也一样 —— 实测过这种形状。
        // 每实例的真实 died 在 `permit ... release died=` 那一行，事件词汇表按这个口径写。
        // 位置用**这条 `jump sync`** 的行号（`sync_span`），不是创建组那条 jump 的：
        // 完成事件发生在等待处，指回 `jump and` 会让编辑器把高亮打到错误的行。
        // 内联 recurse 路径不发：那一头 `wait()` 根本不存在，事件由被调 Cell 自己给。
        if ctx.trace {
            emit_trace(
                out,
                "        ",
                &format!(
                    "group {} done cell={} slots={{}} died_in_slots={{}}{}",
                    trace_group_label(group),
                    target,
                    trace_pos(sync_span)
                ),
                &[
                    &format!("{rvar}.len()"),
                    &format!("{rvar}.iter().filter(|r| r.is_none()).count()"),
                ],
            );
        }
    }

    // 2) 多包 receive：`g1.W2.A` → 对应子组槽位的字段 A
    for (i, key) in cell.receive.keys.iter().enumerate() {
        let Some(zero_based) = slot_index_of_key(key.pkg.as_deref(), group) else { continue };
        let (sub, sub_slot) = slot_to_sub_slot(&layout, zero_based + 1);
        let rvar = group_results_var(group, sub);
        let ty = inputs.typed_params.get(i).map(|(_, t)| t.clone()).unwrap_or_default();
        let vid = inputs
            .alias_to_var
            .get(i)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| format!("v_{}", var_id(i)));
        // P1-2：区分「D5 元素数 < 槽位」与「子实例 die」。
        //   - rvar.get_mut(sub_slot) == None       ⇒ 槽位超出实际元素数（D5）→ Default
        //   - == Some(None)                        ⇒ 子实例 die → panic S003
        //   - == Some(Some)                        ⇒ 正常取字段
        writeln!(out, "        let {vid}: {ty} = match {rvar}.get_mut({sub_slot}) {{").unwrap();
        writeln!(out, "            Some(Some(__o)) => __o.{},", key.name).unwrap();
        writeln!(
            out,
            "            Some(None) => panic!(\"S003: 子实例 die 导致多包 receive 缺键\"),"
        )
        .unwrap();
        writeln!(out, "            None => Default::default(),").unwrap();
        writeln!(out, "        }};").unwrap();
    }

    // 3) 派生路径的 return 别名：延迟绑定（内联路径已在 jump 处绑好）
    if !layout.inline && !j.ret_map.is_empty() {
        let rvar = group_results_var(group, 0);
        let n2 = j.ret_map.len();
        let binds: Vec<String> = (0..n2).map(|i| format!("__rsc_b{i}")).collect();
        let tuple_tail = if n2 == 1 { "," } else { "" };
        writeln!(
            out,
            "        let ({}{}) = match {rvar}.get_mut(0).and_then(Option::take) {{",
            binds.join(", "),
            tuple_tail
        )
        .unwrap();
        let some_fields: Vec<String> = j
            .ret_map
            .iter()
            .map(|(k, _)| format!("Some(__o.{})", k.name))
            .collect();
        let tail = if n2 == 1 { "," } else { "" };
        let none_fields: Vec<String> = (0..n2).map(|_| "None".to_string()).collect();
        writeln!(out, "            Some(__o) => ({}{tail}),", some_fields.join(", ")).unwrap();
        writeln!(out, "            None => ({}{tail}),", none_fields.join(", ")).unwrap();
        writeln!(out, "        }};").unwrap();
        for (i, (_, alias)) in j.ret_map.iter().enumerate() {
            writeln!(out, "        {alias} = {};", binds[i]).unwrap();
        }
    }
}


/// 生成 `over` 管道链：`<coll>.into_iter().filter(..).map(..)...`。
///
/// 算子实参是**纯 Rust 闭包/表达式**（E080 已保证闭包内无 DSL 关键字），
/// 因此按 §10.3 做别名重写后逐字透传。
fn emit_over_chain(out: &mut String, over: &OverSpec, inputs: &CellInputs) {
    let coll = rewrite(over.collection.text.trim(), &inputs.alias_to_var);
    write!(out, "{}.into_iter()", coll.trim()).unwrap();
    for op in &over.ops {
        match op {
            OverOp::Filter(c) => write!(out, ".filter({})", rewrite(c.text.trim(), &inputs.alias_to_var)).unwrap(),
            OverOp::Map(c) => write!(out, ".map({})", rewrite(c.text.trim(), &inputs.alias_to_var)).unwrap(),
            OverOp::FlatMap(c) => write!(out, ".flat_map({})", rewrite(c.text.trim(), &inputs.alias_to_var)).unwrap(),
            OverOp::Take(c) => write!(out, ".take({})", rewrite(c.text.trim(), &inputs.alias_to_var)).unwrap(),
            OverOp::Skip(c) => write!(out, ".skip({})", rewrite(c.text.trim(), &inputs.alias_to_var)).unwrap(),
        }
    }
}


/// 生成 `fn main`（§14.3 argv 入口）。
///
/// 映射记一条 `MapKind::Main` 覆盖整个函数：`fn main` 里唯一的 DSL 相关决策是
/// 「入口 Cell 是哪个、入参怎么从 argv 来」，所以它的 DSL 位置就是入口 Cell 的定义行。
/// 若入口 Cell 在 AST 里找不到（`entry` 指向不存在的名字，check 层会先拦下），
/// `DslPos` 的 `line` 留 0 —— [`DslPos::has_position`] 为假，反映射退回原样输出。
fn emit_main(
    out: &mut String,
    program: &Program,
    env: &TypeEnv,
    async_cells: &std::collections::HashSet<String>,
    map: &mut SrcMapBuilder,
) {
    let entry = program
        .entry
        .clone()
        .or_else(|| program.cells().find(|c| c.name == "main").map(|c| c.name.clone()))
        .expect("program must have entry or cell main");
    let entry_fn = cell_fn_name(&entry);
    let entry_cell = program.cells().find(|c| c.name == entry);
    let entry_async = async_cells.contains(&entry);
    let main_start = next_line(out);
    writeln!(out, "fn main() {{").unwrap();
    // §14.3 argv 入口：先在 main 顶层收集 `std::env::args()`，跳过 argv[0]（二进制路径），
    // 再按 entry Cell 的 receive 形参类型逐个转型：
    //   - `Vec<String>` → 整个 argv（A.8 的 `receive(argv: Vec<String>)` 写法）
    //   - POD（i64/u64/isize/usize/i32/u32/f64/f32/bool/char/String）→ `parse()` / 字面
    //   - 其它类型 → `Default::default()`（argv 只能承载文本，非文本入参留给程序自己读）
    //
    // 同步 entry 不再套 `__rsc_block_on(async { .. })`：绝大多数 DSL 程序全程没有
    // future（见 [`needs_async_set`]），套一层只是白造一个 future + waker。
    writeln!(
        out,
        "    let __rsc_argv: Vec<String> = std::env::args().skip(1).collect();"
    )
    .unwrap();
    if entry_async {
        writeln!(out, "    let _ = __rsc_block_on(async move {{").unwrap();
        write!(out, "        let _ = {entry_fn}(").unwrap();
    } else {
        write!(out, "    let _ = {entry_fn}(").unwrap();
    }
    if let Some(cell) = entry_cell {
        let inputs = collect_inputs(cell, env);
        for (i, (_, ty)) in inputs.typed_params.iter().enumerate() {
            if i > 0 { write!(out, ", ").unwrap(); }
            write!(out, "{}", argv_expr_for_type(ty, i)).unwrap();
        }
    }
    if entry_async {
        writeln!(out, ").await;").unwrap();
        writeln!(out, "    }});").unwrap();
    } else {
        writeln!(out, ");").unwrap();
    }
    writeln!(out, "}}").unwrap();
    let entry_span = entry_cell.map(|c| c.span).unwrap_or_default();
    map.push(MapKind::Main, DslPos::item(&entry, entry_span), false, main_start, written_lines(out));
}

/// entry Cell 形参类型 → 从 `__rsc_argv` 取值的 Rust 表达式。
fn argv_expr_for_type(ty: &str, i: usize) -> String {
    let t = ty.trim();
    let bare = t.replace(' ', "");
    if bare == "Vec<String>" {
        // 拿到的是**全部**实参，且不消费自己的位置：排在它后面的键仍从 `argv[1]` 起算
        // （`receive(rest: Vec<String>, n: i64)` 配 `a b c d` → rest=4 个、n 解析 "b" 失败）
        return "__rsc_argv.clone()".to_string();
    }
    if bare == "String" {
        return format!("__rsc_argv.get({i}).cloned().unwrap_or_default()");
    }
    if matches!(
        bare.as_str(),
        "i64" | "u64" | "isize" | "usize" | "i32" | "u32" | "i16" | "u16" | "i8" | "u8" | "f64" | "f32"
    ) {
        return format!("__rsc_argv.get({i}).and_then(|s| s.parse().ok()).unwrap_or_default()");
    }
    if bare == "bool" {
        return format!(
            "__rsc_argv.get({i}).map(|s| s == \"true\" || s == \"1\").unwrap_or_default()"
        );
    }
    if bare == "char" {
        return format!("__rsc_argv.get({i}).and_then(|s| s.chars().next()).unwrap_or_default()");
    }
    // argv 承载不了的类型（`Vec<i64>`、自定义 struct 等）落到 Default：程序照旧编译运行，
    // 但用户传的命令行实参被丢弃，而 check 层目前零诊断 —— 这条静默降级登记在
    // docs/05-迭代待办.md P1-5，白名单与语义见 docs/02 §11.3。
    "Default::default()".to_string()
}
