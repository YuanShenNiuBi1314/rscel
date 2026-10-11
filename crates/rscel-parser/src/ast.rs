//! 抽象语法树（规格 §8.1，**逐字段照抄**）。
//!
//! 相对 §8.1 原文只有下列扩展，全部在类型定义处就地注释说明：
//! 1. [`Stmt::Sync`]：主会话裁定 A3——§8.1 的 `Stmt` 枚举漏了 `jump sync`，而 §11.2 引用了
//!    `JumpSync`，故补 `Sync { group, span }` 变体。
//! 2. [`Stmt::Out`] 改为 `Out { args, span }`：补 `span`，供 §15 源映射与下游错误定位使用。
//! 3. [`Stmt::RustBlock`]：§3.5.2 要求"扫描到 `{` 就递归进入块内，对块内语句重新做
//!    DSL 关键字 vs Rust 透传分派"，但 §8.1 的 `Stmt` 没有任何位置安放**嵌套语句**
//!    （文档缺陷）。故新增一个变体，用 [`RawBlock`] 承载"逐字文本片段 + 嵌套块语句"，
//!    顺序与源文本一致，`RawBlock::to_source()` 在块内无 DSL 语句时逐字还原原文。
//!
//! 另加 §8.1 提到的 `type Ident = String`，以及 `Program::cells/cell` 便捷访问器。

use crate::span::Span;

/// 标识符（Cell 名、键名、变量别名等）。
pub type Ident = String;

/// 一个程序。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Program {
    pub items: Vec<Item>,
    /// `entry <ident>;` 声明（§2.1 规则 2；多于一条报 E037）。
    pub entry: Option<Ident>,
}

/// 顶层项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    Cell(CellDef),
    /// `extern rust { .. }` / `extern rust read { .. }` 全局块，或顶层 Rust item
    /// （§3.5.4：隐含的 `extern rust{}`）。
    ///
    /// `text` 为**整条**语句的逐字切片（`extern rust` 形式含 `extern rust` 前缀与花括号；
    /// 顶层 item 形式只含 item 本身），生成阶段直接按序拼接即可还原模块顶部内容。
    GlobalRust(RawRust),
}

/// 一个 Cell 定义。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellDef {
    pub name: Ident,
    pub receive: ReceiveClause,
    pub body: Vec<Stmt>,
    pub span: Span,
}

/// `receive(...) [as (...)];`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiveClause {
    /// 键名（可带包前缀、可带类型注解）。
    pub keys: Vec<PacketKey>,
    /// `as` 之后的别名；省略 `as` 时 = 键名去包前缀（§3.4.1 规则 2）。
    ///
    /// 与 `keys` **按下标一一对应**（parser 不校验数量，A7：留给 `rscel-check` 的 C1）。
    /// §1.2 示例里的 `receive(order -> ord);` 写法的行内别名也归一到本字段。
    pub aliases: Vec<Ident>,
    pub span: Span,
}

/// 传输键（`g1.W1.A`、`n`、`n: i64`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PacketKey {
    /// 包限定前缀；裸键为 `None`。
    ///
    /// **裁定依据**（主会话任务书第 5.1 条）：`g1.W1.A` 按 `pkg = 第一个点之前`,
    /// `name = 最后一个点之后`，中间的 `.` 一律并入 `pkg` 文本，故此处为 `Some("g1.W1")`、
    /// `name == "A"`。下游按 `pkg` 里的 `W1` 槽位解析（§9.4 的 `g1.W1` / `g1.W2`）。
    pub pkg: Option<Ident>,
    /// 键名（= 最后一个点之后的部分）。
    pub name: Ident,
    /// `: <rust_type>` 类型注解（透传）。
    pub ty: Option<RawRust>,
    pub span: Span,
}

/// Cell 内一条语句。
///
/// `clippy::large_enum_variant` 已被显式关闭：§8.1 规定 `Jump(JumpStmt)` 内联存放，
/// 改用 `Box` 会破坏"逐字段照抄 §8.1"的契约与下游匹配写法；AST 只在内存中短暂存在，
// 体积差异（264 vs 57 字节）不构成性能问题。
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stmt {
    /// Rust 透传语句（`let` / `if` / `for` / 表达式 / 嵌套 `fn` item …）。
    RawRust(RawRust),
    Jump(JumpStmt),
    /// `exit go with(...);`
    Exit { args: Vec<Arg>, span: Span },
    /// `die;`
    Die { span: Span },
    /// `out(expr, ...);`（**扩展**：§8.1 为 `Out(Vec<RawRust>)`，此处补 `span`）
    Out { args: Vec<RawRust>, span: Span },
    /// `extern rust [read] { ... };`（Cell 内）
    ExternRustBlock { read_only: bool, body: RawRust, span: Span },
    /// `jump sync <ident>;`（**扩展 A3**，§8.1 枚举漏列）
    Sync { group: Ident, span: Span },
    /// 含嵌套 DSL 语句的 Rust 块语句（**扩展**，§3.5.2；§8.1 无对应位置）
    RustBlock { block: RawBlock },
}

