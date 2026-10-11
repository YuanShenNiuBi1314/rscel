//! trace 干跑模拟（§16.5）：不真正 spawn，只按 CFG 顺序把消息链路走一遍。
//!
//! ## 它是什么 / 不是什么
//!
//! §16.5 第 4 条把它定位成**链路排障与教学演示**工具，不是解释器。它回答的是
//! 「哪条 jump 会开新实例、键名怎么对应到目标的 receive、group 槽位谁来收、哪一路
//! die 了」，**不回答「算出来等于几」**。因此本模块：
//!
//! - 不生成 Rust、不调 rustc、不执行任何用户表达式；
//! - 环境里存的是**表达式文本**（`let x = a + b;` → `x = "a + b"`），值一律带
//!   [`UNEVALUATED`] 标注；packet 里的 value 同样是文本；
//! - `when` 条件只解释「字面量 / 简单标识符（查环境）」配 `== != < > <= >=`
//!   的比较 —— 这是 §16.5 明文限定的**首版范围**（边界见 [`eval_when`]）。
//!   算得出真假就按结果走，算不出就在 step 上标 `(uninterpretable)` 并**按不命中
//!   处理**：宁可少画一条边，也不猜一条假边。
//!
//! ## 为什么不与 `topology` 合并
//!
//! `topology` 是**静态图**（一个 Cell 一个节点，边按语句去重），trace 是**动态序列**
//! （同一个 Cell 被 loop/recurse 走 N 次就有 N 段步骤）。两者消费者的诉求不同：
//! 图看拓扑、trace 看时间线。塞进一个模块只会让字段互相打架。
//!
//! ## 保护（绝不静默降级）
//!
//! 任一上限触顶都往 [`Trace::notes`] 写清「已达…上限，链路未走完」：
//! [`MAX_STEPS`]（刷屏 / 内存失控）、[`MAX_DEPTH`]（plain jump 成环，如 A→B→A）、
//! `--max-items`（`over` 元素数 / `jump loop` 轮数 / `jump recurse` 层数）。
//! 末尾还会汇总「多少处条件不可解释、多少处值未求值、几个 extern rust 块」，
//! 读的人必须能看出这份链路的可信边界在哪里。

use std::collections::{BTreeMap, HashMap};
use std::cmp::Ordering;

use rscel_parser::{
    is_plain_ident, Arg, CellDef, Ident, JumpKind, JumpStmt, OverOp, OverSpec, Program, RawBlock,
    RawBlockPart, RawRust, Span, Stmt,
};

use crate::topology::mode_text;

/// 值不求值的统一标注（§16.5：不保证与真实执行数值一致）。
pub const UNEVALUATED: &str = "(unevaluated)";
/// `when` 条件超出可解释范围的标注（§16.5 首版限定）。
pub const UNINTERPRETABLE: &str = "(uninterpretable)";
/// 入口 / 传参没有对应实参时的占位文本。
pub const UNBOUND: &str = "(unbound)";
/// group 槽位数 > 已模拟实例数时缺失键的取值（D5：gen 侧用 `Default::default()` 兜底）。
pub const D5_DEFAULT: &str = "(D5 default)";
/// 子实例 `die` 后父侧该键的取值（S3）。
pub const DIED: &str = "(died)";

/// `--max-items` 默认值（与前端 `#maxItems` 输入框的默认值一致）。
pub const DEFAULT_MAX_ITEMS: usize = 4;
/// 单次模拟的 step 总上限。
pub const MAX_STEPS: usize = 5000;
/// 子实例递归深度上限。
pub const MAX_DEPTH: usize = 64;

/// 一条 step 的种类（§6 的五种 jump 形态 + 三种出口 / 打印语句）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepKind {
    /// plain jump（出口转移）。
    Jump,
    Loop,
    Recurse,
    And,
    Sync,
    Exit,
    Die,
    /// `out(...)`：不是出口，但它是链路里唯一「看得见值」的地方。
    Out,
}

impl StepKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StepKind::Jump => "jump",
            StepKind::Loop => "loop",
            StepKind::Recurse => "recurse",
            StepKind::And => "and",
            StepKind::Sync => "sync",
            StepKind::Exit => "exit",
            StepKind::Die => "die",
            StepKind::Out => "out",
        }
    }
}

/// packet 的一个字段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PacketField {
    /// 键名；多包接收时是完整 `g1.W1.d` 形态（父侧就是这么写的）。
    pub key: String,
    /// 值**文本**（不求值）。
    pub value: String,
    /// S3：产出它的子实例走了 `die` → 本键没有值。
    pub died: bool,
}

/// 模拟时间线上的一步。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    /// 序号，1 起始（前端回放滑杆按它定位）。
    pub seq: usize,
    pub cell: String,
    /// 语句在所属 Cell `body` 里的下标；嵌套 Rust 块内的语句记**所在块语句**的下标
    /// （与 [`crate::topology::Edge::stmt`] 同一规则：块内相对下标对上层没有意义）。
    pub stmt: usize,
    pub line: usize,
    pub col: usize,
    pub kind: StepKind,
    /// 语句的还原文本（`jump and worker over items concurrent_limit=2 group g1`）。
    pub text: String,
    pub packet: Vec<PacketField>,
    /// 本步所属并发组（`jump and` / `jump recurse` 派生的子实例会带上）。
    pub group: Option<String>,
    pub note: Option<String>,
}

/// 一次完整模拟的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Trace {
    pub steps: Vec<Step>,
    /// 全局说明：触顶、截断、fallthrough、extern rust 块、求值边界汇总。
    pub notes: Vec<String>,
}

/// 模拟参数。
#[derive(Clone, Debug)]
pub struct TraceOptions {
    /// `over` 实例数上限 / `jump loop` 轮数上限 / `jump recurse` 层数上限。
    pub max_items: usize,
    pub max_steps: usize,
    pub max_depth: usize,
    /// `--arg` 原文：`key=value` 具名，或按 `receive` 顺序的位置值。
    pub entry_args: Vec<String>,
}

impl Default for TraceOptions {
    fn default() -> Self {
        TraceOptions { max_items: DEFAULT_MAX_ITEMS, max_steps: MAX_STEPS, max_depth: MAX_DEPTH, entry_args: Vec::new() }
    }
}

/// 解析一条 `--arg`：`k=v` → 具名（按 receive 键名或别名找），否则按位置落位。
pub fn parse_entry_arg(spec: &str) -> (Option<Ident>, String) {
    match spec.split_once('=') {
        Some((k, v)) if is_plain_ident(k.trim()) => (Some(k.trim().to_string()), v.trim().to_string()),
        _ => (None, spec.trim().to_string()),
    }
}

// ---------------------------------------------------------------- 入口

/// 从程序入口开始干跑（§16.5）。
pub fn trace_program(program: &Program, opts: &TraceOptions) -> Trace {
    let mut sim = Sim::new(program, opts);

    // 入口判定与 reach / topology 一致：`entry` 声明优先，否则 `cell main`。
    let entry = program
        .entry
        .clone()
        .or_else(|| program.cells().find(|c| c.name == "main").map(|c| c.name.clone()));
    let Some(entry_name) = entry else {
        sim.note("没有入口（既无 `entry` 声明也无 `cell main`），未做模拟");
        return sim.finish();
    };
    let Some(cell) = program.cell(&entry_name) else {
        sim.note(format!("入口 Cell `{entry_name}` 未声明，未做模拟"));
        return sim.finish();
    };

    let (env, arg_notes) = sim.entry_env(cell, &opts.entry_args);
    for n in arg_notes {
        sim.note(n);
    }
    let mut inst = Instance::new(cell, env, 0, None);
    sim.run_instance(&mut inst);
    sim.finish()
}

// ---------------------------------------------------------------- 内部状态

/// 环境：别名 / 键名 → **值文本**（不求值）。
type Env = HashMap<String, String>;

/// 一个 group 槽位（`g.W<k>`）的模拟结果。
#[derive(Clone, Debug)]
struct SlotOutcome {
    target: Ident,
    died: bool,
    outputs: Vec<(Ident, String)>,
}

/// 子实例的返回（父侧用来填 `return(...)` 与 sync 解包）。
#[derive(Clone, Debug)]
struct CellOutcome {
    died: bool,
    outputs: Vec<(Ident, String)>,
}

