//! 语法分析器（规格 §8.2 / §8.3）与公开入口。
//!
//! 解析主循环严格按 §8.2 伪代码；以下几处偏离/扩展均在代码处就地标注裁定依据：
//! - **A2（Cell 边界裁定）**：cell body 的语句循环只在遇到 `cell` / `entry` / `extern rust`
//!   三个起始 token 时结束；`use/struct/enum/fn/trait/impl/const/static/type/mod` 出现在
//!   cell body 的语句位置时视为**该 Cell 内的嵌套 Rust item**（合法 Rust：
//!   `fn f(){ fn helper(){} }`），扫描为 `Stmt::RawRust` 并继续留在本 Cell。
//! - **A3**：`jump sync <ident>;` 解析为 [`Stmt::Sync`]。
//! - **A4**：`entry <ident>;` 第二条起报 RSC-E037，归一到 `Program::entry`。
//! - **A7**：parser 不做语义检查（receive 别名数量、收敛、所有权、键名存在性都留给
//!   `rscel-check`），只报语法层问题 + E034 / E036 / E037。
//! - **§3.5.2（文档缺陷 A3'）**：§8.1 的 `Stmt` 枚举没有任何位置安放"块内重新分派得到的
//!   嵌套语句"，故新增 `Stmt::RustBlock { block: RawBlock }`（`RawBlock` = 逐字文本片段 +
//!   嵌套 DSL 语句的有序交替），见 `ast.rs` 模块文档。

use crate::ast::*;
use crate::diag::{code, Diagnostic, Severity};
use crate::lexer::{
    Lexer, StopKind, Stops, ORPHAN_KEYWORDS, RUST_ITEM_ALL,
};
use crate::span::Span;

/// 解析结果。
#[derive(Clone, Debug)]
pub struct ParseOutcome {
    /// 解析出的程序（即使有语法错误也尽量产出已解析部分）。
    pub program: Program,
    /// 全部诊断（按发现顺序）。
    pub diagnostics: Vec<Diagnostic>,
}

impl ParseOutcome {
    /// 是否存在错误级诊断。
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(|d| d.severity == Severity::Error)
    }

    /// 首个错误级诊断。
    pub fn first_error(&self) -> Option<&Diagnostic> {
        self.diagnostics.iter().find(|d| d.severity == Severity::Error)
    }
}

/// 主入口：解析整个 `.rscel` 源文件。**永不 panic**，语法错误全部进 diagnostics。
pub fn parse_program(src: &str, file_id: u32) -> ParseOutcome {
    let mut p = Parser::new(src, file_id);
    let program = p.run();
    ParseOutcome { program, diagnostics: p.diags }
}

/// 递归下降解析器。
pub struct Parser<'a> {
    src: &'a str,
    lex: Lexer<'a>,
    diags: Vec<Diagnostic>,
    /// 当前所在 cell（用于诊断定位，§15.1）。
    cell: Option<String>,
    /// 已声明的 cell 名（E036 去重）。
    cells: Vec<(String, u32)>,
    /// 当前 jump 的某个可选段是否已报错并跳到 `;`（收尾检查据此不重复报错）。
    jump_recovered: bool,
    /// 是否正在解析 Rust 块内部（块内的语句结束符更宽松，见 [`Parser::end_stmt`]）。
    in_block: bool,
}

impl<'a> Parser<'a> {
    /// 新建解析器。
    pub fn new(src: &'a str, file_id: u32) -> Self {
        Parser {
            src,
            lex: Lexer::new(src, file_id),
            diags: Vec::new(),
            cell: None,
            cells: Vec::new(),
            jump_recovered: false,
            in_block: false,
        }
    }