/// `jump` 的四种形态。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum JumpKind {
    Plain,
    Loop,
    Recurse,
    And,
}

/// 传输模式。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum TransMode {
    GoWith,
    ReadWith,
    ForceGoWith,
    ForceReadWith,
}

/// `jump` 语句。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JumpStmt {
    pub kind: JumpKind,
    pub targets: Vec<Ident>,
    /// `when <expr>`（透传）。
    pub condition: Option<RawRust>,
    pub mode: Option<TransMode>,
    /// `go with` / `read with` 实参。
    pub args: Vec<Arg>,
    /// `return(<key> -> <alias>, ...)`。
    pub ret_map: Vec<(PacketKey, Ident)>,
    pub over: Option<OverSpec>,
    /// `concurrent_limit=<expr>`（透传）。
    pub concurrent_limit: Option<RawRust>,
    pub group: Option<Ident>,
    pub span: Span,
}

/// 一个传输实参（`expr [-> key]`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arg {
    /// 实参表达式（透传）。
    pub expr: RawRust,
    /// `->` 显式键名；缺省时由 §9.1 从表达式推导，见 [`Arg::effective_key`]。
    pub key: Option<Ident>,
    pub span: Span,
}

impl Arg {
    /// 按 §9.1 推导实参键名：
    /// - 有显式 `-> key`：返回该键名；
    /// - 表达式是简单标识符：返回表达式本身（等价 `x -> x`）；
    /// - 表达式不是简单标识符且未写显式键名：返回空串，**由 `rscel-check` 报 E040**
    ///   （A7：parser 不做语义检查，只提供判定依据）。
    ///
    /// 例外：`jump loop` 按位置刷新 receive 列表，不使用键名（§9.2），由 check 层另行判定。
    pub fn effective_key(&self) -> String {
        if let Some(k) = &self.key {
            return k.clone();
        }
        let t = self.expr.text.trim();
        if is_plain_ident(t) {
            t.to_string()
        } else {
            String::new()
        }
    }

    /// 实参是否缺少显式键名（§9.1 的 E040 判定依据）。
    pub fn needs_explicit_key(&self) -> bool {
        self.key.is_none() && !is_plain_ident(self.expr.text.trim())
    }
}

/// 是否是"简单标识符"（`[A-Za-z_][A-Za-z0-9_]*`，与 §9.1 的"简单标识符"一致）。
pub fn is_plain_ident(s: &str) -> bool {
    let mut it = s.chars();
    match it.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    it.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `over <collection> |> op(...) |> op(...)`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverSpec {
    pub collection: RawRust,
    pub ops: Vec<OverOp>,
}

/// `over` 管道算子。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OverOp {
    Filter(RawRust),
    Map(RawRust),
    FlatMap(RawRust),
    Take(RawRust),
    Skip(RawRust),
}

/// 一段 Rust 透传文本。
///
/// **裁定依据 A1**：`text` 必须是源文本的**逐字切片**，生成阶段要求 100% 还原。
/// - 语句型透传（`let x = 5;`）含结尾 `;`；
/// - 块语句型透传（`if`/`while`/`for`/`match` 等）**不含**分号（§3.1 规则 3"解析器忽略块后的
///   多余 `;`"）；
/// - 表达式型透传（`when` 条件、实参、类型注解、闭包…）两端做了 `trim`——A1 允许 trim，
///   且扫描起点前已跳过空白，trim 只影响尾端，`span.len` 同步收缩。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawRust {
    pub text: String,
    pub span: Span,
}

impl RawRust {
    /// 去掉尾端空白（首端扫描前已跳过空白，实际不会被裁）。
    pub fn trimmed(mut self) -> RawRust {
        let new_len = self.text.trim_end().len();
        if new_len != self.text.len() {
            self.text.truncate(new_len);
            self.span.len = (new_len as u32).min(self.span.len);
        }
        self
    }
}

/// 一段 Rust 透传文本 + 块内重新分派得到的语句（§3.5.2）。
///
/// `parts` 严格保持源文本顺序：`Text` 是逐字片段，`Stmt` 是该位置上的 DSL 语句或
/// 嵌套 Rust 块。文本片段承担块之间的连接文本（如 `} else `），嵌套块的 `{}` 由
/// `RawBlockPart::Stmt(RustBlock)` 的渲染补回，因此 `to_source()` 在 [`RawBlock::is_pure`]
/// 为真时与源文本逐字相等。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawBlock {
    pub parts: Vec<RawBlockPart>,
    /// 整条块语句的 span（含头部文本与所有嵌套块）。
    pub span: Span,
}

