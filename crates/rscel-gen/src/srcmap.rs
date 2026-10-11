//! 源映射表（§12.9 `.map.json`）+ 本仓库自带的极简 JSON 读写。
//!
//! 为什么需要它（§15.2）：`rscel-gen` 产出的 `.rs` 是**编译器写的**，用户看不到也不该看，
//! rustc 报的 `app.rs:312:24` 对 DSL 作者毫无意义。所以生成时必须同步记下
//! 「生成 Rust 的第几行 ← DSL 的哪个 Cell 第几条语句」，`rscel-cli` 再据此把 rustc 诊断
//! 反映射成 `cell fact @ 7:5`。
//!
//! 映射粒度（**本批次的设计裁定**）：**一条 DSL 语句 ↔ 一段生成 Rust 行区间**
//! （`rs_line_start..=rs_line_end`），不做逐 token 映射。理由：
//! - codegen 的语义生成路径（`emit_jump` / `emit_sync` / 组派生 / read with 提升）
//!   一条 DSL 语句会展开成任意多行、且顺序与源文本无关（例如 `promote_read_with`
//!   会先插 `Arc::new` 提升行）。逐 token 映射要求每个 `write!` 都上报位置，
//!   等于重写整个 codegen；而行区间只需要「调用 `emit_stmt` 前后各数一次行数」，
//!   对语义路径零改动，且对 rustc 诊断定位来说精度已经足够（诊断必然落在某条语句的区间里）。
//! - 代价：区间内的具体列不再精确，故 `rs_col` 固定为 1（生成行都有 4 空格缩进，
//!   缩进不是语义列）；需要精确列时看 rustc 自己给的 `column_start`。
//! - 代价（**已知精度边界，不是 bug**）：`jump loop` 会把 Cell 体内前几条 `let X = ...;`
//!   的声明接管进 `loop { ... }` 重算（见 `codegen::jump_loop_consumed_let_names`），
//!   于是那些 `let` 的生成行落在 **jump 语句**的区间里，而不是它们自己的语句里
//!   （它们自己不产出任何行 → 按 `SrcMapBuilder::push` 的规则不记条目）。
//!   举例：`factorial_loop.rscel` 里 `let next = cur - 1;`（DSL 第 14 行）生成在
//!   `cell_fact` 的 loop 体内，报错时定位到的是 DSL 第 16 行的 `jump loop fact when ...`。
//!   这是「一条语句 ↔ 一段区间」粒度的必然结果；要改就得让 codegen 在 loop 体内
//!   逐语句回报行号，属于改造生成器，本批次不动它（规约：语义路径由 252 个测试守着）。
//!
//! 行号约定（**容易踩的坑，写死在这里**）：
//! - 生成 Rust 的 `rs_line_*` **从 1 起始**，与 rustc 报的行号同一坐标系（rustc 也是 1-based）；
//! - DSL 的 `line`/`col` 同样 1 起始、列按字节计（见 `rscel-parser/src/span.rs` 的约定）；
//! - `dsl.stmt` 是 `CellDef::body` 的**下标，从 0 起始**，用顶层 `dsl_stmt_base: 0` 显式声明，
//!   避免 §12.9 样例里 `"stmt": 3` 的 0/1 起始歧义。
//!
//! 为什么 JSON 要手写：项目规约 §6.2「零外部依赖」，`std` 里没有 JSON。
//! 本模块因此自带一个够用的 JSON 值解析器（[`Json`] + [`parse_json`]），
//! `rscel-cli` 解析 rustc 的 `--error-format=json` NDJSON 也复用它（见该 crate 的 `errmap.rs`），
//! 避免两棵树各自长出一个 JSON。数值统一按 `f64` 存（行号/列号量级远小于 2^53，无精度问题）。

use rscel_parser::{RawBlock, RawBlockPart, Span, Stmt};
use std::fmt::Write as _;

/// `.map.json` 的格式版本（§12.9 要求字段名 `version`）。
pub const SRCMAP_VERSION: u32 = 1;

/// 生成文件名的缺省值（`generate()` 拿不到输出路径时用；CLI 走 `generate_as` 传真名）。
pub const DEFAULT_FILE_NAME: &str = "generated.rs";

// ------------------------------------------------------------------ 映射条目