    /// 源文本。
    pub fn src(&self) -> &'a str {
        self.src
    }

    /// 取出已收集的诊断。
    pub fn into_diagnostics(self) -> Vec<Diagnostic> {
        self.diags
    }

    fn run(&mut self) -> Program {
        let mut program = Program { items: Vec::new(), entry: None };
        loop {
            if let Err(d) = self.lex.skip_trivia() {
                self.push(d);
                break;
            }
            if self.lex.at_eof() {
                break;
            }
            if self.lex.eat_char(';') {
                continue; // 允许并忽略空语句
            }
            let before = self.lex.pos();
            self.parse_item(&mut program);
            if self.lex.pos() == before {
                // 防御：任何分支都应至少消费一个字符，否则强制前进避免死循环
                self.lex.bump();
            }
        }
        program
    }

    // ---------------------------------------------------------------- 顶层

    fn parse_item(&mut self, program: &mut Program) {
        let item_start = self.lex.pos();
        // 顶层 Rust item 可带 `#[derive(...)]` 等属性前缀（§3.5.4 示例）
        self.scan_attributes();
        let kw = match self.lex.peek_stmt_start() {
            Ok(k) => k,
            Err(d) => {
                self.push(d);
                self.recover_item();
                return;
            }
        };
        match kw {
            Some("cell") => {
                let cell = self.parse_cell();
                program.items.push(Item::Cell(cell));
            }
            Some("extern rust") => {
                if let Some(raw) = self.parse_extern_rust_global() {
                    program.items.push(Item::GlobalRust(raw));
                }
            }
            Some("entry") => self.parse_entry(program),
            Some(k) if is_rust_item_kw(k) => {
                // 顶层 Rust item 透传：属性 + item 本体，整体不透明（§3.5.4）
                if let Some(raw) = self.scan_rust_item(item_start) {
                    program.items.push(Item::GlobalRust(raw));
                }
            }
            Some(k) => {
                self.push(
                    Diagnostic::error(
                        code::E032,
                        "顶层必须是 cell / extern rust / entry / Rust item",
                        self.span_at(self.lex.pos()),
                    )
                    .with_detail(format!("遇到 DSL 关键字 `{k}`（§3.3 的 <item> 产生式）")),
                );
                self.recover_item();
            }
            None => {
                self.push(
                    Diagnostic::error(
                        code::E032,
                        "顶层必须是 cell / extern rust / entry / Rust item",
                        self.span_at(self.lex.pos()),
                    )
                    .with_detail("遇到既非 DSL 关键字也非 Rust item 起始的 token（§8.2 主循环 else 分支）"),
                );
                self.recover_item();
            }
        }
        // 块后多余 `;` 一律忽略（§3.1 规则 3）
        self.lex.eat_semi();
    }

    fn parse_entry(&mut self, program: &mut Program) {
        let start = self.lex.pos();
        self.lex.eat_word("entry");
        let (name, span) = match self.lex.read_ident() {
            Ok(v) => v,
            Err(d) => {
                self.push(d);
                (String::new(), self.span_at(start))
            }
        };
        if let Err(d) = self.lex.expect_semi() {
            self.push(d);
        }
        if let Some(prev) = program.entry.clone() {
            // A4 / E037：多于一条 entry 声明（§2.1 规则 3）
            self.push(
                Diagnostic::error(code::E037, format!("多于一条 `entry` 声明：`entry {name}`"), span)
                    .with_detail(format!("§2.1 规则 3：同一程序至多一条 `entry` 声明；已声明的入口是 `{prev}`")),
            );
        } else {
            program.entry = Some(name);
        }
    }

    /// 顶层 `extern rust [read] { .. }`，返回**整条**语句的逐字切片
    /// （含 `extern rust` 前缀与花括号，不含尾随 `;`）。
    ///
    /// §3.3 的 <extern_rust_global> 允许可选的 `read`，但顶层无语义差别（只读约束只对
    /// Cell 内环境有意义），故原样保留在文本中。
    fn parse_extern_rust_global(&mut self) -> Option<RawRust> {
        let start = self.lex.pos();
        self.lex.eat_phrase("extern rust");
        self.lex.eat_word("read");
        // scan_braced_block 自己消费 `{` 并给出"缺少 `{`"的 E030，这里不要重复 expect
        if let Err(d) = self.lex.scan_braced_block() {
            self.push(d);
            self.recover_item();
            return None;
        }
        Some(self.lex.raw(start, self.lex.pos()))
    }

    /// 顶层 Rust item：从 `start`（可能已含属性）扫到 `;` 或配平的 `{}` 块。
    fn scan_rust_item(&mut self, start: usize) -> Option<RawRust> {
        match self.lex.scan_raw_stmt() {
            Ok(_) => Some(self.lex.raw(start, self.lex.pos())),
            Err(d) => {
                self.push(d);
                self.recover_item();
                None
            }
        }
    }

    /// 消费前置属性 `#[ ... ]`（可多个），返回是否消费到。
    fn scan_attributes(&mut self) -> bool {
        let mut any = false;
        loop {
            let save = self.lex.pos();
            if !self.lex.peek_str("#[") {
                self.lex.restore(save);
                break;
            }
            self.lex.bump(); // '#'
            if self.lex.skip_balanced().is_err() {
                self.lex.restore(save);
                break;
            }
            any = true;
        }
        any
    }

    // ---------------------------------------------------------------- Cell

    fn parse_cell(&mut self) -> CellDef {
        let start = self.lex.pos();
        self.lex.eat_word("cell");
        let (name, name_span) = match self.lex.read_ident() {
            Ok(v) => v,
            Err(d) => {
                self.push(d);
                (String::new(), self.span_at(start))
            }
        };
        if let Err(d) = self.lex.expect_semi() {
            self.push(d);
        }
        if !name.is_empty() {
            if let Some((_, line)) = self.cells.iter().find(|(n, _)| *n == name) {
                self.push(
                    Diagnostic::error(code::E036, format!("cell 名称重复定义：`{name}`"), name_span)
                        .with_detail(format!("首次定义在第 {line} 行")),
                );
            } else {
                self.cells.push((name.clone(), name_span.line));
            }
        }

        self.cell = Some(name.clone());
        let receive = self.parse_receive_clause(&name);
        let body = self.parse_cell_body();
        self.cell = None;

        CellDef { name, receive, body, span: self.lex.span(start, self.lex.pos()) }
    }

    /// Cell body 语句循环（§8.2 `parse_cell`）。
    ///
    /// **A2 裁定**：只在 `cell` / `entry` / `extern rust` 三个起始 token 处结束；
    /// Rust item 关键字出现在语句位置时视为本 Cell 内的嵌套 item，继续留在本 Cell。
    fn parse_cell_body(&mut self) -> Vec<Stmt> {
        let mut body = Vec::new();
        loop {
            if let Err(d) = self.lex.skip_trivia() {
                self.push(d);
                break;
            }
            if self.lex.at_eof() {
                break;
            }
            if self.lex.eat_char(';') {
                continue; // 忽略空语句
            }
            let kw = match self.lex.peek_stmt_start() {
                Ok(k) => k,
                Err(d) => {
                    self.push(d);
                    break;
                }
            };
            // A2 裁定：cell body 只在 `cell` / `entry` 处结束。
            // **偏离主会话裁定 A2 的字面表述**（A2 原文把 `extern rust` 也列为终止符）：
            // §3.3 文法把 <extern_rust_block> 定义为 <stmt>，§3.4.4 明确给出 Cell 内
            // `extern rust { .. }` 的用法（热点计算逃逸口），任务书测试矩阵第 5 条也要求
            // "全局与 Cell 内两种位置"。故 `extern rust` 在 cell body 内是**语句**，
            // 顶层（不在 cell 内）才是 Item::GlobalRust。
            if matches!(kw, Some("cell") | Some("entry")) {
                break;
            }
            let before = self.lex.pos();
            match self.parse_stmt(kw) {
                Ok(Some(s)) => body.push(s),
                Ok(None) => {
                    // 孤立关键字 / 多余 receive：跳到语句边界，避免原地重复报同一处
                    self.recover_stmt();
                }
                Err(d) => {
                    self.push(d);
                    self.recover_stmt();
                }
            }
            if self.lex.pos() == before {
                self.lex.bump();
            }
        }
        body
    }

    /// 解析 Cell 内一条语句。`Ok(None)` 表示未产出语句（已就地报诊断）。
    fn parse_stmt(&mut self, kw: Option<&'static str>) -> Result<Option<Stmt>, Diagnostic> {
        match kw {
            Some("jump") => Ok(Some(self.parse_jump())),
            Some("exit go with") => Ok(Some(self.parse_exit())),
            Some("die") => Ok(Some(self.parse_die())),
            Some("out") => Ok(Some(self.parse_out())),
            Some("extern rust") => Ok(Some(self.parse_extern_rust_block())),
            Some(k) if is_orphan_kw(k) => {
                let at = self.lex.pos();
                self.push(
                    Diagnostic::error(
                        code::E031,
                        format!("孤立关键字：`{k}` 不构成合法 DSL 语句"),
                        self.span_at(at),
                    )
                    .with_detail("§7.4：孤立的 go / with / exit 出现在语句起始位置"),
                );
                Ok(None)
            }
            Some("receive") => {
                let at = self.lex.pos();
                self.push(
                    Diagnostic::error(
                        code::E030,
                        "语法错误：多余的 `receive(...)` 声明",
                        self.span_at(at),
                    )
                    .with_detail("§3.4.1 规则 1：每个 Cell 恰好一条 receive，且必须紧跟 `cell <name>;`"),
                );
                Ok(None)
            }
            _ => self.parse_raw_stmt().map(Some),
        }
    }

    /// `receive(...) [as (...)];`；缺失时报 E034 并返回空子句。
    fn parse_receive_clause(&mut self, cell_name: &str) -> ReceiveClause {
        // 先吃掉前导空白/注释，保证 receive 子句的 span 落在 `receive` 关键字上
        if let Err(d) = self.lex.skip_trivia() {
            self.push(d);
            return ReceiveClause { keys: Vec::new(), aliases: Vec::new(), span: self.lex.span(self.lex.pos(), self.lex.pos()) };
        }
        let start = self.lex.pos();
        if !self.lex.eat_word("receive") {
            self.push(
                Diagnostic::error(
                    code::E034,
                    format!("cell `{cell_name}` 之后必须紧跟 `receive(...)` 声明"),
                    self.span_at(start),
                )
                .with_detail(
                    "§3.4.1 规则 1：receive 必须紧跟 cell 之后，每个 Cell 恰好一条（空接收写 `receive();`）",
                ),
            );
            return ReceiveClause { keys: Vec::new(), aliases: Vec::new(), span: self.span_at(start) };
        }
        self.parse_receive_inner(start)
    }

    /// `receive (键列表) [as (别名列表)];`
    ///
    /// **括号可省略（文档缺陷兼容）**：§3.3 文法与 §3.4.1 示例一律写 `receive(a, b);`，
    /// 而《语言设计文档.md》§5.2/§5.3/§5.4、§7.6 与附录 A.2/A.4/A.6 一律写
    /// `receive a, b;`（无括号，`;` 结束）。两种写法都接受，产出的 AST 完全一致。
    fn parse_receive_inner(&mut self, start: usize) -> ReceiveClause {
        self.lex.eat_word("receive");
        let paren = self.lex.eat_char('(');

        let mut keys: Vec<PacketKey> = Vec::new();
        let mut inline: Vec<Option<Ident>> = Vec::new();
        loop {
            if let Err(d) = self.lex.skip_trivia() {
                self.push(d);
                break;
            }
            if self.lex.at_eof() {
                if paren {
                    self.push(Diagnostic::error(
                        code::E030,
                        "词法错误：未闭合的 `(`（receive 键列表）",
                        self.lex.span(start, start),
                    ));
                }
                break;
            }
            match self.lex.peek_char() {
                Some(')') if paren => break,
                Some(';') if !paren => break,
                _ => {}
            }
            // 无括号形式的键列表由 `;` 结束，故必须在 `as` 子句之前停下
            if !paren && self.lex.peek_word("as") {
                break;
            }
            if self.lex.eat_char(',') {
                continue; // 容忍多余逗号
            }
            let before = self.lex.pos();
            match self.parse_packet_key() {
                Ok((k, alias)) => {
                    keys.push(k);
                    inline.push(alias);
                }
                Err(d) => self.push(d),
            }
            if self.lex.pos() == before {
                self.lex.bump();
            }
        }
        if paren {
            if let Err(d) = self.lex.expect_char(')') {
                self.push(d);
            }
        }

        // `as (<ident> [: <type>] , ...)`（括号同样可省略，与键列表保持一致）
        let mut aliases: Vec<Ident> = Vec::new();
        if self.lex.eat_word("as") {
            let as_paren = self.lex.eat_char('(');
            {
                loop {
                    if let Err(d) = self.lex.skip_trivia() {
                        self.push(d);
                        break;
                    }
                    if self.lex.at_eof() {
                        if as_paren {
                            self.push(Diagnostic::error(
                                code::E030,
                                "词法错误：未闭合的 `(`（receive 别名列表）",
                                self.lex.span(start, start),
                            ));
                        }
                        break;
                    }
                    match self.lex.peek_char() {
                        Some(')') if as_paren => break,
                        Some(';') if !as_paren => break,
                        _ => {}
                    }
                    if self.lex.eat_char(',') {
                        continue;
                    }
                    let before = self.lex.pos();
                    match self.lex.read_ident() {
                        Ok((id, _)) => {
                            // §3.4.1 示例 `receive(n: i64) as (num: i64);` 的别名也带类型注解，
                            // §3.3 文法的 <ident> 没有：解析后归一到对应键的 `ty`（键没有时）。
                            let ty = if self.lex.eat_char(':') { Some(self.scan_type_text()) } else { None };
                            if let Some(ty) = ty {
                                if let Some(k) = keys.get_mut(aliases.len()) {
                                    if k.ty.is_none() {
                                        k.ty = Some(ty);
                                    }
                                }
                            }
                            aliases.push(id);
                        }
                        Err(d) => self.push(d),
                    }
                    if self.lex.pos() == before {
                        self.lex.bump();
                    }
                }
                if as_paren {
                    if let Err(d) = self.lex.expect_char(')') {
                        self.push(d);
                    }
                }
            }
        }

        if aliases.is_empty() {
            // 省略 `as` 时别名 = 键名去包前缀（§3.4.1 规则 2）；§1.2 示例的
            // `receive(order -> ord);` 行内别名同样归一到本列表（无需给 PacketKey 加字段）。
            for (i, k) in keys.iter().enumerate() {
                aliases.push(inline.get(i).cloned().flatten().unwrap_or_else(|| k.name.clone()));
            }
        } else if inline.iter().any(|a| a.is_some()) {
            self.push(
                Diagnostic::error(
                    code::E030,
                    "语法错误：`receive` 不能同时使用 `->` 行内别名与 `as` 别名",
                    self.lex.span(start, self.lex.pos()),
                )
                .with_detail("§1.2 示例用 `receive(order -> ord);`，§3.3 文法用 `as (...)`，二者只能选其一"),
            );
        }

        if let Err(d) = self.lex.expect_semi() {
            self.push(d);
        }
        ReceiveClause { keys, aliases, span: self.lex.span(start, self.lex.pos()) }
    }

    /// `<packet_key> ::= <ident> [ "." <ident> ]* [ ":" <rust_type> ] [ "->" <ident> ]`
    ///
    /// 返回 `(key, 行内别名)`：行内别名只出现在 §1.2 的 `receive(order -> ord)` 写法与
    /// `return(<key> -> <alias>)` 中；`receive` 把它归一到 `ReceiveClause::aliases`，
    /// `return` 把它当作映射目标，因此 `PacketKey` 无需增加字段。
    fn parse_packet_key(&mut self) -> Result<(PacketKey, Option<Ident>), Diagnostic> {
        let start = self.lex.pos();
        let (first, _) = self.lex.read_ident()?;
        let mut segs = vec![first];
        while self.lex.peek_char() == Some('.') {
            self.lex.bump();
            let (seg, _) = self.lex.read_ident()?;
            segs.push(seg);
        }
        // 裁定依据（任务书第 5.1 条）：pkg = 最后一个点之前的全部文本，name = 最后一段
        let (pkg, name) = if segs.len() == 1 {
            (None, segs[0].clone())
        } else {
            let last = segs.pop().expect("segs 非空");
            (Some(segs.join(".")), last)
        };
        let ty = if self.lex.eat_char(':') { Some(self.scan_type_text()) } else { None };
        let alias = self.parse_arrow_alias()?;
        Ok((PacketKey { pkg, name, ty, span: self.lex.span(start, self.lex.pos()) }, alias))
    }

    /// `-> <ident>`（可选）。
    fn parse_arrow_alias(&mut self) -> Result<Option<Ident>, Diagnostic> {
        if !self.lex.peek_str("->") {
            return Ok(None);
        }
        self.lex.bump();
        self.lex.bump();
        let (id, _) = self.lex.read_ident()?;
        Ok(Some(id))
    }

    fn scan_type_text(&mut self) -> RawRust {
        match self.lex.scan_raw(Stops::TYPE) {
            Ok(r) => self.lex.raw_trimmed(r.start, r.end),
            Err(_) => RawRust { text: String::new(), span: self.lex.span(self.lex.pos(), self.lex.pos()) },
        }
    }

    // ---------------------------------------------------------------- jump

    /// `jump [loop|recurse|and] <targets> [when] [mode] [return] [over] [concurrent_limit] [group];`
    ///
    /// 段序固定（§8.3 规则 2），任何一段解析失败给出带位置的 RSC-E033。
    fn parse_jump(&mut self) -> Stmt {
        let start = self.lex.pos();
        self.lex.eat_word("jump");
        // 段解析失败时已跳到下一个 `;`，收尾检查不再重复报错
        self.jump_recovered = false;

        // A3：`jump sync <ident>;` 是独立语句
        if self.lex.eat_word("sync") {
            let group = match self.lex.read_ident() {
                Ok((g, _)) => g,
                Err(_) => {
                    self.e033("jump 语句解析失败：`jump sync` 之后必须是 group 名");
                    self.lex.skip_to_semi();
                    return Stmt::Sync { group: String::new(), span: self.lex.span(start, self.lex.pos()) };
                }
            };
            if let Err(d) = self.end_stmt() {
                self.e033_from(d, "jump 语句解析失败：`jump sync` 缺少结尾 `;`");
            }
            return Stmt::Sync { group, span: self.lex.span(start, self.lex.pos()) };
        }

        let kind = if self.lex.eat_word("loop") {
            JumpKind::Loop
        } else if self.lex.eat_word("recurse") {
            JumpKind::Recurse
        } else if self.lex.eat_word("and") {
            JumpKind::And
        } else {
            JumpKind::Plain
        };

        let targets = self.parse_target_list();
        let condition = self.parse_when_segment();
        let (mode, args) = self.parse_mode_segment();
        let ret_map = self.parse_return_segment();
        let over = self.parse_over_segment();
        let concurrent_limit = self.parse_limit_segment();
        let group = self.parse_group_segment();

        match self.lex.skip_trivia() {
            Ok(()) => {}
            Err(d) => {
                self.e033_from(d, "jump 语句解析失败：词法错误");
                return Stmt::Jump(JumpStmt {
                    kind,
                    targets,
                    condition,
                    mode,
                    args,
                    ret_map,
                    over,
                    concurrent_limit,
                    group,
                    span: self.lex.span(start, self.lex.pos()),
                });
            }
        }
        match self.lex.peek_char() {
            Some(';') => {
                self.lex.bump();
            }
            // 块内 match 分支体后面可以没有分号（见 end_stmt）
            Some(',') | Some('}') if self.in_block => {}
            None => {
                if !self.jump_recovered {
                    self.e033("jump 语句解析失败：缺少结尾 `;`");
                }
            }
            _ => {
                // 收尾还剩 token：若它正好是**某个已知段的开头**，那就不该报成"多余的
                // token"，而是段序写错（§7.4）—— 点名是哪个段、给出正确段序，用户才改得动。
                match self.misplaced_segment_kw() {
                    Some(kw) => self.e033(format!(
                        "jump 语句解析失败：`{kw}` 这一段的位置不对，段序固定为 \
                         `jump [loop|recurse|and] <目标> [when <expr>] \
                         [go with(..) | read with(..) | force go with(..) | force read with(..)] \
                         [return(..)] [over <集合> [|> <算子>(..)]] [concurrent_limit = <expr>] \
                         [group <名>] ;`"
                    )),
                    None => self.e033("jump 语句解析失败：出现多余的 token"),
                }
                self.lex.skip_to_semi();
            }
        }

        Stmt::Jump(JumpStmt {
            kind,
            targets,
            condition,
            mode,
            args,
            ret_map,
            over,
            concurrent_limit,
            group,
            span: self.lex.span(start, self.lex.pos()),
        })
    }

    fn parse_target_list(&mut self) -> Vec<Ident> {
        let mut targets = Vec::new();
        loop {
            match self.lex.read_ident() {
                Ok((n, _)) => targets.push(n),
                Err(_) => {
                    self.e033("jump 语句解析失败：缺少跳转目标 Cell 名");
                    break;
                }
            }
            if !self.lex.eat_char(',') {
                break;
            }
        }
        targets
    }

    fn parse_when_segment(&mut self) -> Option<RawRust> {
        if !self.lex.eat_word("when") {
            return None;
        }
        match self.lex.scan_raw(Stops::WHEN_EXPR) {
            Ok(r) => Some(self.lex.raw_trimmed(r.start, r.end)),
            Err(d) => {
                self.e033_from(d, "jump 语句解析失败：`when` 表达式未闭合");
                None
            }
        }
    }

    fn parse_mode_segment(&mut self) -> (Option<TransMode>, Vec<Arg>) {
        let mode = if self.lex.eat_phrase("force go with") {
            Some(TransMode::ForceGoWith)
        } else if self.lex.eat_phrase("force read with") {
            Some(TransMode::ForceReadWith)
        } else if self.lex.eat_phrase("go with") {
            Some(TransMode::GoWith)
        } else if self.lex.eat_phrase("read with") {
            Some(TransMode::ReadWith)
        } else {
            return (None, Vec::new());
        };
        if let Err(d) = self.lex.expect_char('(') {
            self.seg_fail_from(d, "jump 语句解析失败：传输模式后必须是 `(`");
            return (mode, Vec::new());
        }
        let args = self.parse_arg_list();
        if let Err(d) = self.lex.expect_char(')') {
            self.seg_fail_from(d, "jump 语句解析失败：实参列表缺少 `)`");
        }
        (mode, args)
    }

    /// `(<expr> [-> <ident>] , ...)`，按 depth 0 的逗号切分（§8.3 规则 3）。
    fn parse_arg_list(&mut self) -> Vec<Arg> {
        let mut args: Vec<Arg> = Vec::new();
        loop {
            if let Err(d) = self.lex.skip_trivia() {
                self.push(d);
                break;
            }
            if self.lex.at_eof() {
                break;
            }
            if self.lex.peek_char() == Some(')') {
                break;
            }
            if self.lex.eat_char(',') {
                continue; // 容忍多余逗号
            }
            let start = self.lex.pos();
            let r = match self.lex.scan_raw(Stops::ARG) {
                Ok(r) => r,
                Err(d) => {
                    self.e033_from(d, "jump 语句解析失败：实参表达式未闭合");
                    break;
                }
            };
            let end = r.end;
            let key = match self.parse_arrow_alias() {
                Ok(k) => k,
                Err(d) => {
                    self.e033_from(d, "jump 语句解析失败：非法实参");
                    None
                }
            };
            let expr = self.lex.raw_trimmed(start, end);
            if !expr.text.is_empty() || key.is_some() {
                args.push(Arg { expr, key, span: self.lex.span(start, self.lex.pos()) });
            }
            match r.stop {
                StopKind::Comma => {
                    self.lex.bump();
                }
                StopKind::CloseParen => break,
                StopKind::DashGt => {
                    // `-> <key>` 已被 parse_arrow_alias 消费，继续下一项
                    if self.lex.pos() == start {
                        self.lex.bump();
                    }
                }
                _ => {
                    self.seg_fail("jump 语句解析失败：实参之间缺少 `,`");
                    break;
                }
            }
        }
        args
    }

    fn parse_return_segment(&mut self) -> Vec<(PacketKey, Ident)> {
        let mut ret_map: Vec<(PacketKey, Ident)> = Vec::new();
        if !self.lex.eat_word("return") {
            return ret_map;
        }
        if let Err(d) = self.lex.expect_char('(') {
            self.seg_fail_from(d, "jump 语句解析失败：`return` 后必须是 `(`");
            return ret_map;
        }
        loop {
            if let Err(d) = self.lex.skip_trivia() {
                self.push(d);
                break;
            }
            if self.lex.at_eof() || self.lex.peek_char() == Some(')') {
                break;
            }
            if self.lex.eat_char(',') {
                continue;
            }
            let before = self.lex.pos();
            match self.parse_packet_key() {
                Ok((key, target)) => {
                    let target = target.unwrap_or_else(|| {
                        self.e033("jump 语句解析失败：`return` 映射项缺少 `-> <变量名>`");
                        String::new()
                    });
                    ret_map.push((key, target));
                }
                Err(d) => self.e033_from(d, "jump 语句解析失败：非法 `return` 映射项"),
            }
            if self.lex.pos() == before {
                self.lex.bump();
            }
        }
        if let Err(d) = self.lex.expect_char(')') {
            self.seg_fail_from(d, "jump 语句解析失败：`return(...)` 缺少 `)`");
        }
        ret_map
    }

    fn parse_over_segment(&mut self) -> Option<OverSpec> {
        if !self.lex.eat_word("over") {
            return None;
        }
        let mut r = match self.lex.scan_raw(Stops::OVER_COLLECTION) {
            Ok(r) => r,
            Err(d) => {
                self.e033_from(d, "jump 语句解析失败：`over` 集合表达式未闭合");
                return None;
            }
        };
        let collection = self.lex.raw_trimmed(r.start, r.end);
        let mut ops: Vec<OverOp> = Vec::new();
        while r.stop == StopKind::PipeGt {
            self.lex.bump(); // |
            self.lex.bump(); // >
            let op = if self.lex.eat_word("filter") {
                OverOpKind::Filter
            } else if self.lex.eat_word("map") {
                OverOpKind::Map
            } else if self.lex.eat_word("flatmap") {
                OverOpKind::FlatMap
            } else if self.lex.eat_word("take") {
                OverOpKind::Take
            } else if self.lex.eat_word("skip") {
                OverOpKind::Skip
            } else {
                self.seg_fail("jump 语句解析失败：`over` 管道算子必须是 filter / map / flatmap / take / skip");
                break;
            };
            if let Err(d) = self.lex.expect_char('(') {
                self.seg_fail_from(d, "jump 语句解析失败：`over` 算子后必须是 `(`");
                break;
            }
            let arg = match self.lex.scan_raw(Stops::PAREN_EXPR) {
                Ok(a) => a,
                Err(d) => {
                    self.e033_from(d, "jump 语句解析失败：`over` 算子实参未闭合");
                    break;
                }
            };
            let expr = self.lex.raw_trimmed(arg.start, arg.end);
            if let Err(d) = self.lex.expect_char(')') {
                self.seg_fail_from(d, "jump 语句解析失败：`over` 算子实参缺少 `)`");
                break;
            }
            ops.push(match op {
                OverOpKind::Filter => OverOp::Filter(expr),
                OverOpKind::Map => OverOp::Map(expr),
                OverOpKind::FlatMap => OverOp::FlatMap(expr),
                OverOpKind::Take => OverOp::Take(expr),
                OverOpKind::Skip => OverOp::Skip(expr),
            });
            r = match self.lex.scan_raw(Stops::OVER_COLLECTION) {
                Ok(nr) => nr,
                Err(d) => {
                    self.e033_from(d, "jump 语句解析失败：`over` 管道未闭合");
                    break;
                }
            };
        }
        Some(OverSpec { collection, ops })
    }

    fn parse_limit_segment(&mut self) -> Option<RawRust> {
        if !self.lex.eat_word("concurrent_limit") {
            return None;
        }
        if !self.lex.eat_char('=') {
            self.seg_fail("jump 语句解析失败：`concurrent_limit` 后必须是 `=`");
            return None;
        }
        match self.lex.scan_raw(Stops::CONCURRENT_LIMIT) {
            Ok(r) => Some(self.lex.raw_trimmed(r.start, r.end)),
            Err(d) => {
                self.e033_from(d, "jump 语句解析失败：`concurrent_limit` 表达式未闭合");
                None
            }
        }
    }

    fn parse_group_segment(&mut self) -> Option<Ident> {
        if !self.lex.eat_word("group") {
            return None;
        }
        match self.lex.read_ident() {
            Ok((g, _)) => Some(g),
            Err(_) => {
                self.seg_fail("jump 语句解析失败：`group` 后必须是 group 名");
                None
            }
        }
    }

    /// jump 收尾处剩下的 token 是否是**某个已知段的开头**（= 段序写错，§7.4）。
    ///
    /// 每个候选都在**同一个位置**试匹配，所以表内顺序无关：`force go with(..)` 处试
    /// `go with` 会在首词就失败，不会把 `force` 吃掉。词边界由 `match_word` 保证，
    /// 因此 `over returned_items` 里的 `return` 前缀不会被认成一个段。
    fn misplaced_segment_kw(&mut self) -> Option<&'static str> {
        const SEGMENTS: &[&str] = &[
            "force go with",
            "force read with",
            "go with",
            "read with",
            "return",
            "over",
            "concurrent_limit",
            "group",
            "when",
        ];
        SEGMENTS.iter().copied().find(|kw| self.lex.peek_phrase_str(kw))
    }

    // -------------------------------------------------- exit / die / out

    fn parse_exit(&mut self) -> Stmt {
        let start = self.lex.pos();
        self.lex.eat_phrase("exit go with");
        if let Err(d) = self.lex.expect_char('(') {
            self.push(d);
            return Stmt::Exit { args: Vec::new(), span: self.lex.span(start, self.lex.pos()) };
        }
        let args = self.parse_arg_list();
        if let Err(d) = self.lex.expect_char(')') {
            self.push(d);
        }
        if let Err(d) = self.lex.expect_semi() {
            self.push(d);
        }
        Stmt::Exit { args, span: self.lex.span(start, self.lex.pos()) }
    }

    fn parse_die(&mut self) -> Stmt {
        let start = self.lex.pos();
        self.lex.eat_word("die");
        if let Err(d) = self.end_stmt() {
            self.push(d);
        }
        Stmt::Die { span: self.lex.span(start, self.lex.pos()) }
    }

    fn parse_out(&mut self) -> Stmt {
        let start = self.lex.pos();
        self.lex.eat_word("out");
        let mut args: Vec<RawRust> = Vec::new();
        if let Err(d) = self.lex.expect_char('(') {
            self.push(d);
        } else {
            loop {
                if let Err(d) = self.lex.skip_trivia() {
                    self.push(d);
                    break;
                }
                if self.lex.at_eof() || self.lex.peek_char() == Some(')') {
                    break;
                }
                if self.lex.eat_char(',') {
                    continue;
                }
                let arg_start = self.lex.pos();
                let r = match self.lex.scan_raw(Stops::ARG) {
                    Ok(r) => r,
                    Err(d) => {
                        self.push(d);
                        break;
                    }
                };
                args.push(self.lex.raw_trimmed(arg_start, r.end));
                match r.stop {
                    StopKind::Comma => {
                        self.lex.bump();
                    }
                    StopKind::CloseParen => break,
                    _ => {
                        self.push(Diagnostic::error(
                            code::E030,
                            "语法错误：`out(...)` 实参之间缺少 `,`",
                            self.span_at(self.lex.pos()),
                        ));
                        self.lex.skip_to_semi();
                        break;
                    }
                }
            }
            if let Err(d) = self.lex.expect_char(')') {
                self.push(d);
            }
        }
        if let Err(d) = self.lex.expect_semi() {
            self.push(d);
        }
        Stmt::Out { args, span: self.lex.span(start, self.lex.pos()) }
    }

    /// Cell 内 `extern rust [read] { .. };`
    ///
    /// `body` 是**花括号内部**的逐字切片（字段名语义如此），`span` 覆盖整条语句
    /// （含 `extern rust` 前缀与花括号），供 §15.2 的 `in_extern` 源映射使用。
    fn parse_extern_rust_block(&mut self) -> Stmt {
        let start = self.lex.pos();
        self.lex.eat_phrase("extern rust");
        let read_only = self.lex.eat_word("read");
        // scan_braced_block 自己消费 `{`，不要重复 expect
        let block = match self.lex.scan_braced_block() {
            Ok(b) => b,
            Err(d) => {
                self.push(d);
                return Stmt::ExternRustBlock {
                    read_only,
                    body: self.lex.raw(start, self.lex.pos()),
                    span: self.lex.span(start, self.lex.pos()),
                };
            }
        };
        // body 只取花括号内部（block 是 `{...}` 的逐字切片）
        let end = self.lex.pos();
        let inner_start = end - block.text.len() + 1; // 去掉 `{`
        let inner_end = end - 1; // 去掉 `}`
        let body = self.lex.raw_trimmed(inner_start, inner_end);
        self.lex.eat_semi();
        Stmt::ExternRustBlock { read_only, body, span: self.lex.span(start, end) }
    }

    // ------------------------------------------------------- Rust 透传

    /// Cell 内一条 Rust 透传语句（§3.5.1 规则 2 + §3.5.2 块内递归分派）。
    fn parse_raw_stmt(&mut self) -> Result<Stmt, Diagnostic> {
        let start = self.lex.pos();
        let r = self.lex.scan_raw(Stops::STMT_HEAD)?;
        match r.stop {
            StopKind::Semi => {
                self.lex.bump();
                Ok(Stmt::RawRust(self.lex.raw_trimmed(start, self.lex.pos())))
            }
            StopKind::Eof => Ok(Stmt::RawRust(self.lex.raw_trimmed(start, r.end))),
            StopKind::OpenBrace => {
                let head = self.lex.raw(start, r.end);
                if block_is_dispatchable(&head.text) {
                    let (block, end) = self.parse_raw_block(head, r.end)?;
                    // 块后多余 `;` 忽略（§3.1 规则 3）
                    let end = self.eat_trailing_semi(end, is_ctrl_flow_head(&self.src[start..r.end]));
                    if block.is_pure() {
                        // 块内无 DSL 语句：整条合并为一个逐字切片（A1 的 100% 还原）
                        return Ok(Stmt::RawRust(self.lex.raw_trimmed(start, end)));
                    }
                    Ok(Stmt::RustBlock { block })
                } else {
                    self.lex.scan_braced_block()?;
                    let end = self.lex.pos();
                    let end = self.eat_trailing_semi(end, is_ctrl_flow_head(&self.src[start..r.end]));
                    Ok(Stmt::RawRust(self.lex.raw_trimmed(start, end)))
                }
            }
            other => Err(Diagnostic::error(
                code::E030,
                format!("语法错误：语句意外终止（{other:?}），括号可能不配平"),
                self.lex.span(r.end, r.end),
            )),
        }
    }

    /// 解析"已进入 DSL 分派"的 Rust 块：`{ <parts> }`，并继续扫描其后的连接文本。
    ///
    /// `head_start` 是块前文本的起点，`head` 是块前的**逐字**文本。返回块与整条语句的
    /// 结束偏移（结束偏移在尾随 `;` 之前，§3.1 规则 3）。
    fn parse_raw_block(&mut self, head: RawRust, head_start: usize) -> Result<(RawBlock, usize), Diagnostic> {
        let mut parts: Vec<RawBlockPart> = Vec::new();
        if !head.text.is_empty() {
            parts.push(RawBlockPart::Text(head));
        }
        loop {
            let open_at = self.lex.pos();
            if !self.lex.eat_char('{') {
                // 调用方保证位于 `{` 之前；防御性返回
                let end = self.lex.pos();
                return Ok((RawBlock { parts, span: self.lex.span(head_start, end) }, end));
            }
            let saved_in_block = std::mem::replace(&mut self.in_block, true);
            let parsed = self.parse_block_parts(open_at);
            self.in_block = saved_in_block;
            let stmts = parsed?;
            let block_end = self.lex.pos();
            let closed = self.lex.eat_char('}');
            parts.push(RawBlockPart::Stmt(Stmt::RustBlock {
                block: RawBlock { parts: stmts, span: self.lex.span(open_at, block_end) },
            }));
            if !closed {
                let end = block_end;
                return Ok((RawBlock { parts, span: self.lex.span(head_start, end) }, end));
            }
            // 尾部：扫描到下一个 `{`（else / 后续块）或 `;` / 文件尾
            let r = self.lex.scan_raw(Stops::STMT_HEAD)?;
            let tail = self.lex.raw(r.start, r.end);
            // 纯空白尾部（块后的换行/缩进）不进入 parts：它不属于块语句本身
            if !tail.text.trim().is_empty() {
                parts.push(RawBlockPart::Text(tail));
            }
            if r.stop == StopKind::OpenBrace {
                continue;
            }
            return Ok((RawBlock { parts, span: self.lex.span(head_start, r.end) }, r.end));
        }
    }

    /// 解析 `{ .. }` 内部：DSL 语句、透传文本、`=>` match 分支头、嵌套块混排。
    fn parse_block_parts(&mut self, open_at: usize) -> Result<Vec<RawBlockPart>, Diagnostic> {
        let mut parts: Vec<RawBlockPart> = Vec::new();
        loop {
            self.lex.skip_trivia()?;
            if self.lex.at_eof() {
                return Err(Diagnostic::error(
                    code::E030,
                    "词法错误：未闭合的 Rust 块，缺少配平的 `}`",
                    self.lex.span(open_at, open_at),
                ));
            }
            if self.lex.peek_char() == Some('}') {
                return Ok(parts);
            }
            if self.lex.eat_char(';') {
                continue; // 忽略空语句
            }
            let before = self.lex.pos();
            match self.parse_block_part() {
                Ok(Some(part)) => {
                    if !is_empty_part(&part) {
                        parts.push(part);
                    }
                }
                Ok(None) => self.recover_stmt(),
                Err(d) => {
                    self.push(d);
                    self.recover_stmt();
                    if self.lex.peek_char() == Some('}') || self.lex.at_eof() {
                        return Ok(parts);
                    }
                }
            }
            if self.lex.pos() == before {
                self.lex.bump();
            }
        }
    }

    /// 块内一个片段（透传文本 / DSL 语句 / `=>` 分支头 / 嵌套块）。
    fn parse_block_part(&mut self) -> Result<Option<RawBlockPart>, Diagnostic> {
        let kw = self.lex.peek_stmt_start()?;
        if let Some(k) = kw {
            if matches!(k, "jump" | "exit go with" | "die" | "out" | "extern rust") {
                let s = self.parse_stmt(kw)?;
                return Ok(s.map(RawBlockPart::Stmt));
            }
            if is_orphan_kw(k) {
                let at = self.lex.pos();
                self.push(
                    Diagnostic::error(
                        code::E031,
                        format!("孤立关键字：`{k}` 不构成合法 DSL 语句"),
                        self.span_at(at),
                    )
                    .with_detail("§7.4：孤立的 go / with / exit 出现在语句起始位置"),
                );
                return Ok(None);
            }
            if matches!(k, "cell" | "entry" | "receive") {
                let at = self.lex.pos();
                self.push(
                    Diagnostic::error(
                        code::E031,
                        format!("孤立关键字：`{k}` 不能出现在 Rust 块内"),
                        self.span_at(at),
                    )
                    .with_detail("§3.4.4：块内允许 `extern rust {..}` 逃逸口；cell/entry 只能出现在顶层"),
                );
                return Ok(None);
            }
        }
        // 其余一律 Rust 透传；`=>` 是 match 分支头（消费后继续分派，§3.5.2）
        let start = self.lex.pos();
        let r = self.lex.scan_raw(Stops {
            semi: true,
            open_brace: true,
            comma: true,
            arrow: true,
            ..Stops::NONE
        })?;        match r.stop {
            StopKind::Semi => {
                self.lex.bump();
                Ok(Some(RawBlockPart::Text(self.lex.raw_trimmed(start, self.lex.pos()))))
            }
            StopKind::Eof => Ok(Some(RawBlockPart::Text(self.lex.raw_trimmed(start, r.end)))),
            StopKind::OpenBrace => {
                let head = self.lex.raw(start, r.end);
                if block_is_dispatchable(&head.text) {
                    let (block, _end) = self.parse_raw_block(head, r.end)?;
                    Ok(Some(RawBlockPart::Stmt(Stmt::RustBlock { block })))
                } else {
                    self.lex.scan_braced_block()?;
                    let stop = self.lex.pos();
                    Ok(Some(RawBlockPart::Text(RawRust {
                        text: self.src[start..stop].trim().to_string(),
                        span: self.lex.span(start, stop),
                    })))
                }
            }
            StopKind::Arrow => {
                // match 分支头：前面已产出一段文本，这里只消费 `=>`
                self.lex.bump();
                self.lex.bump();
                Ok(Some(RawBlockPart::Text(self.lex.raw(r.start, self.lex.pos()))))
            }
            StopKind::CloseBrace => {
                // 语句到块的 `}` 为止：不消费（由 parse_block_parts 收尾）
                Ok(None)
            }
            StopKind::Comma => {
                // 分支之间的逗号：并入同一片段，保持顺序与逐字可还原
                let mut text = self.lex.raw_trimmed(start, r.end).text;
                text.push(',');
                self.lex.bump();
                Ok(Some(RawBlockPart::Text(RawRust {
                    text,
                    span: self.lex.span(start, r.end + 1),
                })))
            }
            other => Err(Diagnostic::error(
                code::E030,
                format!("语法错误：块内片段意外终止（{other:?}），括号可能不配平"),
                self.lex.span(r.end, r.end),
            )),
        }
    }

    /// 语句结束符（§3.1 规则 1：分号是唯一语句分隔符）。
    ///
    /// **块内放宽**：`match` 分支体位于表达式位置（Rust 也不要求分号），故 Rust 块内部
    /// 遇到 depth 0 的 `,` / `}` 时视为语句结束且**不消费**（交给块的分派循环处理）。
    fn end_stmt(&mut self) -> Result<(), Diagnostic> {
        self.lex.skip_trivia()?;
        match self.lex.peek_char() {
            Some(';') => {
                self.lex.bump();
                Ok(())
            }
            Some(c) if self.in_block && (c == ',' || c == '}') => Ok(()),
            _ => Err(Diagnostic::error(
                code::E030,
                "语法错误：期望 `;` 结束语句",
                self.span_at(self.lex.pos()),
            )),
        }
    }

    /// 块语句收尾（§3.1 规则 3：Rust 块语句以配平的 `}` 结束，可带可选尾分号）。
    ///
    /// **裁定依据 A1**：`if`/`while`/`for`/`loop`/`match` 等控制流块之后的尾随 `;`
    /// 是"多余的"，只消费不计入 `text`；而 `let q = quote! { .. };`、`let g = |x| { .. };`
    /// 这类语句的分号是语句的一部分，计入 `text`（保持逐字切片）。
    fn eat_trailing_semi(&mut self, end: usize, is_ctrl_flow: bool) -> usize {
        if self.lex.eat_semi() {
            if is_ctrl_flow {
                end
            } else {
                self.lex.pos()
            }
        } else {
            end
        }
    }

    // ------------------------------------------------------- 错误恢复

    /// 顶层错误恢复：跳到下一个 depth 0 的 `;` 之后，或下一个 item 起始关键字之前。
    fn recover_item(&mut self) {
        loop {
            if self.lex.at_eof() {
                return;
            }
            if self.lex.peek_char() == Some(';') {
                self.lex.bump();
                return;
            }
            if let Ok(Some(kw)) = self.lex.peek_stmt_start() {
                if matches!(kw, "cell" | "entry" | "extern rust") || is_rust_item_kw(kw) {
                    return;
                }
            }
            if self.lex.bump().is_none() {
                return;
            }
        }
    }

    /// 语句级错误恢复：跳到下一个 depth 0 的 `;` 之后 / `}` 之前 / 下一个 DSL 语句之前。
    fn recover_stmt(&mut self) {
        loop {
            if self.lex.at_eof() {
                return;
            }
            if self.lex.peek_char() == Some(';') {
                self.lex.bump();
                return;
            }
            if self.lex.peek_char() == Some('}') {
                return;
            }
            if let Ok(Some(kw)) = self.lex.peek_stmt_start() {
                if matches!(
                    kw,
                    "jump" | "exit go with" | "die" | "out" | "cell" | "entry" | "extern rust"
                ) {
                    return;
                }
            }
            if self.lex.bump().is_none() {
                return;
            }
        }
    }

    // -------------------------------------------------------- 诊断工具

    fn span_at(&self, off: usize) -> Span {
        self.lex.span(off, off)
    }

    fn push(&mut self, d: Diagnostic) {
        let mut d = d;
        if d.cell.is_none() {
            d.cell = self.cell.clone();
        }
        self.diags.push(d);
    }

    fn e033(&mut self, msg: impl Into<String>) {
        let at = self.lex.pos();
        self.push(
            Diagnostic::error(code::E033, msg, self.span_at(at))
                .with_detail("§8.3 规则 2：jump 语句按固定段序解析，任一段失败给出 RSC-E033"),
        );
    }

    fn e033_from(&mut self, d: Diagnostic, msg: impl Into<String>) {
        let span = d.span;
        self.push(Diagnostic::error(code::E033, msg, span).with_detail(d.message));
    }

    /// jump 某个可选段解析失败：报 E033 并跳到下一个 `;`（收尾检查据此不重复报错）。
    fn seg_fail(&mut self, msg: impl Into<String>) {
        self.e033(msg);
        self.lex.skip_to_semi();
        self.jump_recovered = true;
    }

    /// [`Parser::seg_fail`] 的词法错误版本。
    fn seg_fail_from(&mut self, d: Diagnostic, msg: impl Into<String>) {
        self.e033_from(d, msg);
        self.jump_recovered = true;
    }
}