/// 一个正在被模拟的 Cell 实例。
struct Instance<'a> {
    /// `&'a CellDef`：模拟期间程序不可变，且这个引用与 `&mut Sim` 无别名冲突
    /// （所以遍历时可以 `let cell = inst.cell;` 之后照样改 `inst`）。
    cell: &'a CellDef,
    env: Env,
    /// 本实例创建的 group → 按入组顺序排列的槽位结果（`W1` = 第 0 个）。
    groups: BTreeMap<String, Vec<SlotOutcome>>,
    depth: usize,
    /// 派生本实例的 group（画链路时标在 step 上）。
    group_label: Option<String>,
    /// 下一条 step 要补的说明（例如「位于 Rust 块内」）。
    pending_note: Option<String>,
}

impl<'a> Instance<'a> {
    fn new(cell: &'a CellDef, env: Env, depth: usize, group_label: Option<String>) -> Self {
        Instance { cell, env, groups: BTreeMap::new(), depth, group_label, pending_note: None }
    }
}

enum Pass {
    /// 走到终句（exit / die）或 body 自然结束。
    Done(CellOutcome),
    /// `jump loop` 命中：本实例回到 body 顶部重走。
    Restart,
}

/// `when` 条件的判定结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cond {
    Taken,
    Skipped,
    /// 超出可解释范围：按不命中处理，但必须在 step 上留痕。
    Unknown,
}

/// 一条操作数的取值。
#[derive(Clone, Debug, PartialEq)]
enum Val {
    Int(i128),
    Float(f64),
    Bool(bool),
    Str(String),
}

/// 一条 step 的落地参数。
///
/// 为什么用结构体而不是参数列表：`push` 需要 7 个以上的字段（cell 来自 inst），
/// 拆成一串位置参数既会被 clippy `too_many_arguments` 拦，也容易在调用点写错顺序。
struct Draft {
    kind: StepKind,
    text: String,
    line: usize,
    col: usize,
    packet: Vec<PacketField>,
    note: Option<String>,
    /// `None` = 用本实例的 group_label。
    group: Option<String>,
}

struct Sim<'a> {
    program: &'a Program,
    max_items: usize,
    max_steps: usize,
    max_depth: usize,
    steps: Vec<Step>,
    notes: Vec<String>,
    /// 已因 step 上限中止过（同样的原因只报一次）。
    stepped_out: bool,
    extern_blocks: usize,
    uninterpretable: usize,
    unevaluated: usize,
}

impl<'a> Sim<'a> {
    fn new(program: &'a Program, opts: &TraceOptions) -> Self {
        // 下限夹到 1：`--max-items 0` 会让「只走 1 轮 / 只记 1 条派生记录」的兜底
        // 变成「什么都不做」，那是静默降级而不是模拟结果。
        Sim {
            program,
            max_items: opts.max_items.max(1),
            max_steps: opts.max_steps.max(1),
            max_depth: opts.max_depth.max(1),
            steps: Vec::new(),
            notes: Vec::new(),
            stepped_out: false,
            extern_blocks: 0,
            uninterpretable: 0,
            unevaluated: 0,
        }
    }

    fn finish(mut self) -> Trace {
        if self.uninterpretable > 0 {
            self.note(format!(
                "{} 处 `when` 条件超出可解释范围（首版只认 字面量/简单标识符 + == != < > <= >=），已按不命中处理",
                self.uninterpretable
            ));
        }
        if self.unevaluated > 0 {
            self.note(format!("{} 处的值是 Rust 表达式文本，trace 不求值（§16.5：不保证与真实执行数值一致）", self.unevaluated));
        }
        if self.extern_blocks > 0 {
            self.note(format!("{} 处 extern rust 块只计数、不模拟其内部效应", self.extern_blocks));
        }
        Trace { steps: self.steps, notes: self.notes }
    }

    fn note(&mut self, text: impl Into<String>) {
        let text = text.into();
        // 去重：notes 是给 UI 一行行读的，同一条原因重复 20 遍没有信息量。
        if !self.notes.contains(&text) {
            self.notes.push(text);
        }
    }

    // ------------------------------------------------------ 实例执行

    fn run_instance(&mut self, inst: &mut Instance<'_>) -> CellOutcome {
        if inst.depth > self.max_depth {
            self.note(format!("已达递归深度上限 {}（`{}` 实例），链路未走完", self.max_depth, inst.cell.name));
            return CellOutcome { died: false, outputs: Vec::new() };
        }
        let mut rounds = 0usize;
        let mut loop_ok = true;
        loop {
            match self.body_pass(inst, loop_ok) {
                Pass::Done(outcome) => return self.wrap_up(inst, outcome),
                Pass::Restart => {
                    rounds += 1;
                    if rounds >= self.max_items {
                        loop_ok = false;
                        self.note(format!(
                            "`{}` 的 jump loop 已达 --max-items={} 轮上限，后续按退出循环继续画（链路未走完）",
                            inst.cell.name, self.max_items
                        ));
                    }
                }
            }
        }
    }

    /// 末尾非出口的 Cell 要在 notes 里说清楚，不能假装它产出了返回包。
    fn wrap_up(&mut self, inst: &Instance<'_>, outcome: CellOutcome) -> CellOutcome {
        let cell = inst.cell;
        let ends_open = cell
            .body
            .last()
            .map(|s| !matches!(s, Stmt::Exit { .. } | Stmt::Die { .. } | Stmt::Jump(_)))
            .unwrap_or(true);
        if ends_open {
            self.note(format!("`{}` 末尾不是出口语句（fallthrough），本实例无返回包", cell.name));
        }
        outcome
    }

    fn body_pass(&mut self, inst: &mut Instance<'_>, loop_ok: bool) -> Pass {
        let cell = inst.cell;
        for (idx, stmt) in cell.body.iter().enumerate() {
            if self.steps.len() >= self.max_steps {
                if !self.stepped_out {
                    self.stepped_out = true;
                    self.note(format!("已达模拟 step 上限 {}，链路未走完", self.max_steps));
                }
                return Pass::Done(CellOutcome { died: false, outputs: Vec::new() });
            }
            if let Some(pass) = self.stmt_pass(inst, idx, stmt, loop_ok) {
                return pass;
            }
        }
        Pass::Done(CellOutcome { died: false, outputs: Vec::new() })
    }

    /// 处理一条语句；返回 `None` = 「不改变控制流，继续下一条」。
    fn stmt_pass(&mut self, inst: &mut Instance<'_>, idx: usize, stmt: &Stmt, loop_ok: bool) -> Option<Pass> {
        match stmt {
            // 普通 let 透传：不出 step（否则图上刷屏），但名字要进环境供后续引用。
            Stmt::RawRust(r) => {
                if let Some((name, value)) = parse_let(&r.text) {
                    inst.env.insert(name, value);
                }
                None
            }
            Stmt::ExternRustBlock { .. } => {
                self.extern_blocks += 1;
                None
            }
            Stmt::RustBlock { block } => {
                inst.pending_note =
                    Some("位于 Rust 块语句内：外层 if/match 的条件不求值，块内 DSL 语句一律按顺序模拟".to_string());
                let pass = self.block_pass(inst, idx, block, loop_ok);
                // 块里一个 step 都没产出时要把这条说明撤掉：否则它会贴到**下一条**
                // 无关语句上，读 trace 的人以为那条也在块内。
                if pass.is_none() {
                    inst.pending_note = None;
                }
                pass
            }
            Stmt::Out { args, span } => {
                let packet = self.out_packet(args, inst);
                self.push(
                    inst,
                    idx,
                    Draft {
                        kind: StepKind::Out,
                        text: out_text(args),
                        line: span.line as usize,
                        col: span.col as usize,
                        packet,
                        note: None,
                        group: None,
                    },
                );
                None
            }
            Stmt::Sync { group, span } => {
                self.do_sync(inst, idx, group, *span);
                None
            }
            Stmt::Exit { args, span } => {
                let packet = self.args_packet(args, inst);
                let outputs = packet.iter().map(|f| (f.key.clone(), f.value.clone())).collect();
                let (line, col) = (span.line as usize, span.col as usize);
                self.push(
                    inst,
                    idx,
                    Draft { kind: StepKind::Exit, text: exit_text(args), line, col, packet, note: None, group: None },
                );
                Some(Pass::Done(CellOutcome { died: false, outputs }))
            }
            Stmt::Die { span } => {
                let (line, col) = (span.line as usize, span.col as usize);
                self.push(
                    inst,
                    idx,
                    Draft {
                        kind: StepKind::Die,
                        text: "die".to_string(),
                        line,
                        col,
                        packet: Vec::new(),
                        note: Some("本实例销毁、不产出返回包（S3：父侧对应槽位标 died）".to_string()),
                        group: None,
                    },
                );
                Some(Pass::Done(CellOutcome { died: true, outputs: Vec::new() }))
            }
            Stmt::Jump(j) => self.do_jump(inst, idx, j, loop_ok),
        }
    }