/// 一条映射的来源类型。
///
/// 为什么要区分：§12.9 只规定了「语句级」映射，但错误同样会落在
/// Ret 结构体、Cell 函数头、`fn main`、顶层 extern rust 上。
/// 有了 `kind`，这些区间也能各记一条（`dsl.stmt` 为 `null`），
/// 而不是统统掉进「查不到映射」的黑洞里。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapKind {
    /// Cell 体内的一条 DSL 语句（`dsl.stmt` = body 下标）。
    Stmt,
    /// Cell 的函数头 `fn cell_<name>(..) -> Option<Ret<Name>> {`。
    CellHead,
    /// 该 Cell 的 `Ret_<Name>` 结构体（含 `#[derive(Default)]`）。
    RetStruct,
    /// 生成的 `fn main`（argv 采集 + 入口调用）。
    Main,
    /// 顶层 `extern rust {}` / 顶层 Rust item（§3.5.4：隐含 extern）。
    GlobalExtern,
}

impl MapKind {
    /// JSON 里的稳定字符串形式。
    pub fn as_str(self) -> &'static str {
        match self {
            MapKind::Stmt => "stmt",
            MapKind::CellHead => "cell_head",
            MapKind::RetStruct => "ret_struct",
            MapKind::Main => "main",
            MapKind::GlobalExtern => "global_extern",
        }
    }

    /// 反解析（未知 kind 视为错误，绝不静默降级成 `Stmt`）。
    fn parse(s: &str) -> Result<MapKind, String> {
        match s {
            "stmt" => Ok(MapKind::Stmt),
            "cell_head" => Ok(MapKind::CellHead),
            "ret_struct" => Ok(MapKind::RetStruct),
            "main" => Ok(MapKind::Main),
            "global_extern" => Ok(MapKind::GlobalExtern),
            other => Err(format!("未知 map kind: {other:?}")),
        }
    }
}

/// DSL 侧位置。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct DslPos {
    /// Cell 名；顶层 extern rust 区间没有 Cell，用空串。
    pub cell: String,
    /// `CellDef::body` 下标（0 起始）；非语句级条目为 `None`（JSON 里写 `null`）。
    pub stmt: Option<usize>,
    /// DSL 源行（1 起始）。0 表示「AST 没有位置信息」，反映射时会退回原始输出。
    pub line: u32,
    /// DSL 源列（1 起始，按字节）。
    pub col: u32,
}

impl DslPos {
    /// 语句级条目。
    pub fn stmt(cell: &str, idx: usize, span: Span) -> Self {
        DslPos { cell: cell.to_string(), stmt: Some(idx), line: span.line, col: span.col }
    }

    /// 非语句级条目（Cell 头 / Ret 结构体 / main / 顶层 extern）。
    pub fn item(cell: &str, span: Span) -> Self {
        DslPos { cell: cell.to_string(), stmt: None, line: span.line, col: span.col }
    }

    /// 是否有可用的 DSL 位置（`line == 0` 时 rustc 坐标系对不上，不该做反映射）。
    pub fn has_position(&self) -> bool {
        self.line != 0
    }
}

/// 一条映射：生成 Rust 的行区间 ↔ DSL 位置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MapEntry {
    /// 生成 Rust 起始行（1 起始，含）。
    pub rs_line_start: usize,
    /// 生成 Rust 结束行（1 起始，含）。零行区间不会出现：至少占一行。
    pub rs_line_end: usize,
    /// 起始列（恒为 1，见模块注释）。保留字段是为了兼容 §12.9 的形状。
    pub rs_col: u32,
    /// 条目类型。
    pub kind: MapKind,
    /// 是否位于 extern rust（纯用户 Rust）区间内 —— §15.2 的 RSC-E901 判据。
    pub in_extern: bool,
    /// DSL 侧位置。
    pub dsl: DslPos,
}

impl MapEntry {
    /// 生成的行号是否落在本区间内。
    pub fn covers(&self, rs_line: usize) -> bool {
        self.rs_line_start <= rs_line && rs_line <= self.rs_line_end
    }
}

/// Cell 名 → DSL 定义位置（§12.9 的 `cells` 表）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellPos {
    /// DSL 定义行（`cell <name>;` 所在行，1 起始）。
    pub line: u32,
    /// DSL 定义列（1 起始）。
    pub col: u32,
}