/// Rust 块内的一个片段。
///
/// 同样因内联持有 `Stmt` 而关闭 `large_enum_variant`（见 [`Stmt`] 的说明）。
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RawBlockPart {
    /// 逐字文本片段。
    Text(RawRust),
    /// DSL 语句或嵌套 Rust 块。
    Stmt(Stmt),
}

impl RawBlock {
    /// 块内是否**只**含透传文本（无任何 DSL 语句，递归判定）。
    ///
    /// 为真时 `to_source()` 与源文本逐字相等。
    pub fn is_pure(&self) -> bool {
        self.parts.iter().all(|p| match p {
            RawBlockPart::Text(_) => true,
            RawBlockPart::Stmt(Stmt::RustBlock { block }) => block.is_pure(),
            RawBlockPart::Stmt(Stmt::RawRust(_)) => true,
            RawBlockPart::Stmt(_) => false,
        })
    }

    /// 块内是否含 DSL 语句（`is_pure` 的反面）。
    pub fn has_dsl(&self) -> bool {
        !self.is_pure()
    }

    /// 拼回透传文本。`is_pure()` 为真时逐字等于源文本；含 DSL 语句时，DSL 语句位置
    /// 不产出字符（生成阶段应改走 code-gen 路径把 DSL 语句翻译为 Rust 调用）。
    pub fn to_source(&self) -> String {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                RawBlockPart::Text(t) => out.push_str(&t.text),
                RawBlockPart::Stmt(Stmt::RawRust(r)) => out.push_str(&r.text),
                RawBlockPart::Stmt(Stmt::RustBlock { block }) => {
                    out.push('{');
                    out.push_str(&block.to_source());
                    out.push('}');
                }
                RawBlockPart::Stmt(_) => {}
            }
        }
        out
    }
}

impl Program {
    /// 按声明顺序遍历所有 Cell。
    pub fn cells(&self) -> impl Iterator<Item = &CellDef> {
        self.items.iter().filter_map(|i| match i {
            Item::Cell(c) => Some(c),
            Item::GlobalRust(_) => None,
        })
    }

    /// 按名字查找 Cell。
    pub fn cell(&self, name: &str) -> Option<&CellDef> {
        self.cells().find(|c| c.name == name)
    }

    /// 顶层 `extern rust {}` / Rust item 透传片段。
    pub fn global_rust(&self) -> impl Iterator<Item = &RawRust> {
        self.items.iter().filter_map(|i| match i {
            Item::GlobalRust(r) => Some(r),
            Item::Cell(_) => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(text: &str) -> RawRust {
        RawRust { text: text.to_string(), span: Span::default() }
    }

    #[test]
    fn plain_ident_detection() {
        assert!(is_plain_ident("x"));
        assert!(is_plain_ident("_a1"));
        assert!(!is_plain_ident("num - 1"));
        assert!(!is_plain_ident("f()"));
        assert!(!is_plain_ident("1"));
        assert!(!is_plain_ident("a.b"));
    }

    #[test]
    fn arg_key_derivation() {
        let a = Arg { expr: raw("next"), key: Some("n".into()), span: Span::default() };
        assert_eq!(a.effective_key(), "n");
        let b = Arg { expr: raw("val"), key: None, span: Span::default() };
        assert_eq!(b.effective_key(), "val");
        assert!(!b.needs_explicit_key());
        let c = Arg { expr: raw("num - 1"), key: None, span: Span::default() };
        assert!(c.needs_explicit_key());
        assert_eq!(c.effective_key(), "");
    }

    #[test]
    fn pure_block_reconstructs_source() {
        // 与 parser 的实际约定一致：块的花括号由 RustBlock 渲染，Text 片段只含块外文本
        let b = RawBlock {
            parts: vec![
                RawBlockPart::Text(raw("if c ")),
                RawBlockPart::Stmt(Stmt::RawRust(raw("let x = 1;"))),
                RawBlockPart::Text(raw(" } else ")),
                RawBlockPart::Stmt(Stmt::RustBlock {
                    block: RawBlock { parts: vec![RawBlockPart::Text(raw("die;"))], span: Span::default() },
                }),
            ],
            span: Span::default(),
        };
        assert!(b.is_pure());
        assert_eq!(b.to_source(), "if c let x = 1; } else {die;}");
    }

    #[test]
    fn block_with_dsl_is_not_pure() {
        let b = RawBlock {
            parts: vec![
                RawBlockPart::Text(raw("if c ")),
                RawBlockPart::Stmt(Stmt::Die { span: Span::default() }),
            ],
            span: Span::default(),
        };
        assert!(b.has_dsl());
        assert_eq!(b.to_source(), "if c ");
    }
}