    /// 递归 Rust 块里的 DSL 语句（§3.5.2 的嵌套分派）。step 记**块语句**的下标。
    fn block_pass(&mut self, inst: &mut Instance<'_>, idx: usize, block: &RawBlock, loop_ok: bool) -> Option<Pass> {
        for part in &block.parts {
            let RawBlockPart::Stmt(inner) = part else { continue };
            if let Some(pass) = self.stmt_pass(inst, idx, inner, loop_ok) {
                return Some(pass);
            }
        }
        None
    }

    // ------------------------------------------------------ jump 五形态

    fn do_jump(&mut self, inst: &mut Instance<'_>, idx: usize, j: &JumpStmt, loop_ok: bool) -> Option<Pass> {
        let cond = match &j.condition {
            None => Cond::Taken,
            Some(c) => match eval_when(&c.text, &inst.env) {
                Some(true) => Cond::Taken,
                Some(false) => Cond::Skipped,
                None => {
                    self.uninterpretable += 1;
                    Cond::Unknown
                }
            },
        };
        // `when` 判假 = 这条分支根本不走 = 不产生拓扑效应 → 不出 step（§16.5 第 1 条）。
        // 例外：jump loop 的「条件不命中」正是循环结束的现场，不画出来读者就不知道
        // 链路是怎么从循环里出来的。
        if cond == Cond::Skipped && j.kind != JumpKind::Loop {
            return None;
        }
        let (line, col) = (j.span.line as usize, j.span.col as usize);
        match j.kind {
            JumpKind::Plain => {
                let packet = self.args_packet(&j.args, inst);
                let text = format!("{} → {}", jump_text(j), j.targets.join(", "));
                let note = unknown_note(cond);
                self.push(inst, idx, Draft { kind: StepKind::Jump, text, line, col, packet, note, group: None });
                if cond == Cond::Taken {
                    // plain 是**转移**：本实例其实结束了，但 §16.5 要的是「链路画全」，
                    // 所以这里跑完子实例后回到父侧继续走后面的语句。
                    self.spawn_children(inst, j, None);
                }
                None
            }
            JumpKind::Loop => {
                let note = match cond {
                    Cond::Skipped => Some("条件不命中 → 退出循环".to_string()),
                    Cond::Unknown => Some(format!("{UNINTERPRETABLE}：只走 1 轮，按退出循环处理")),
                    Cond::Taken if !loop_ok => Some(format!("已达 --max-items={max} 轮上限，停止迭代（链路未走完）", max = self.max_items)),
                    Cond::Taken => None,
                };
                let packet = self.args_packet(&j.args, inst);
                let text = format!("{} ↻ 回到本 Cell 顶部", jump_text(j));
                self.push(inst, idx, Draft { kind: StepKind::Loop, text, line, col, packet, note, group: None });
                if cond == Cond::Taken && loop_ok {
                    self.apply_loop_bindings(inst, j);
                    return Some(Pass::Restart);
                }
                None
            }
            JumpKind::Recurse => {
                let packet = self.args_packet(&j.args, inst);
                let text = format!("{} ↧ 派生子实例", jump_text(j));
                let note = unknown_note(cond);
                self.push(inst, idx, Draft { kind: StepKind::Recurse, text, line, col, packet, note, group: j.group.clone() });
                if cond != Cond::Taken {
                    return None;
                }
                // §16.5：recurse 的展开层数受 --max-items 约束（真实递归会炸栈）。
                if inst.depth >= self.max_items {
                    self.note(format!(
                        "jump recurse 已达 --max-items={max} 层上限（`{}`），更深的递归未展开",
                        inst.cell.name,
                        max = self.max_items
                    ));
                    return None;
                }
                for slot in self.spawn_children(inst, j, None) {
                    self.record_slot(inst, j, slot);
                }
                None
            }
            JumpKind::And => {
                self.do_and(inst, idx, j, cond);
                None
            }
        }
    }

    /// `jump and`：`over` 是字面数组 → 每元素一个实例；非字面集合 → 记 1 条派生记录。
    fn do_and(&mut self, inst: &mut Instance<'_>, idx: usize, j: &JumpStmt, cond: Cond) {
        let (line, col) = (j.span.line as usize, j.span.col as usize);
        let base = jump_text(j);
        if j.targets.is_empty() {
            self.push(
                inst,
                idx,
                Draft { kind: StepKind::And, text: base, line, col, packet: Vec::new(), note: Some("jump and 没有目标 Cell".to_string()), group: j.group.clone() },
            );
            return;
        }
        let collection_text = j.over.as_ref().map(|o| o.collection.text.trim().to_string());
        let has_ops = j.over.as_ref().map(|o| !o.ops.is_empty()).unwrap_or(false);
        let elements: Option<Vec<String>> = match &collection_text {
            Some(t) => self.literal_collection(t, &inst.env),
            None => None,
        };

        let mut notes: Vec<String> = Vec::new();
        if let Some(n) = unknown_note(cond) {
            notes.push(n);
        }
        if has_ops {
            notes.push("over 管道的 filter/map/flat_map/take/skip 闭包不求值，按原集合逐元素派生".to_string());
        }
        // 实例计划：槽位 k = 第 k 个入组实例（`g.W(k+1)`），目标按 targets 轮转。
        // 多目标 + over 的语义规范 §7 未定义（docs/05 P3），这里明确取
        // 「元素序为主、目标轮转」，并把它写成 step note，让读者知道是模拟约定。
        let mut plan: Vec<(Ident, Option<String>)> = Vec::new();
        if let Some(elems) = &elements {
            let shown = elems.len().min(self.max_items);
            if elems.len() > shown {
                let msg = format!("已截断，实际 {} 个实例（--max-items={max}）", elems.len(), max = self.max_items);
                self.note(format!("`{}`：{base} {msg}", inst.cell.name));
                notes.push(msg);
            }
            if elems.is_empty() {
                notes.push("over 集合是空字面数组 → 不派生实例".to_string());
            }
            if shown > 1 && j.targets.len() > 1 {
                notes.push("多目标 + over：按「元素序为主、目标轮转」模拟（规范 §7 未定义此组合）".to_string());
            }
            plan = (0..shown).map(|i| (j.targets[i % j.targets.len()].clone(), Some(elems[i].clone()))).collect();
        } else if j.over.is_some() {
            notes.push(format!(
                "over 集合 `{}` 不是静态字面数组/区间 → 实例数量运行期未知，只记 1 条派生记录",
                collection_text.as_deref().unwrap_or("")
            ));
            plan.push((j.targets[0].clone(), None));
        } else {
            plan = j.targets.iter().cloned().map(|t| (t, None)).collect();
        }

        let mut packet = self.args_packet(&j.args, inst);
        for (i, element) in plan.iter().filter_map(|(_, e)| e.as_ref()).enumerate() {
            packet.push(PacketField { key: format!("over[{i}]"), value: element.clone(), died: false });
        }
        let total = elements.as_ref().map(|e| e.len()).unwrap_or(plan.len());
        let text = format!("{base} ⇒ 模拟 {} 个实例（总计 {total}）", plan.len());
        self.push(
            inst,
            idx,
            Draft {
                kind: StepKind::And,
                text,
                line,
                col,
                packet,
                note: if notes.is_empty() { None } else { Some(notes.join("；")) },
                group: j.group.clone(),
            },
        );

        if cond != Cond::Taken {
            return;
        }
        for (target, element) in plan {
            for slot in self.spawn_children(inst, j, Some((&target, element.as_deref()))) {
                self.record_slot(inst, j, slot);
            }
        }
    }