/// 完整源映射表。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SrcMap {
    /// 格式版本。
    pub version: u32,
    /// 生成文件名（`.rs` 的 basename；rustc 的 `file_name` 以它结尾）。
    pub file: String,
    /// 文件头部「编译运行时」占用的行数（`#![allow]` + 内嵌 runtime + 空行）。
    ///
    /// 为什么要记：这段是**编译器自身生成**的、没有任何 DSL 对应物，
    /// 落在里面的诊断必须走「未映射」分支原样输出（§15.2 第 3 条的另一半）。
    pub runtime_lines: usize,
    /// Cell 名 → DSL 定义位置（保持声明顺序）。
    pub cells: Vec<(String, CellPos)>,
    /// 映射条目，按 `rs_line_start` 升序、且**互不重叠**（同区间覆盖 Cell 体的构造保证）。
    pub entries: Vec<MapEntry>,
}

impl SrcMap {
    /// 查生成 Rust 第 `rs_line` 行属于哪条映射（线性扫描）。
    ///
    /// 条目按构造是升序且不重叠的，理论上可二分；但一次构建只有几十~几百条，
    /// 且 rustc 诊断条数更小，二分带来的索引维护成本不划算。
    pub fn lookup(&self, rs_line: usize) -> Option<&MapEntry> {
        self.entries.iter().find(|e| e.covers(rs_line))
    }

    /// Cell 名 → DSL 定义位置。
    pub fn cell_pos(&self, name: &str) -> Option<CellPos> {
        self.cells.iter().find(|(n, _)| n == name).map(|(_, p)| *p)
    }

    /// 序列化为 `.map.json` 文本（§12.9 形状 + 向后兼容的键名）。
    pub fn to_json(&self) -> String {
        let mut s = String::new();
        s.push_str("{\n");
        writeln!(&mut s, "  \"version\": {},", self.version).unwrap();
        writeln!(&mut s, "  \"file\": {},", json_str(&self.file)).unwrap();
        // 1-based 声明：所有 rs_line_* / dsl.line / dsl.col 都是 1 起始，只有 dsl.stmt 是 0 起始。
        s.push_str("  \"rs_line_base\": 1,\n");
        s.push_str("  \"dsl_line_base\": 1,\n");
        s.push_str("  \"dsl_stmt_base\": 0,\n");
        writeln!(&mut s, "  \"runtime_lines\": {},", self.runtime_lines).unwrap();

        s.push_str("  \"cells\": {");
        for (i, (name, pos)) in self.cells.iter().enumerate() {
            if i == 0 {
                s.push('\n');
            }
            writeln!(
                &mut s,
                "    {}: {{ \"line\": {}, \"col\": {} }}{}",
                json_str(name),
                pos.line,
                pos.col,
                if i + 1 == self.cells.len() { "" } else { "," }
            )
            .unwrap();
        }
        if !self.cells.is_empty() {
            s.push_str("  },\n");
        } else {
            s.push_str(" },\n");
        }

        s.push_str("  \"map\": [");
        for (i, e) in self.entries.iter().enumerate() {
            if i == 0 {
                s.push('\n');
            }
            // `rs_line` 是 §12.9 的原字段（= 区间起点），与 `rs_line_start` 同时写出，
            // 保证只读旧字段的消费方仍可用。
            let stmt = match e.dsl.stmt {
                Some(idx) => idx.to_string(),
                None => "null".to_string(),
            };
            writeln!(
                &mut s,
                "    {{ \"rs_line\": {start}, \"rs_line_start\": {start}, \"rs_line_end\": {end}, \
                 \"rs_col\": {col}, \"kind\": {kind}, \"in_extern\": {ext}, \
                 \"dsl\": {{ \"cell\": {cell}, \"stmt\": {stmt}, \"line\": {line}, \"col\": {pcol} }} }}{tail}",
                start = e.rs_line_start,
                end = e.rs_line_end,
                col = e.rs_col,
                kind = json_str(e.kind.as_str()),
                ext = e.in_extern,
                cell = json_str(&e.dsl.cell),
                stmt = stmt,
                line = e.dsl.line,
                pcol = e.dsl.col,
                tail = if i + 1 == self.entries.len() { "" } else { "," },
            )
            .unwrap();
        }
        if !self.entries.is_empty() {
            s.push_str("  ]\n");
        } else {
            s.push_str(" ]\n");
        }
        s.push_str("}\n");
        s
    }
}

/// 构建期累加器：codegen 只做「数行数 + 追加条目」这两件事。
#[derive(Clone, Debug)]
pub struct SrcMapBuilder {
    inner: SrcMap,
}