enum OverOpKind {
    Filter,
    Map,
    FlatMap,
    Take,
    Skip,
}

// ------------------------------------------------------------------ 工具

/// 顶层 Rust item 起始关键字（§3.5.4 + 文档扩展：pub/async/unsafe/extern/macro_rules）。
fn is_rust_item_kw(kw: &str) -> bool {
    RUST_ITEM_ALL.contains(&kw)
}

/// 语句起始位置的孤立关键字（主会话裁定 A5）。
fn is_orphan_kw(kw: &str) -> bool {
    ORPHAN_KEYWORDS.contains(&kw)
}

/// 块的"头部文本"是否意味着块内要重新做 DSL 分派（§3.5.2）。
///
/// 判定依据：只有**控制流块**（`if` / `else` / `while` / `for` / `loop` / `match`）与
/// 匿名块（头文本为空）内部才可能出现 DSL 语句；`fn` / `impl` / `mod` / 闭包 / 结构体
/// 字面量 / `macro!{...}` 的块是纯 Rust，必须整体不透明。首词按 §3.5.3 词边界取。
fn block_is_dispatchable(head: &str) -> bool {
    let t = head.trim();
    if t.is_empty() {
        return true;
    }
    match first_word(t) {
        Some(w) => matches!(w, "if" | "else" | "while" | "for" | "loop" | "match"),
        None => false,
    }
}

/// 头文本是否以控制流关键字开头（决定尾随 `;` 是否计入 `text`，见 A1）。
fn is_ctrl_flow_head(head: &str) -> bool {
    match first_word(head.trim()) {
        Some(w) => matches!(w, "if" | "else" | "while" | "for" | "loop" | "match"),
        None => false,
    }
}

fn first_word(s: &str) -> Option<&str> {    let mut i = 0usize;
    while i < s.len() {
        let c = s[i..].chars().next()?;
        if c.is_alphanumeric() || c == '_' {
            i += c.len_utf8();
        } else {
            break;
        }
    }
    if i == 0 {
        return None;
    }
    Some(&s[..i])
}

fn is_empty_part(p: &RawBlockPart) -> bool {
    match p {
        RawBlockPart::Text(t) => t.text.is_empty(),
        RawBlockPart::Stmt(_) => false,
    }
}