    /// 派生并跑完子实例，返回它们的 `SlotOutcome`（目标未声明时返回空并记 note）。
    ///
    /// `single = Some((target, element))` 用于 `jump and`（只跑指定目标）；
    /// `None` 用于 plain / recurse（跑 `j.targets` 的全部目标）。
    fn spawn_children(
        &mut self,
        inst: &mut Instance<'_>,
        j: &JumpStmt,
        single: Option<(&Ident, Option<&str>)>,
    ) -> Vec<SlotOutcome> {
        let mut out = Vec::new();
        let targets: Vec<(Ident, Option<String>)> = match single {
            Some((t, e)) => vec![(t.clone(), e.map(str::to_string))],
            None => j.targets.iter().cloned().map(|t| (t, None)).collect(),
        };
        for (name, element) in targets {
            let Some(cell) = self.program.cell(&name) else {
                self.note(format!("目标 Cell `{name}` 未声明（应由 check 报 E042），跳过其子实例"));
                continue;
            };
            let (cenv, bind_notes) = self.env_for_target(cell, &j.args, element.as_deref(), inst);
            for n in bind_notes {
                self.note(format!("`{name}` 入参：{n}"));
            }
            let mut child = Instance::new(cell, cenv, inst.depth + 1, j.group.clone());
            let outcome = self.run_instance(&mut child);
            out.push(SlotOutcome { target: name, died: outcome.died, outputs: outcome.outputs });
        }
        out
    }

    /// 把子实例结果登记进父侧的 group 槽位；`return(...)` 的别名也在这里绑。
    fn record_slot(&mut self, inst: &mut Instance<'_>, j: &JumpStmt, slot: SlotOutcome) {
        if let Some(g) = &j.group {
            inst.groups.entry(g.clone()).or_default().push(slot.clone());
        }
        let cell = inst.cell;
        let mut alias_conflict = false;
        for (key, alias) in &j.ret_map {
            let value = if slot.died {
                format!("{DIED} {}", slot.target)
            } else {
                slot
                    .outputs
                    .iter()
                    .find(|(k, _)| k == &key.name)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_else(|| format!("(子实例 {} 未产出键 `{}`)", slot.target, key.name))
            };
            // §6.3 / E047 的形态：同一 group 的返回值既走 return(...) 又多包 receive。
            // check 已经报错，这里只提醒读者两处看到的文本会不一样。
            if j.group.is_some() && cell.receive.keys.iter().any(|k| k.pkg.as_deref().is_some_and(|p| parse_slot(p, j.group.as_deref().unwrap_or("")).is_some())) {
                alias_conflict = true;
            }
            inst.env.insert(alias.clone(), value);
        }
        if alias_conflict {
            self.note(format!("`{}`：return(...) 与多包 receive 读同一 group 的返回值（E047 形态）", cell.name));
        }
    }

    /// `jump sync <group>`：解包多包 receive（S4 late binding）。
    fn do_sync(&mut self, inst: &mut Instance<'_>, idx: usize, group: &str, span: Span) {
        let instances: Vec<SlotOutcome> = inst.groups.get(group).cloned().unwrap_or_default();
        let mut notes: Vec<String> = Vec::new();
        let mut packet: Vec<PacketField> = Vec::new();
        let cell = inst.cell;
        for (i, k) in cell.receive.keys.iter().enumerate() {
            let Some(pkg) = k.pkg.as_deref() else { continue };
            let Some(slot_no) = parse_slot(pkg, group) else { continue };
            let alias = cell.receive.aliases.get(i).cloned().unwrap_or_else(|| k.name.clone());
            let (value, died) = match instances.get(slot_no - 1) {
                None => {
                    notes.push(format!("槽位 W{slot_no} 没有已模拟的实例（元素不足或数量运行期未知）→ 取 {D5_DEFAULT}（D5）"));
                    (D5_DEFAULT.to_string(), false)
                }
                Some(s) if s.died => {
                    notes.push(format!("W{slot_no}（`{}`）走了 die → 本键无值（S3）", s.target));
                    (format!("{DIED} {}", s.target), true)
                }
                Some(s) => match s.outputs.iter().find(|(k2, _)| k2 == &k.name) {
                    Some((_, v)) => (v.clone(), false),
                    None => {
                        notes.push(format!("W{slot_no}（`{}`）的返回包里没有键 `{}`", s.target, k.name));
                        (format!("(no key {})", k.name), false)
                    }
                },
            };
            inst.env.insert(alias, value.clone());
            packet.push(PacketField { key: format!("{pkg}.{}", k.name), value, died });
        }
        if instances.is_empty() {
            notes.push(format!("group `{group}` 没有已模拟的实例（本 Cell 未创建它，或创建它的条件未命中）"));
        } else if packet.is_empty() {
            notes.push(format!("group `{group}` 有 {} 个实例，但本 Cell 没有多包 receive 来收 → 结果丢弃", instances.len()));
        }
        self.push(
            inst,
            idx,
            Draft {
                kind: StepKind::Sync,
                text: format!("jump sync {group}"),
                line: span.line as usize,
                col: span.col as usize,
                packet,
                note: if notes.is_empty() { None } else { Some(notes.join("；")) },
                group: Some(group.to_string()),
            },
        );
    }

    /// `jump loop` 的按位置刷新（§9.2 例外：不用键名）。
    fn apply_loop_bindings(&mut self, inst: &mut Instance<'_>, j: &JumpStmt) {
        let cell = inst.cell;
        let mut updates: Vec<(Ident, String)> = Vec::new();
        for (i, a) in j.args.iter().enumerate() {
            let Some(k) = cell.receive.keys.get(i) else {
                self.note(format!("`{}`：jump loop 实参多于 receive 槽位（E041 形态），多余实参忽略", cell.name));
                break;
            };
            let alias = cell.receive.aliases.get(i).cloned().unwrap_or_else(|| k.name.clone());
            let (value, _) = self.value_text(&a.expr.text, inst);
            updates.push((alias, value));
        }
        if j.args.len() != cell.receive.keys.len() {
            self.note(format!(
                "`{}`：jump loop 实参 {} 个 / receive 槽 {} 个不一致（E041 形态）",
                cell.name,
                j.args.len(),
                cell.receive.keys.len()
            ));
        }
        // 先全部算完再写回：`go with(next, next_acc)` 的值来自**本轮**环境，
        // 边写边改会让后面的实参读到刚被刷新的名字（等价于写错序的迭代语义）。
        for (alias, value) in updates {
            inst.env.insert(alias, value);
        }
    }

    // ------------------------------------------------------ 环境与求值

    /// 入口 Cell 的环境：`--arg` 具名优先，其余按 receive 顺序落位。
    fn entry_env(&self, cell: &CellDef, args: &[String]) -> (Env, Vec<String>) {
        let mut env: Env = HashMap::new();
        let mut notes = Vec::new();
        for (i, k) in cell.receive.keys.iter().enumerate() {
            let alias = cell.receive.aliases.get(i).cloned().unwrap_or_else(|| k.name.clone());
            env.insert(alias, UNBOUND.to_string());
        }
        let mut next = 0usize;
        for spec in args {
            let (name, value) = parse_entry_arg(spec);
            match name {
                Some(key) => {
                    let mut hit = false;
                    for (i, k) in cell.receive.keys.iter().enumerate() {
                        let alias = cell.receive.aliases.get(i).cloned().unwrap_or_else(|| k.name.clone());
                        if k.name == key || alias == key {
                            env.insert(alias, value.clone());
                            hit = true;
                        }
                    }
                    if !hit {
                        notes.push(format!("入口参数 `{key}` 在 `{}` 的 receive 里不存在，已忽略", cell.name));
                    }
                }
                None => {
                    let Some((i, k)) = cell.receive.keys.iter().enumerate().nth(next) else {
                        notes.push(format!("入口参数 `{spec}` 没有对应的 receive 槽位，已忽略"));
                        continue;
                    };
                    next += 1;
                    let alias = cell.receive.aliases.get(i).cloned().unwrap_or_else(|| k.name.clone());
                    env.insert(alias, value);
                }
            }
        }
        let mut unbound: Vec<String> = Vec::new();
        for (i, k) in cell.receive.keys.iter().enumerate() {
            let alias = cell.receive.aliases.get(i).cloned().unwrap_or_else(|| k.name.clone());
            if env.get(&alias).map(|v| v.as_str()) == Some(UNBOUND) {
                unbound.push(format!("{alias}（{}）", k.name));
            }
        }
        if !unbound.is_empty() {
            notes.push(format!("入口 `{}` 未传值的 receive 键：{}", cell.name, unbound.join(", ")));
        }
        (env, notes)
    }