impl SrcMapBuilder {
    /// 以生成文件名建一个空累加器。
    pub fn new(file_name: &str) -> Self {
        SrcMapBuilder {
            inner: SrcMap {
                version: SRCMAP_VERSION,
                file: file_name.to_string(),
                runtime_lines: 0,
                cells: Vec::new(),
                entries: Vec::new(),
            },
        }
    }

    /// 记录「运行时头部」占用的行数（划定未映射区间的右边界）。
    pub fn set_runtime_lines(&mut self, n: usize) {
        self.inner.runtime_lines = n;
    }

    /// 登记 Cell 名 → DSL 定义位置（§12.9 的 `cells` 表）。
    ///
    /// 同名 Cell 重复登记时保留第一次（parser 允许重复声明，重名由 check 层报错；
    /// 映射表不该因为一个注定失败的程序而丢信息）。
    pub fn register_cell(&mut self, name: &str, span: Span) {
        if self.inner.cell_pos(name).is_some() {
            return;
        }
        self.inner.cells.push((name.to_string(), CellPos { line: span.line, col: span.col }));
    }

    /// 追加一条映射；**空区间（该构造没写任何行）直接不记**。
    ///
    /// 为什么不能夹成 `[start, start]`：被 `jump loop` 抑制的 `let` 一条行都不产出，
    /// 若强行记成一行，它就会宣称覆盖「下一条语句的第一行」，
    /// 于是反映射把错误归到错误的语句上，也破坏「条目互不重叠」这条不变式。
    pub fn push(&mut self, kind: MapKind, dsl: DslPos, in_extern: bool, start: usize, end: usize) {
        if end < start {
            return;
        }
        let entry = MapEntry {
            rs_line_start: start,
            rs_line_end: end,
            rs_col: 1,
            kind,
            in_extern,
            dsl,
        };
        // 按 `rs_line_start` **有序插入**，而不是追加。
        //
        // 为什么不能只 append：候选 1 的实体展开里，被调方的条目是在 `emit_stmt` 内部
        // 记的，而调用方那条 jump 自己的条目（展开块的**头**，行号更小）要等 `emit_stmt`
        // 返回、知道块尾在哪才能算出区间。追加会让表失去升序，而「升序 + 互不重叠」是
        // §12.9 的既有不变式（`srcmap_test` 逐条验，消费方也按这个前提读）。
        // 相等起点的条目保持相对顺序（`pos` 落在所有 `<=` 之后），行为对既有路径不变。
        let pos = self
            .inner
            .entries
            .iter()
            .rposition(|e| e.rs_line_start <= start)
            .map_or(0, |i| i + 1);
        self.inner.entries.insert(pos, entry);
    }

    /// 收工，交出映射表。
    pub fn build(self) -> SrcMap {
        self.inner
    }
}

// ------------------------------------------------------------------ 行号插桩工具

/// `out` 里已经写完的行数（= 已写出的 `\n` 个数）。
///
/// **为什么按 `\n` 计数就够**：codegen 全部用 `write!`/`writeln!` 追加，
/// 唯一的换行来源就是真实写出的 `\n` 字节；生成物里的字符串字面量 containing `\n`
/// 是两字符转义序列（不是真换行），rustc 数行时同样忽略它 —— 两边坐标系天然一致。
pub fn written_lines(out: &str) -> usize {
    out.bytes().filter(|b| *b == b'\n').count()
}

/// 下一条将要写入的行的 1-based 行号。
pub fn next_line(out: &str) -> usize {
    written_lines(out) + 1
}

// ------------------------------------------------------------------ in_extern 判定

/// 该 DSL 语句是否「纯用户 Rust」(§15.2 的 RSC-E901 判据)。
pub fn extern_flag_for_stmt(stmt: &Stmt) -> bool {
    extern_scan_stmt(stmt) == ExternScan::ExternOnly
}

/// 块/语句的 extern 扫描结果。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ExternScan {
    /// 没有任何 extern rust 内容。
    NoExtern,
    /// 其中的 DSL 构造**只有** extern rust 块（其余全是透传文本）。
    ExternOnly,
    /// 既有 extern rust 又有别的 DSL 构造（jump/exit/out/die…）。
    Mixed,
}

/// 递归扫描语句，判定是否 extern-only。
///
/// 为什么 `Mixed` 判 `false`（这是对任务书「RustBlock 里透传文本的部分也算 extern」的
/// 有意收窄）：一个块里只要出现了真正的 DSL 构造，它生成的那几行就是**编译器代码**，
/// 反映射到 DSL 位置比 RSC-E901 有用；而块内透传文本本身走 §10.3 的别名重写
/// （`cur` → `v_a`），和 Cell 体内一条 `let` 没有本质区别 —— 把纯透传语句
/// （`Stmt::RawRust`）也算成 extern 会让最常见的「Cell 体内 Rust 类型写错」
/// 这类错误丢掉 DSL 位置，得不偿失。
/// 因此只有「整段都是 extern rust（可能裹在块文本里）」才标 extern。
fn extern_scan_stmt(stmt: &Stmt) -> ExternScan {
    match stmt {
        Stmt::ExternRustBlock { .. } => ExternScan::ExternOnly,
        Stmt::RustBlock { block } => extern_scan_block(block),
        _ => ExternScan::NoExtern,
    }
}

fn extern_scan_block(block: &RawBlock) -> ExternScan {
    let mut seen_extern = false;
    for part in &block.parts {
        match part {
            // 透传文本（`if cond ` / ` else `）不改变判定。
            RawBlockPart::Text(_) => {}
            RawBlockPart::Stmt(s) => match extern_scan_stmt(s) {
                ExternScan::NoExtern => return ExternScan::Mixed,
                ExternScan::ExternOnly => seen_extern = true,
                ExternScan::Mixed => return ExternScan::Mixed,
            },
        }
    }
    if seen_extern {
        ExternScan::ExternOnly
    } else {
        ExternScan::NoExtern
    }
}

/// `Stmt` 的 DSL 位置（§8.1 的每个变体都带 span，但字段名/嵌套层级不一，集中在这里取）。
pub fn span_of_stmt(stmt: &Stmt) -> Span {
    match stmt {
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

// ------------------------------------------------------------------ JSON 序列化

/// 手写 JSON 字符串字面量：转义 `\\`、`\"`、控制字符，其余（含 UTF-8 中文）原样输出。
///
/// 为什么够用：本模块写出的字符串只有 Cell 名 / 文件名 / kind 这类简单标识符，
/// 但 codegen 的 `in_extern` 区间可能把用户 Rust 文本带进 `file` 之外的字段吗？
/// 不会 —— 只有上述三类。仍按标准转义，避免以后扩字段时踩坑。
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ------------------------------------------------------------------ JSON 解析（读侧）

/// 一个 JSON 值。
///
/// 数值统一用 `f64` 承载（`std` 的 `parse::<f64>` 能吃下整数与小数两种写法），
/// 取整用 [`Json::as_usize`]；对象保留插入顺序（`Vec<(String, Json)>`），
/// 这样 `cells` 表读回来仍是 DSL 声明顺序。
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    /// `null`。
    Null,
    /// `true` / `false`。
    Bool(bool),
    /// 数字。
    Num(f64),
    /// 字符串。
    Str(String),
    /// 数组。
    Arr(Vec<Json>),
    /// 对象。
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// 按整数取（非整数/负数/非数字返回 `None`）。
    pub fn as_usize(&self) -> Option<usize> {
        match self {
            Json::Num(n) if *n >= 0.0 && n.fract() == 0.0 => Some(*n as usize),
            _ => None,
        }
    }

    /// 按 `u32` 取。
    pub fn as_u32(&self) -> Option<u32> {
        self.as_usize().and_then(|v| u32::try_from(v).ok())
    }

    /// 按布尔取。
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// 按字符串取。
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    /// 数组的元素视图。
    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(v) => Some(v),
            _ => None,
        }
    }

    /// 对象按键取值。
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(kv) => kv.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// 可选整数：`null`/缺失/非数字都返回 `None`（`dsl.stmt` 允许 `null`）。
    pub fn as_usize_opt(&self) -> Option<usize> {
        match self {
            Json::Null => None,
            _ => self.as_usize(),
        }
    }
}

/// 解析一个完整 JSON 值（跳过前后空白，允许可选的尾随空白）。
pub fn parse_json(text: &str) -> Result<Json, String> {
    let mut p = Parser { b: text.as_bytes(), i: 0 };
    p.ws();
    let v = p.value()?;
    p.ws();
    if p.i != p.b.len() {
        return Err(format!("JSON 第 {} 字节后有残留内容", p.i));
    }
    Ok(v)
}