    /// 目标 Cell 的入参环境：按**键名**对应 receive 槽（§9.1，不是书写顺序）。
    fn env_for_target(&self, target: &CellDef, args: &[Arg], element: Option<&str>, inst: &Instance<'_>) -> (Env, Vec<String>) {
        let mut env: Env = HashMap::new();
        let mut notes = Vec::new();
        for (i, k) in target.receive.keys.iter().enumerate() {
            let alias = target.receive.aliases.get(i).cloned().unwrap_or_else(|| k.name.clone());
            if let Some(a) = args.iter().find(|a| a.effective_key() == k.name) {
                let (v, _) = self.value_text(&a.expr.text, inst);
                env.insert(alias, v);
            } else if let Some(e) = element {
                // over 的隐式实参：集合元素就是唯一入参（§7.1 展开式 `spawn worker go with(arg)`）。
                env.insert(alias, e.to_string());
                if i > 0 {
                    notes.push(format!("键 `{}` 由 over 元素复用填充（目标有多个 receive 槽，规范未定义此形状）", k.name));
                }
            } else {
                env.insert(alias, UNBOUND.to_string());
                notes.push(format!("键 `{}` 没有对应实参 → {UNBOUND}", k.name));
            }
        }
        (env, notes)
    }

    /// 求「值文本」：标识符查环境，其它一律留表达式原文（trace 不求值 Rust）。
    /// 第二个返回值 = 是否字面量（假者在 step note 里标 [`UNEVALUATED`]）。
    fn value_text(&self, expr: &str, inst: &Instance<'_>) -> (String, bool) {
        let t = expr.trim();
        if literal_of(t).is_some() {
            return (t.to_string(), true);
        }
        if is_plain_ident(t) {
            return match inst.env.get(t) {
                Some(v) => {
                    let is_lit = literal_of(v).is_some();
                    (v.clone(), is_lit)
                }
                None => (format!("{t} {UNBOUND}"), false),
            };
        }
        (t.to_string(), false)
    }

    fn args_packet(&self, args: &[Arg], inst: &Instance<'_>) -> Vec<PacketField> {
        args.iter()
            .map(|a| {
                let key = if a.effective_key().is_empty() { a.expr.text.trim().to_string() } else { a.effective_key() };
                let (value, _) = self.value_text(&a.expr.text, inst);
                PacketField { key, value, died: false }
            })
            .collect()
    }

    fn out_packet(&self, args: &[RawRust], inst: &Instance<'_>) -> Vec<PacketField> {
        args.iter()
            .map(|a| {
                let (value, _) = self.value_text(&a.text, inst);
                PacketField { key: a.text.trim().to_string(), value, died: false }
            })
            .collect()
    }

    /// `over` 集合的字面展开：`[a, b, c]` / `vec![..]` / `(a..b)` / 标识符查环境。
    fn literal_collection(&self, text: &str, env: &Env) -> Option<Vec<String>> {
        let raw = text.trim();
        // 标识符先查环境：`let items = [1, 2, 3];` 之后的 `over items` 同样是字面集合
        // （jump_and_worker 样例就是这个形状）。
        let t = if is_plain_ident(raw) {
            // `?`：标识符查不到环境就是「集合未知」，与下面的形态不匹配是同一个归宿。
            env.get(raw).map(|v| v.trim().to_string())?
        } else {
            raw.to_string()
        };
        if let Some(inner) = t.strip_prefix("vec![").and_then(|s| s.strip_suffix(']')) {
            return Some(split_top_level(inner));
        }
        if let Some(inner) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            return Some(split_top_level(inner));
        }
        if let Some(inner) = t.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
            return range_elements(inner);
        }
        None
    }

    // ------------------------------------------------------ step 落地

    fn push(&mut self, inst: &mut Instance<'_>, idx: usize, draft: Draft) {
        let Draft { kind, text, line, col, packet, note, group } = draft;
        let mut notes: Vec<String> = Vec::new();
        if let Some(n) = note {
            notes.push(n);
        }
        if let Some(p) = inst.pending_note.take() {
            notes.push(p);
        }
        // 值里只要有非字面量文本 → 本步标「不求值」。
        // 为什么在落地点统一判：packet 的 value 全部出自 value_text / sync 解包 /
        // over 元素三条路，与其在三个调用点各提醒一遍（迟早漏一个），不如在唯一
        // 出口判一次。
        if !packet.is_empty() && packet.iter().any(|f| !f.died && literal_of(&f.value).is_none()) {
            self.unevaluated += 1;
            notes.push(format!("值取自表达式文本，不求值 {UNEVALUATED}"));
        }
        let seq = self.steps.len() + 1;
        self.steps.push(Step {
            seq,
            cell: inst.cell.name.clone(),
            stmt: idx,
            line,
            col,
            kind,
            text,
            packet,
            group: group.or_else(|| inst.group_label.clone()),
            note: if notes.is_empty() { None } else { Some(notes.join("；")) },
        });
    }
}

// ---------------------------------------------------------------- 纯函数工具

fn unknown_note(cond: Cond) -> Option<String> {
    match cond {
        Cond::Unknown => Some(format!("{UNINTERPRETABLE}：按不命中处理，不进入目标")),
        _ => None,
    }
}

/// `g1.W2` + group `g1` → `Some(2)`（1 起始的槽位号）；形态不符返回 `None`。
fn parse_slot(pkg: &str, group: &str) -> Option<usize> {
    let mut it = pkg.split('.');
    if it.next()? != group {
        return None;
    }
    let n: usize = it.next()?.strip_prefix('W')?.parse().ok()?;
    if n == 0 {
        None
    } else {
        Some(n)
    }
}

/// 解析 `let <name>[: <ty>] = <expr>;` → `(name, 表达式文本)`。
///
/// 只做**文本级**拆解（trace 不求值 Rust）：按第一个 `=` 切，左端去 `mut` 与
/// `: 类型` 注解。解不出来的形态（`let (a, b) = ..`、`let Ok(x) = ..` 这类模式绑定）
/// 返回 `None` = 不登记，于是后续引用它时显示 `(unbound)`，而不是编一个假名字。
fn parse_let(text: &str) -> Option<(Ident, String)> {
    let body = text.trim().trim_end_matches(';').trim();
    let rest = body.strip_prefix("let")?.trim();
    let (binding, expr) = match rest.split_once('=') {
        Some((l, r)) => (l.trim(), r.trim()),
        // `let x;` 先声明后赋值：登记成占位文本，别让后面的引用凭空消失。
        None => (rest, UNBOUND),
    };
    let binding = binding.strip_prefix("mut ").unwrap_or(binding).trim();
    let name = binding.split(':').next()?.trim();
    if name == "_" || !is_plain_ident(name) {
        return None;
    }
    Some((name.to_string(), expr.to_string()))
}

fn out_text(args: &[RawRust]) -> String {
    let parts: Vec<&str> = args.iter().map(|a| a.text.trim()).collect();
    format!("out({})", parts.join(", "))
}

fn exit_text(args: &[Arg]) -> String {
    format!("exit go with({})", args_text(args))
}