/// 把 `.map.json` 文本读回 [`SrcMap`]。
///
/// 为什么 CLI 要先序列化再反序列化一遍（而不是直接把 builder 的产物传下去）：
/// 落盘的 `.map.json` 和反映射用的映射表必须是**同一份数据**，
/// 用「写出 -> 读回」这条路，任何一边格式漂移都会在构建时立刻失败，
/// 而不是等到用户碰到 rustc 报错时才发现表是坏的。
pub fn parse_srcmap_json(text: &str) -> Result<SrcMap, String> {
    let root = parse_json(text)?;
    let version = root.get("version").and_then(Json::as_u32).ok_or("缺 version（u32）")?;
    let file = root.get("file").and_then(Json::as_str).unwrap_or("").to_string();
    let runtime_lines = root.get("runtime_lines").and_then(Json::as_usize).unwrap_or(0);

    let mut cells = Vec::new();
    if let Some(Json::Obj(kv)) = root.get("cells") {
        for (name, pos) in kv {
            let line = pos.get("line").and_then(Json::as_u32).ok_or_else(|| {
                format!("cells[{name:?}] 缺 line")
            })?;
            let col = pos.get("col").and_then(Json::as_u32).ok_or_else(|| {
                format!("cells[{name:?}] 缺 col")
            })?;
            cells.push((name.clone(), CellPos { line, col }));
        }
    }

    let mut entries = Vec::new();
    match root.get("map") {
        Some(Json::Arr(items)) => {
            for it in items {
                let start = it.get("rs_line_start").and_then(Json::as_usize).or_else(|| it.get("rs_line").and_then(Json::as_usize)).ok_or("map 项缺 rs_line_start/rs_line")?;
                let end = it.get("rs_line_end").and_then(Json::as_usize).unwrap_or(start);
                let col = it.get("rs_col").and_then(Json::as_u32).unwrap_or(1);
                let kind = it
                    .get("kind")
                    .and_then(Json::as_str)
                    .map(MapKind::parse)
                    .transpose()?
                    // 旧版（§12.9 原形状）没有 kind：按语句级条目处理。
                    .unwrap_or(MapKind::Stmt);
                let in_extern = it.get("in_extern").and_then(Json::as_bool).unwrap_or(false);
                let dsl_node = it.get("dsl").ok_or("map 项缺 dsl")?;
                let cell = dsl_node.get("cell").and_then(Json::as_str).unwrap_or("").to_string();
                let stmt = dsl_node.get("stmt").and_then(Json::as_usize_opt);
                let line = dsl_node.get("line").and_then(Json::as_u32).unwrap_or(0);
                let dcol = dsl_node.get("col").and_then(Json::as_u32).unwrap_or(0);
                entries.push(MapEntry {
                    rs_line_start: start,
                    rs_line_end: end.max(start),
                    rs_col: col,
                    kind,
                    in_extern,
                    dsl: DslPos { cell, stmt, line, col: dcol },
                });
            }
        }
        Some(Json::Null) | None => return Err("缺 map 数组".to_string()),
        Some(_) => return Err("map 不是数组".to_string()),
    }

    Ok(SrcMap { version, file, runtime_lines, cells, entries })
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\r' | b'\n') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            Err(format!("第 {} 字节：期望 {:?}，实际 {:?}", self.i, c as char, self.peek()))
        }
    }

    fn lit(&mut self, word: &str, v: Json) -> Result<Json, String> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(format!("第 {} 字节：不是合法 JSON 字面量", self.i))
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        self.ws();
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.lit("true", Json::Bool(true)),
            Some(b'f') => self.lit("false", Json::Bool(false)),
            Some(b'n') => self.lit("null", Json::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            _ => Err(format!("第 {} 字节：不是 JSON 值", self.i)),
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.expect(b'{')?;
        let mut kv = Vec::new();
        loop {
            self.ws();
            if self.peek() == Some(b'}') {
                self.i += 1;
                return Ok(Json::Obj(kv));
            }
            let key = self.string()?;
            self.ws();
            self.expect(b':')?;
            let val = self.value()?;
            kv.push((key, val));
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Json::Obj(kv));
                }
                _ => return Err(format!("第 {} 字节：对象里期望 , 或 }}", self.i)),
            }
        }
    }

    fn array(&mut self) -> Result<Json, String> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        loop {
            self.ws();
            if self.peek() == Some(b']') {
                self.i += 1;
                return Ok(Json::Arr(items));
            }
            items.push(self.value()?);
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(format!("第 {} 字节：数组里期望 , 或 ]", self.i)),
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let start = self.i;
        // 先扫到未转义的收尾引号，再解转义（绝大多数字符串没有转义，省一次分配）。
        while self.i < self.b.len() {
            let c = self.b[self.i];
            if c == b'"' {
                let raw = std::str::from_utf8(&self.b[start..self.i]).map_err(|e| e.to_string())?;
                self.i += 1;
                if raw.contains('\\') {
                    return unescape(raw).map_err(|e| format!("第 {start} 字节: {e}"));
                }
                return Ok(raw.to_string());
            }
            if c == b'\\' {
                self.i += 2;
                continue;
            }
            self.i += 1;
        }
        Err(format!("第 {start} 字节：字符串没闭合"))
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        while self.i < self.b.len() {
            let c = self.b[self.i];
            if c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-') {
                self.i += 1;
            } else {
                break;
            }
        }
        let raw = std::str::from_utf8(&self.b[start..self.i]).map_err(|e| e.to_string())?;
        raw.parse::<f64>().map(Json::Num).map_err(|_| format!("第 {start} 字节：非法数字 {raw:?}"))
    }
}