fn args_text(args: &[Arg]) -> String {
    args.iter()
        .map(|a| match &a.key {
            Some(k) => format!("{} -> {k}", a.expr.text.trim()),
            None => a.expr.text.trim().to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// 按 §6.6 的固定段序还原一条 jump 的文本。
fn jump_text(j: &JumpStmt) -> String {
    let mut s = String::from("jump");
    match j.kind {
        JumpKind::Plain => {}
        JumpKind::Loop => s.push_str(" loop"),
        JumpKind::Recurse => s.push_str(" recurse"),
        JumpKind::And => s.push_str(" and"),
    }
    s.push(' ');
    s.push_str(&j.targets.join(", "));
    if let Some(c) = &j.condition {
        s.push_str(&format!(" when {}", c.text.trim()));
    }
    if let Some(m) = mode_text(j.mode) {
        s.push(' ');
        s.push_str(m);
        s.push('(');
        s.push_str(&args_text(&j.args));
        s.push(')');
    } else if !j.args.is_empty() {
        // 没写传输模式的 jump 由 check 报错；这里照原样显示，不替用户补一个 `go with`。
        s.push_str(" (无传输模式: ");
        s.push_str(&args_text(&j.args));
        s.push(')');
    }
    for (k, alias) in &j.ret_map {
        s.push_str(&format!(" return({} -> {alias})", k.name));
    }
    if let Some(o) = &j.over {
        s.push_str(&over_text(o));
    }
    if let Some(l) = &j.concurrent_limit {
        s.push_str(&format!(" concurrent_limit={}", l.text.trim()));
    }
    if let Some(g) = &j.group {
        s.push_str(&format!(" group {g}"));
    }
    s
}

/// `over` 的文本还原。形态与 `topology` 的边标签一致，但那边把它折进了
/// `Edge.over`（一条边只算一次），这里要逐 step 现算，故本地实现一份最小的。
fn over_text(o: &OverSpec) -> String {
    let mut s = format!(" over {}", o.collection.text.trim());
    for op in &o.ops {
        let (name, c) = match op {
            OverOp::Filter(c) => ("filter", c),
            OverOp::Map(c) => ("map", c),
            OverOp::FlatMap(c) => ("flat_map", c),
            OverOp::Take(c) => ("take", c),
            OverOp::Skip(c) => ("skip", c),
        };
        s.push_str(&format!(" |> {name}({})", c.text.trim()));
    }
    s
}

/// 字面量识别：bool / 数字（含 `i64` 之类后缀）/ 字符串 / 字符。
fn literal_of(text: &str) -> Option<Val> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    if t == "true" {
        return Some(Val::Bool(true));
    }
    if t == "false" {
        return Some(Val::Bool(false));
    }
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        return Some(Val::Str(t[1..t.len() - 1].to_string()));
    }
    if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
        return Some(Val::Str(t[1..t.len() - 1].to_string()));
    }
    number_of(t)
}

const NUM_SUFFIXES: &[&str] =
    &["i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize", "f32", "f64"];

fn number_of(t: &str) -> Option<Val> {
    let s: String = t.chars().filter(|c| *c != '_').collect();
    let cut = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
    let (digits, suffix) = s.split_at(cut);
    if digits.is_empty() {
        return None;
    }
    if !suffix.is_empty() && !NUM_SUFFIXES.contains(&suffix) {
        return None;
    }
    if suffix.starts_with('f') || digits.contains('.') || digits.contains('e') || digits.contains('E') {
        return digits.parse::<f64>().ok().map(Val::Float);
    }
    digits.parse::<i128>().ok().map(Val::Int)
}

/// 操作数：字面量，或「简单标识符 → 查环境 → 再把取到的文本当字面量读」。
fn operand(text: &str, env: &Env) -> Option<Val> {
    let t = text.trim();
    if let Some(v) = literal_of(t) {
        return Some(v);
    }
    if is_plain_ident(t) {
        return env.get(t).and_then(|v| literal_of(v));
    }
    None
}

/// `when` 条件的可解释范围（§16.5 首版限定，**边界到这里为止**）：
///
/// - 六种比较 `== != < <= > >=`，两侧各是一个**字面量**或**简单标识符**
///   （标识符再去环境里取一次字面量，所以 `let n: i64 = 5;` 之后的 `n > 1` 能算）；
/// - `!x` 取反与裸布尔 `x`（§5.3 的 `jump b when !cond;` 就是这个形状）；
/// - 其它一律 `None` = 不可解释：函数调用、算术表达式（`cur - 1 > 0`）、
///   `&&` / `||` 复合条件、字段访问、`match`……全部交回调用方标 [`UNINTERPRETABLE`]
///   并按不命中处理。
///
/// 为什么止步于此：再往下就要实现 Rust 的整数溢出 / 类型后缀 / 方法解析语义，
/// 那是「第二个解释器」，且必然与 rustc 的真实结果漂移——§16.5 明示 trace
/// 不保证数值一致，所以**判不了就说判不了**，不猜。
pub fn eval_when(text: &str, env: &Env) -> Option<bool> {
    let t = text.trim();
    if let Some(rest) = t.strip_prefix('!') {
        return match operand(rest, env)? {
            Val::Bool(b) => Some(!b),
            _ => None,
        };
    }
    for op in ["==", "!=", "<=", ">=", "<", ">"] {
        if let Some((l, r)) = t.split_once(op) {
            // 找到了运算符却解不出操作数 = 不可解释，直接 None；
            // 继续试下一个运算符会把 `a <= b` 拆成 `a < = b` 之类的假形状。
            let a = operand(l, env)?;
            let b = operand(r, env)?;
            let ord = compare(&a, &b)?;
            return Some(match op {
                "==" => ord == Ordering::Equal,
                "!=" => ord != Ordering::Equal,
                "<" => ord == Ordering::Less,
                "<=" => ord != Ordering::Greater,
                ">" => ord == Ordering::Greater,
                _ => ord != Ordering::Less,
            });
        }
    }
    match operand(t, env)? {
        Val::Bool(b) => Some(b),
        _ => None,
    }
}

fn compare(a: &Val, b: &Val) -> Option<Ordering> {
    match (a, b) {
        (Val::Int(x), Val::Int(y)) => Some(x.cmp(y)),
        (Val::Int(x), Val::Float(y)) | (Val::Float(y), Val::Int(x)) => (*x as f64).partial_cmp(y),
        (Val::Float(x), Val::Float(y)) => x.partial_cmp(y),
        (Val::Bool(x), Val::Bool(y)) => Some(x.cmp(y)),
        (Val::Str(x), Val::Str(y)) => Some(x.cmp(y)),
        // 混合类型（数字 vs 字符串…）在 Rust 里根本编译不过，判不了 → 不可解释。
        _ => None,
    }
}

/// 数字区间 `(0..10)` / `(1..=3)` 的字面展开（元素文本 = 边界数字）。
/// 解不出（非数字边界 / 没有 `..`）返回 `None` = 数量运行期未知。
fn range_elements(inner: &str) -> Option<Vec<String>> {
    for (marker, inclusive) in [("..=", true), ("..", false)] {
        if let Some(idx) = inner.find(marker) {
            let lo: i128 = inner[..idx].trim().parse().ok()?;
            let hi: i128 = inner[idx + marker.len()..].trim().parse().ok()?;
            if hi < lo {
                return Some(Vec::new());
            }
            let end = if inclusive { hi } else { hi - 1 };
            return Some((lo..=end).map(|n| n.to_string()).collect());
        }
    }
    None
}

/// 按**顶层**逗号切分（忽略嵌套 `[] () {} <>` 与字符串内部的逗号），元素去空白。
fn split_top_level(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    let mut start = 0usize;
    for (i, c) in s.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '[' | '(' | '{' | '<' => depth += 1,
            ']' | ')' | '}' | '>' => depth -= 1,
            ',' if depth == 0 => {
                out.push(s[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(s[start..].trim().to_string());
    out.into_iter().filter(|p| !p.is_empty()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rscel_parser::parse_program;

    fn trace(src: &str, opts: &TraceOptions) -> Trace {
        let parsed = parse_program(src, 0);
        assert!(!parsed.has_errors(), "样例必须能过 parser：{}", render_diags(&parsed));
        trace_program(&parsed.program, opts)
    }

    fn render_diags(parsed: &rscel_parser::ParseOutcome) -> String {
        parsed.diagnostics.iter().map(|d| d.render("t.rscel")).collect::<Vec<_>>().join("\n")
    }

    /// 复用 `rscel-gen` 的 M1/M4 端到端验收样例。
    ///
    /// **为什么不复制一份进 tests/**：那两份文本迟早漂移（P0-1 正在改 jump loop 的
    /// 生成形状），而 trace 要验证的恰恰是「读者在 examples 里看到的那个程序，
    /// 干跑出来长什么样」。这里是**读文件**，不引入 crate 依赖（check → parser 的
    /// 依赖方向不变）。
    fn example(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../rscel-gen/examples")
            .join(format!("{name}.rscel"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("读样例 {} 失败: {e}", path.display()))
    }

    fn count(t: &Trace, kind: StepKind) -> usize {
        t.steps.iter().filter(|s| s.kind == kind).count()
    }

    fn find<'a>(t: &'a Trace, kind: StepKind, cell: &str) -> &'a Step {
        t.steps
            .iter()
            .find(|s| s.kind == kind && s.cell == cell)
            .unwrap_or_else(|| panic!("应有 {} step（cell={}），实际：{:#?}", kind.as_str(), cell, t.steps))
    }

    // ---- 验收 1：factorial_loop —— loop 回边 + exit 的 result 键 + out 的值文本
    #[test]
    fn factorial_loop_sample_traces_loop_back_edge_and_result_packet() {
        let t = trace(&example("factorial_loop"), &TraceOptions::default());

        // 入口 main 的 jump：实参按**键名**落到 fact 的 receive 别名上。
        let jump = find(&t, StepKind::Jump, "main");
        assert_eq!(jump.packet.first().map(|f| (f.key.as_str(), f.value.as_str())), Some(("n", "5")), "five 查环境应得到字面量 5");
        assert_eq!(jump.packet.get(1).map(|f| (f.key.as_str(), f.value.as_str())), Some(("acc", "1i64")));

        let back = find(&t, StepKind::Loop, "fact");
        assert!(back.text.contains("jump loop"), "{}", back.text);
        assert!(back.note.as_deref().unwrap_or("").contains(UNINTERPRETABLE), "next>0 里 next=cur-1 不是字面量 → 必须标注");
        assert!(back.text.contains("回到本 Cell 顶部"), "loop 步要显式画出回边：{}", back.text);

        let out = find(&t, StepKind::Out, "fact");
        let acc = out.packet.iter().find(|f| f.key == "next_acc").expect("out 步骤要带出 next_acc");
        assert_eq!(acc.value, "sum * cur", "值文本 = let 右侧表达式原文（不求值）");
        assert!(out.note.as_deref().unwrap_or("").contains(UNEVALUATED));

        let exit = find(&t, StepKind::Exit, "fact");
        assert!(exit.packet.iter().any(|f| f.key == "result"), "最终 exit 的 packet 必须含 result");
    }

    // ---- 验收 2：jump_and_worker —— 1 条 and + 1 条 sync + 三个多包键
    #[test]
    fn jump_and_worker_sample_yields_one_and_one_sync_with_three_slots() {
        let t = trace(&example("jump_and_worker"), &TraceOptions::default());
        assert_eq!(count(&t, StepKind::And), 1, "一条 jump and 语句只出一个 and step");
        assert_eq!(count(&t, StepKind::Sync), 1);

        let and = find(&t, StepKind::And, "start");
        assert_eq!(and.group.as_deref(), Some("g1"));
        assert!(and.text.contains("over items"), "{}", and.text);
        assert!(and.text.contains("模拟 3 个实例"), "over 的 let items = [1,2,3] 必须按字面数组展开：{}", and.text);

        // 3 个子实例（每个 worker 一句 exit），都带 group=g1。
        let children: Vec<&Step> = t.steps.iter().filter(|s| s.kind == StepKind::Exit && s.cell == "worker").collect();
        assert_eq!(children.len(), 3);
        assert!(children.iter().all(|s| s.group.as_deref() == Some("g1")));

        let sync = find(&t, StepKind::Sync, "start");
        let keys: Vec<&str> = sync.packet.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, vec!["g1.W1.d", "g1.W2.d", "g1.W3.d"], "sync 的 packet 必须是三个多包键");
        assert!(sync.packet.iter().all(|f| !f.died));
        // S4：sync 之后别名 a/b/c 才绑定，值来自子实例的键 d。
        assert!(sync.packet.iter().all(|f| f.value == "x * 2"));
    }

    // ---- 验收 3：die 分支 —— died 标记传到父侧多包键（S3）
    #[test]
    fn die_in_child_marks_parent_multipacket_key() {
        let src = r#"entry main;

cell main;
receive(g1.W1.d, g1.W2.d) as (a, b);
jump and alive, dead go with(1i64 -> x) group g1;
jump sync g1;
exit go with();

cell alive;
receive(x: i64);
exit go with(x -> d);

cell dead;
receive(x: i64);
die;
"#;
        let t = trace(src, &TraceOptions::default());
        assert_eq!(count(&t, StepKind::Die), 1, "dead 实例必须走到 die");
        let sync = find(&t, StepKind::Sync, "main");
        let w1 = sync.packet.iter().find(|f| f.key == "g1.W1.d").expect("W1");
        let w2 = sync.packet.iter().find(|f| f.key == "g1.W2.d").expect("W2");
        assert!(!w1.died, "活着的实例不该被标 died");
        assert!(w2.died, "die 的子实例必须让父侧对应槽位标 died（S3）");
        assert!(w2.value.contains(DIED), "值位置要写明 died：{}", w2.value);
        assert!(sync.note.as_deref().unwrap_or("").contains("S3"), "{:?}", sync.note.as_deref());
    }

    // ---- 验收 4：条件可解释 vs 不可解释
    #[test]
    fn interpretable_condition_fires_and_uninterpretable_does_not_enter_branch() {
        let src = r#"entry main;

cell main;
receive(n: i64);
jump a when n > 1 go with(n -> v);
jump b when f(x) go with(n -> v);
die;

cell a;
receive(v: i64);
exit go with(v -> k);

cell b;
receive(v: i64);
exit go with(v -> k);
"#;
        // n=5 已知 → `n > 1` 可解释且为真 → 进 a。
        let opts = TraceOptions { entry_args: vec!["n=5".to_string()], ..Default::default() };
        let t = trace(src, &opts);
        assert_eq!(count(&t, StepKind::Exit), 1, "只有 a 被进入，b 不能出现在链路里");
        assert_eq!(find(&t, StepKind::Exit, "a").packet.first().map(|f| (f.key.as_str(), f.value.as_str())), Some(("k", "5")));
        assert!(t.steps.iter().all(|s| s.cell != "b"), "不可解释的分支不能被当成真");

        // `f(x)` 超出范围 → 标 (uninterpretable) 且按不命中处理。
        let uninterp = t.steps.iter().find(|s| s.text.contains("f(x)")).expect("jump b 要留一条 step 说明为什么没走");
        assert!(uninterp.note.as_deref().unwrap_or("").contains(UNINTERPRETABLE), "{:?}", uninterp.note.as_deref());
        assert!(t.notes.iter().any(|n| n.contains("超出可解释范围")), "汇总 note 要写明有几处判不了：{:?}", t.notes);

        // 判假的分支连 step 都不出（不产生拓扑效应）。
        let opts_false = TraceOptions { entry_args: vec!["n=0".to_string()], ..Default::default() };
        let t2 = trace(src, &opts_false);
        assert!(t2.steps.iter().all(|s| s.cell != "a"), "n=0 时 n>1 为假，不该进 a");
        assert_eq!(count(&t2, StepKind::Die), 1, "两条都不命中 → 落兜底 die");
    }

    // ---- 验收 5：over 截断
    #[test]
    fn over_collection_is_truncated_at_max_items_with_note() {
        let src = r#"entry main;

cell main;
receive();
let items = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
jump and worker over items group g1;
jump sync g1;
exit go with();

cell worker;
receive(x: i64);
exit go with(x -> d);
"#;
        let t = trace(src, &TraceOptions { max_items: 4, ..Default::default() });
        let instances = t.steps.iter().filter(|s| s.kind == StepKind::Exit && s.cell == "worker").count();
        assert_eq!(instances, 4, "--max-items=4 → 只模拟 4 个实例");
        assert!(t.notes.iter().any(|n| n.contains("已截断") && n.contains("实际 10")), "notes 必须写明截断：{:?}", t.notes);
        let and = find(&t, StepKind::And, "main");
        assert!(and.note.as_deref().unwrap_or("").contains("已截断，实际 10"), "{:?}", and.note.as_deref());
        // 被截断掉的 W5..W10 没有多包 receive 来读，sync 的 packet 只覆盖声明的槽位。
        assert!(and.text.contains("模拟 4 个实例（总计 10）"), "{}", and.text);
    }

    // ---- 补充：jump loop 真的迭代（条件可解释）
    #[test]
    fn interpretable_loop_condition_iterates_then_stops_when_uninterpretable() {
        let src = r#"entry main;

cell main;
receive();
jump acc go with(0i64 -> i, 0i64 -> s);
exit go with();

cell acc;
receive(i: i64, s: i64) as (cur, tot);
jump loop acc when cur < 3 go with(cur + 1, tot + cur);
exit go with();
"#;
        let t = trace(src, &TraceOptions::default());
        let loops: Vec<&Step> = t.steps.iter().filter(|s| s.kind == StepKind::Loop).collect();
        assert_eq!(loops.len(), 2, "第 1 轮 cur=0<3 可解释为真 → 迭代；第 2 轮 cur=cur+1 判不了 → 停");
        assert!(loops[0].note.as_deref().unwrap_or("").contains(UNEVALUATED), "按位置刷新的值都是表达式文本");
        assert!(!loops[0].note.as_deref().unwrap_or("").contains(UNINTERPRETABLE));
        assert!(loops[1].note.as_deref().unwrap_or("").contains(UNINTERPRETABLE));
    }

    // ---- 补充：槽位 > 实例 → D5 默认值，且明确标注（不静默）
    #[test]
    fn missing_group_slot_uses_d5_default_and_says_so() {
        let src = r#"entry main;

cell main;
receive(g1.W1.d, g1.W2.d, g1.W3.d) as (a, b, c);
let items = [1, 2];
jump and worker over items group g1;
jump sync g1;
exit go with();

cell worker;
receive(x: i64);
exit go with(x -> d);
"#;
        let t = trace(src, &TraceOptions::default());
        let sync = find(&t, StepKind::Sync, "main");
        assert_eq!(sync.packet.len(), 3);
        assert_eq!(sync.packet[0].value, "1");
        assert_eq!(sync.packet[2].value, D5_DEFAULT);
        assert!(!sync.packet[2].died, "缺槽位不是 died，是 D5 默认值");
        assert!(sync.note.as_deref().unwrap_or("").contains("D5"), "{:?}", sync.note.as_deref());
        // 父侧的别名在 sync 之后才绑定（S4 late binding），后面引用 c 就能查到这条文本。
        assert_eq!(sync.cell, "main");
    }

    // ---- 补充：step 上限触顶必须明说，不静默截断
    #[test]
    fn step_cap_is_reported_not_silently_truncated() {
        let t = trace(&example("factorial_loop"), &TraceOptions { max_steps: 2, ..Default::default() });
        assert!(t.steps.len() <= 2);
        assert!(t.notes.iter().any(|n| n.contains("已达模拟 step 上限")), "{:?}", t.notes);
    }

    // ---- 补充：入口没有 --arg 时不编造值
    #[test]
    fn unsupplied_entry_args_show_as_unbound() {
        let src = r#"entry main;

cell main;
receive(mode: i64);
jump a when mode == 1 go with(mode -> v);
die;

cell a;
receive(v: i64);
exit go with();
"#;
        let t = trace(src, &TraceOptions::default());
        assert!(t.notes.iter().any(|n| n.contains("未传值")), "{:?}", t.notes);
        let jump = t.steps.iter().find(|s| s.kind == StepKind::Jump).expect("mode 未传值 → 条件判不了 → 仍要留痕");
        assert!(jump.note.as_deref().unwrap_or("").contains(UNINTERPRETABLE));
        assert_eq!(count(&t, StepKind::Die), 1);
    }

    // ---- 工具函数的直接单测
    #[test]
    fn parse_let_records_only_the_name_and_rhs_text() {
        assert_eq!(parse_let("let five: i64 = 5;"), Some(("five".to_string(), "5".to_string())));
        assert_eq!(parse_let("let mut r: Option<i64> = None;"), Some(("r".to_string(), "None".to_string())));
        assert_eq!(parse_let("let next = cur - 1;"), Some(("next".to_string(), "cur - 1".to_string())));
        // 按第一个 `=` 切，右边里的比较运算符不能被吃掉。
        assert_eq!(parse_let("let eq = a == b;"), Some(("eq".to_string(), "a == b".to_string())));
        // 模式绑定不硬编一个假名字。
        assert_eq!(parse_let("let (a, b) = f();"), None);
        assert_eq!(parse_let("out(x);"), None);
    }

    #[test]
    fn eval_when_only_covers_the_documented_first_version_scope() {
        let mut env: Env = HashMap::new();
        env.insert("n".to_string(), "5".to_string());
        env.insert("flag".to_string(), "true".to_string());
        assert_eq!(eval_when("n > 1", &env), Some(true));
        assert_eq!(eval_when("n <= 5", &env), Some(true));
        assert_eq!(eval_when("n != 5", &env), Some(false));
        assert_eq!(eval_when("flag", &env), Some(true));
        assert_eq!(eval_when("!flag", &env), Some(false));
        assert_eq!(eval_when("1i64 == 1", &env), Some(true));
        assert_eq!(eval_when("2.5 > 1.0", &env), Some(true));
        // 越界的形状一律 None（= 不可解释），绝不猜。
        assert_eq!(eval_when("cur - 1 > 0", &env), None);
        assert_eq!(eval_when("f(x)", &env), None);
        assert_eq!(eval_when("n > 1 && flag", &env), None);
        assert_eq!(eval_when("unknown > 1", &env), None);
        assert_eq!(eval_when("n > 1 && flag", &env), None);
        assert_eq!(eval_when("", &env), None);
    }

    #[test]
    fn literal_collection_covers_arrays_vecs_ranges_and_env_lookup() {
        let parsed = parse_program("cell main;\nreceive();\ndie;\n", 0);
        let sim = Sim::new(&parsed.program, &TraceOptions::default());
        let mut env: Env = HashMap::new();
        env.insert("items".to_string(), "[1, 2, 3]".to_string());
        assert_eq!(sim.literal_collection("[a, b]", &env), Some(vec!["a".to_string(), "b".to_string()]));
        assert_eq!(sim.literal_collection("items", &env), Some(vec!["1".to_string(), "2".to_string(), "3".to_string()]));
        assert_eq!(sim.literal_collection("vec![1, 2]", &env), Some(vec!["1".to_string(), "2".to_string()]));
        assert_eq!(sim.literal_collection("(1..=3)", &env), Some(vec!["1".to_string(), "2".to_string(), "3".to_string()]));
        assert_eq!(sim.literal_collection("(0..2)", &env), Some(vec!["0".to_string(), "1".to_string()]));
        assert_eq!(sim.literal_collection("[]", &env), Some(Vec::new()));
        // 运行期才知道的集合：老实返回 None。
        assert_eq!(sim.literal_collection("files.collect()", &env), None);
        assert_eq!(sim.literal_collection("unknown_ident", &env), None);
        // 元素里的逗号不能被当成分隔符。
        assert_eq!(sim.literal_collection("[\"a,b\", \"c\"]", &env), Some(vec!["\"a,b\"".to_string(), "\"c\"".to_string()]));
    }

    #[test]
    fn recurse_binds_return_alias_from_child_output() {
        let src = r#"entry main;

cell main;
receive();
let mut r: Option<i64> = None;
jump recurse fact go with(5i64 -> n) return(product -> r) group r0;
jump sync r0;
exit go with();

cell fact;
receive(n: i64);
exit go with(n * 2 -> product);
"#;
        let t = trace(src, &TraceOptions::default());
        assert_eq!(count(&t, StepKind::Recurse), 1);
        let child = find(&t, StepKind::Exit, "fact");
        assert_eq!(child.group.as_deref(), Some("r0"), "recurse 的子实例要记 group");
        assert_eq!(child.packet.first().map(|f| (f.key.as_str(), f.value.as_str())), Some(("product", "n * 2")));
        // 父侧 sync 收的是 r0 的实例；`return` 的别名 r 已被绑进父环境（这里通过
        // 后续没有 (unbound) 抱怨来体现 —— 环境本身不进契约，故只断言链路形状）。
        assert_eq!(count(&t, StepKind::Sync), 1);
    }

    #[test]
    fn fallthrough_cell_is_reported_in_notes() {
        let src = r#"entry main;

cell main;
receive();
out(1);
"#;
        let t = trace(src, &TraceOptions::default());
        assert!(t.notes.iter().any(|n| n.contains("fallthrough")), "{:?}", t.notes);
    }
}