/// 解 JSON 字符串转义（`\uXXXX` 含代理对）。
fn unescape(raw: &str) -> Result<String, String> {
    let mut out = String::with_capacity(raw.len());
    let mut it = raw.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('b') => out.push('\u{08}'),
            Some('f') => out.push('\u{0c}'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('u') => {
                let hi = hex4(&mut it)?;
                // 高位代理：必须跟一个 `\uXXXX` 低位代理才凑成完整码点。
                let ch = if (0xD800..0xDC00).contains(&hi) {
                    let _slash = it.next().filter(|c| *c == '\\').ok_or("代理对缺第二位")?;
                    let _u = it.next().filter(|c| *c == 'u').ok_or("代理对缺 u")?;
                    let lo = hex4(&mut it)?;
                    let combined = 0x10000 + (((hi - 0xD800) as u32) << 10) + (lo - 0xDC00) as u32;
                    char::from_u32(combined).ok_or("非法代理对码点")?
                } else {
                    char::from_u32(hi as u32).ok_or("非法 \\u 码点")?
                };
                out.push(ch);
            }
            Some(other) => return Err(format!("非法转义 \\{other}")),
            None => return Err("结尾悬挂的反斜杠".to_string()),
        }
    }
    Ok(out)
}

fn hex4(it: &mut std::str::Chars<'_>) -> Result<u16, String> {
    let mut v: u32 = 0;
    for _ in 0..4 {
        let c = it.next().ok_or("\\u 不足 4 位")?;
        let d = c.to_digit(16).ok_or_else(|| format!("非法十六进制位 {c:?}"))?;
        v = v * 16 + d;
    }
    Ok(v as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_str_escapes_quote_and_backslash() {
        assert_eq!(json_str(r#"a\b"c"#), r#""a\\b\"c""#);
        assert_eq!(json_str("\n"), r#""\n""#);
    }

    #[test]
    fn roundtrip_preserves_entries() {
        let mut b = SrcMapBuilder::new("app.rs");
        b.set_runtime_lines(300);
        b.register_cell("fact", Span::new(0, 12, 1, 10));
        b.push(MapKind::RetStruct, DslPos::item("fact", Span::new(0, 12, 1, 10)), false, 305, 308);
        b.push(MapKind::Stmt, DslPos::stmt("fact", 3, Span::new(0, 15, 5, 9)), true, 320, 325);
        let m = b.build();
        let text = m.to_json();
        let back = parse_srcmap_json(&text).expect("自产的 JSON 必须能读回");
        assert_eq!(back, m);
        assert_eq!(back.lookup(322).map(|e| e.in_extern), Some(true));
        assert_eq!(back.lookup(300), None);
    }

    #[test]
    fn written_lines_counts_newlines() {
        assert_eq!(written_lines("a\nb\n"), 2);
        assert_eq!(written_lines("a\nb"), 1);
        assert_eq!(next_line("a\nb\n"), 3);
        // 字符串字面量里的 \n 转义（两个字符）不算行
        assert_eq!(written_lines("println!(\"\\n\");\n"), 1);
    }
}
