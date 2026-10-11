//! 词法分析器（规格 §7、§3.5）。
//!
//! **按需驱动**（§7.3）：Lexer 不产出完整 token 流，而是由 Parser 在需要"下一条语句"时
//! 调用 [`Lexer::peek_stmt_keyword`] 判断起始 token 是否命中 DSL 关键字；未命中则由
//! [`Lexer::scan_raw`] / [`Lexer::scan_braced_block`] 按 §3.5 规则把整段 Rust 文本切片出去。
//!
//! 索引约定：全部按**字节**记录，但只按 `char` 推进，故任何切片都落在 UTF-8 字符边界上；
//! [`Lexer::raw`] / [`Lexer::raw_trimmed`] 另做一次向内对齐兜底，保证"绝不 panic"。

use crate::ast::RawRust;
use crate::diag::{code, Diagnostic};
use crate::span::{align_range, span_between, trim_range, Span};

/// §3.5.4：顶层 Rust item 起始关键字（文档原文 10 个）。
pub const RUST_ITEM_KEYWORDS: &[&str] =
    &["use", "struct", "enum", "fn", "trait", "impl", "const", "static", "type", "mod"];

/// §3.5.4 **文档扩展**（主会话裁定允许的补充）：真实 Rust 顶层 item 还可能以
/// `pub` / `async` / `unsafe` / `extern "C"` / `macro_rules!` 开头。§3.5.4 的关键字集合
/// 不全，会把这些完全合法的 Rust item 判成 RSC-E032。扩展只放宽"识别为 Rust 透传"的
/// 起始词集合，不放宽 DSL 关键字识别，风险为零。
pub const RUST_ITEM_KEYWORDS_EXT: &[&str] = &["pub", "async", "unsafe", "extern", "macro_rules"];

/// `RUST_ITEM_KEYWORDS` + `RUST_ITEM_KEYWORDS_EXT`（语句起始分派用的合并集合）。
pub const RUST_ITEM_ALL: &[&str] = &[
    "use", "struct", "enum", "fn", "trait", "impl", "const", "static", "type", "mod", "pub", "async",
    "unsafe", "extern", "macro_rules",
];

/// 关键字表（§3.2 / §7.2）。多词关键字**整体匹配**，按词序 + 词边界，
/// 顺序即优先级（长词在前）。
const KEYWORD_PHRASES: &[(&str, &[&str])] = &[
    ("extern rust", &["extern", "rust"]),
    ("exit go with", &["exit", "go", "with"]),
    ("force go with", &["force", "go", "with"]),
    ("force read with", &["force", "read", "with"]),
    ("go with", &["go", "with"]),
    ("read with", &["read", "with"]),
    ("concurrent_limit", &["concurrent_limit"]),
    ("cell", &["cell"]),
    ("receive", &["receive"]),
    ("as", &["as"]),
    ("jump", &["jump"]),
    ("loop", &["loop"]),
    ("recurse", &["recurse"]),
    ("and", &["and"]),
    ("sync", &["sync"]),
    ("group", &["group"]),
    ("when", &["when"]),
    ("return", &["return"]),
    ("die", &["die"]),
    ("out", &["out"]),
    ("over", &["over"]),
    ("force", &["force"]),
    ("entry", &["entry"]),
    ("go", &["go"]),
    ("with", &["with"]),
    ("exit", &["exit"]),
    ("read", &["read"]),
];

/// 单词（单个标识符）在句首出现时即构成"孤立关键字"（主会话裁定 A5）。
///
/// 判定依据：这些词在 Rust 里都不是语句起始词，出现在语句起始位置只可能是 DSL 片段写错。
/// `return` / `loop` / `and` 等**不在**此列：`return x;` / `loop { }` 是合法 Rust 透传
/// （§3.5.1 的语句分派表也不含它们）。
pub const ORPHAN_KEYWORDS: &[&str] = &[
    "exit go with", "go with", "read with", "force go with", "force read with", "go", "with",
    "exit", "read", "force",
];

/// 是否是 DSL 保留字（§3.2：上表全部关键字 + `let` / `mut`）。
/// 用于 `read_ident` 拒绝把关键字当变量名（§3.2 明确"用户变量名不得使用"）。
pub fn is_reserved_word(w: &str) -> bool {
    KEYWORD_PHRASES
        .iter()
        .any(|(_, words)| words.len() == 1 && words[0] == w)
        || w == "let"
        || w == "mut"
}

/// 扫描停止的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopKind {
    /// depth 0 的 `;`
    Semi,
    /// depth 0 的 `,`
    Comma,
    /// depth 0 的 `)`
    CloseParen,
    /// depth 0 的 `]`
    CloseBracket,
    /// depth 0 的 `}`
    CloseBrace,
    /// depth 0 的 `{`
    OpenBrace,
    /// depth 0 的 `=>`
    Arrow,
    /// depth 0 的 `->`
    DashGt,
    /// depth 0 的 `|>`
    PipeGt,
    /// 命中 `Stops::words` 中的某个词组
    Keyword,
    /// 文件结束（depth 必须为 0，否则 `scan_raw` 直接报 E030）
    Eof,
}

/// 扫描停止条件。`)` / `]` / `}` 在 depth 0 时**总是**停止（返回对应 `StopKind`），
/// 由调用方判定"这是正常终止"还是"括号不配平"（E030）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stops {
    /// depth 0 的 `;` 是否停止。
    pub semi: bool,
    /// depth 0 的 `,` 是否停止。
    pub comma: bool,
    /// depth 0 的 `{` 是否停止（不消费）。
    pub open_brace: bool,
    /// depth 0 的 `=>` 是否停止。
    pub arrow: bool,
    /// depth 0 的 `->` 是否停止（实参的显式键名分隔符，§8.3 规则 3）。
    pub dash_gt: bool,
    /// depth 0 的 `|>` 是否停止。
    pub pipe_gt: bool,
    /// 命中任一词组（空格分隔的多词组合）即停止，不消费。
    pub words: &'static [&'static str],
}

impl Stops {
    /// 全空停止集。
    pub const NONE: Stops = Stops {
        semi: false,
        comma: false,
        open_brace: false,
        arrow: false,
        dash_gt: false,
        pipe_gt: false,
        words: &[],
    };
    /// 语句头扫描：到 `;` 或 `{` 为止（§3.5.1 的 Rust 透传语句边界）。
    pub const STMT_HEAD: Stops = Stops { semi: true, open_brace: true, ..Stops::NONE };
    /// `when <expr>`：到 `;` 或下一个 jump 段关键字为止（§8.3 规则 2 的固定段序）。
    pub const WHEN_EXPR: Stops = Stops {
        semi: true,
        words: &[
            "go with",
            "read with",
            "force go with",
            "force read with",
            "return",
            "over",
            "concurrent_limit",
            "group",
        ],
        ..Stops::NONE
    };
    /// `over <collection>`：到 `;` / `|>` / 后续段关键字为止。
    ///
    /// 停止词里必须包含 **mode 段与 return 段的关键字**（`go with` / `read with` /
    /// `force go with` / `force read with` / `return`）：段序固定（规范 §6.6、§7.4），
    /// 这些段只允许出现在 `over` **之前**。少一个，用户写错顺序时那一段就被当成
    /// 集合表达式吞掉，生成 `items read with(cfg).into_iter()` 这种非法 Rust，
    /// 而 `rscel check` 只会报一条误导的 RSC-E042（`docs/05` P1-1 记的缺陷）。
    /// `concurrent_limit` / `group` 本来就在表里，是同一个道理。
    pub const OVER_COLLECTION: Stops = Stops {
        semi: true,
        pipe_gt: true,
        words: &[
            "concurrent_limit",
            "group",
            "go with",
            "read with",
            "force go with",
            "force read with",
            "return",
        ],
        ..Stops::NONE
    };
    /// `concurrent_limit=<expr>`：到 `;` / `group` 为止。
    pub const CONCURRENT_LIMIT: Stops = Stops { semi: true, words: &["group"], ..Stops::NONE };
    /// 实参列表项：到 `,` / `)` / `->` 为止（`->` 后是显式键名，§8.3 规则 3）。
    pub const ARG: Stops = Stops { comma: true, dash_gt: true, ..Stops::NONE };
    /// 类型注解：到 `,` / `)` / `;` / `as` 为止（`as` 用于无括号形式切出别名子句）。
    pub const TYPE: Stops = Stops { semi: true, comma: true, words: &["as"], ..Stops::NONE };
    /// 括号内的表达式（闭包 / 算子实参）：到 `)` 为止。
    pub const PAREN_EXPR: Stops = Stops::NONE;
}

/// 一次 `scan_raw` 的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawScan {
    /// 起始字节偏移（调用 [`Lexer::skip_trivia`] 之后的位置）。
    pub start: usize,
    /// 结束字节偏移（不含停止符本身）。
    pub end: usize,
    /// 停止原因。
    pub stop: StopKind,
}

/// 扫描器。
#[derive(Clone)]
pub struct Lexer<'a> {
    src: &'a str,
    file: u32,
    pos: usize,
    /// 探测性扫描中吞掉的错误（未闭合字符串/注释等），由下一次非探测性
    /// [`Lexer::skip_trivia`] 上报，保证"错误不丢、也不重复报"。
    pending: Option<Diagnostic>,
}

impl<'a> Lexer<'a> {
    /// 新建扫描器。
    pub fn new(src: &'a str, file_id: u32) -> Self {
        Lexer { src, file: file_id, pos: 0, pending: None }
    }

    /// 当前字节偏移。
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// 重置到指定偏移（供探测性匹配回滚）。
    pub fn restore(&mut self, pos: usize) {
        self.pos = pos.min(self.src.len());
        while self.pos > 0 && !self.src.is_char_boundary(self.pos) {
            self.pos -= 1;
        }
    }

    /// 源文本。
    pub fn src(&self) -> &'a str {
        self.src
    }

    /// 文件 id。
    pub fn file(&self) -> u32 {
        self.file
    }

    /// 是否已到文件尾。
    pub fn at_eof(&self) -> bool {
        self.pos >= self.src.len()
    }

    /// 当前字符。
    pub fn peek_char(&self) -> Option<char> {
        self.char_at(self.pos)
    }

    /// 偏移 `off` 处的字符。偏移非字符边界时返回 `None`（绝不 panic）。
    pub fn char_at(&self, off: usize) -> Option<char> {
        if off >= self.src.len() || !self.src.is_char_boundary(off) {
            return None;
        }
        self.src[off..].chars().next()
    }

    /// 偏移 `off` 处的字节。
    pub fn byte_at(&self, off: usize) -> Option<u8> {
        self.src.as_bytes().get(off).copied()
    }

    /// 前进一个字符。
    pub fn bump(&mut self) -> Option<char> {
        let c = self.peek_char()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    /// 构造 span（`len` 精确到字节）。
    pub fn span(&self, start: usize, end: usize) -> Span {
        span_between(self.src, self.file, start, end)
    }

    /// 构造逐字切片（不做任何 trim）。
    pub fn raw(&self, start: usize, end: usize) -> RawRust {
        let (s, e) = align_range(self.src, start, end);
        RawRust { text: self.src[s..e].to_string(), span: self.span(s, e) }
    }

    /// 构造裁掉两端空白的切片（**裁定依据 A1**：允许 trim，不允许其它加工）。
    pub fn raw_trimmed(&self, start: usize, end: usize) -> RawRust {
        let (s, e) = trim_range(self.src, start, end);
        RawRust { text: self.src[s..e].to_string(), span: self.span(s, e) }
    }

    // ---------------------------------------------------------------- trivia

    /// 跳过空白与注释（`//`、可嵌套的 `/* */`）。未闭合块注释报 RSC-E030。
    pub fn skip_trivia(&mut self) -> Result<(), Diagnostic> {
        if let Some(d) = self.pending.take() {
            return Err(d);
        }
        while self.skip_trivia_impl()? {}
        Ok(())
    }

    /// 探测性跳 trivia：出错时把诊断压进 `pending`，等待下一次真实 `skip_trivia` 上报。
    fn skip_trivia_lossy(&mut self) -> bool {
        if self.pending.is_some() {
            return false;
        }
        loop {
            match self.skip_trivia_impl() {
                Ok(true) => {}
                Ok(false) => return true,
                Err(d) => {
                    self.pending = Some(d);
                    return false;
                }
            }
        }
    }

    fn skip_trivia_impl(&mut self) -> Result<bool, Diagnostic> {
        let mut any = false;
        loop {
            match self.skip_ws_comment()? {
                true => any = true,
                false => return Ok(any),
            }
        }
    }

    /// 只跳过空白与注释（§3.1 规则 4 的两种注释 + 空白）。
    fn skip_ws_comment(&mut self) -> Result<bool, Diagnostic> {
        let Some(c) = self.peek_char() else { return Ok(false) };
        if c.is_whitespace() {
            self.bump();
            return Ok(true);
        }
        let next = self.byte_at(self.pos + 1);
        match c {
            '/' if next == Some(b'/') => {
                self.pos += 2;
                while self.pos < self.src.len() && self.peek_char() != Some('\n') {
                    self.bump();
                }
                Ok(true)
            }
            '/' if next == Some(b'*') => self.skip_block_comment().map(|()| true),
            _ => Ok(false),
        }
    }

    /// 跳过"不参与结构分析"的词法单元：空白、注释、字符串、字符/生命周期、数字、
    /// 原始字符串（§3.5.3 清单）。
    ///
    /// **注意**：只在 `scan_raw` / `scan_braced_block` 这类"扫描一段透传文本"的路径里使用；
    /// [`Lexer::skip_trivia`] 只跳空白与注释，否则 `123` 会被当成 trivia 静默吞掉。
    fn skip_noise(&mut self) -> Result<bool, Diagnostic> {
        if self.skip_ws_comment()? {
            return Ok(true);
        }
        let Some(c) = self.peek_char() else { return Ok(false) };
        let next = self.byte_at(self.pos + 1);
        match c {
            '"' => self.skip_string().map(|()| true),
            '\'' => self.skip_char_or_lifetime().map(|()| true),
            'r' if next == Some(b'"') => self.skip_raw_string(0).map(|()| true),
            'r' if next == Some(b'#') => {
                let mut hashes = 0usize;
                let mut off = self.pos + 1;
                while self.byte_at(off) == Some(b'#') {
                    hashes += 1;
                    off += 1;
                }
                if self.byte_at(off) == Some(b'"') {
                    self.skip_raw_string(hashes).map(|()| true)
                } else {
                    // raw identifier（`r#match`）：当作标识符整体跳过
                    self.pos = off;
                    self.skip_ident_run();
                    Ok(true)
                }
            }
            d if d.is_ascii_digit() => {
                self.skip_number();
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn skip_block_comment(&mut self) -> Result<(), Diagnostic> {
        let start = self.pos;
        self.pos += 2;
        let mut depth = 1usize;
        loop {
            match self.peek_char() {
                None => return Err(self.unclosed(start, "块注释 `/*`")),
                Some('*') if self.byte_at(self.pos + 1) == Some(b'/') => {
                    self.pos += 2;
                    depth -= 1;
                    if depth == 0 {
                        return Ok(());
                    }
                }
                Some('/') if self.byte_at(self.pos + 1) == Some(b'*') => {
                    self.pos += 2;
                    depth += 1;
                }
                Some(_) => {
                    self.bump();
                }
            }
        }
    }

    fn skip_string(&mut self) -> Result<(), Diagnostic> {
        let start = self.pos;
        self.bump();
        loop {
            match self.peek_char() {
                None => return Err(self.unclosed(start, "字符串 `\"`")),
                Some('\\') => {
                    self.bump();
                    if self.peek_char().is_some() {
                        self.bump();
                    }
                }
                Some('"') => {
                    self.bump();
                    return Ok(());
                }
                // 普通字符串不能跨行：提前判错，避免把后续整段 DSL 当成字符串内容
                Some('\n') => return Err(self.unclosed(start, "字符串 `\"`")),
                Some(_) => {
                    self.bump();
                }
            }
        }
    }

    fn skip_raw_string(&mut self, hashes: usize) -> Result<(), Diagnostic> {
        let start = self.pos;
        self.bump(); // r
        for _ in 0..hashes {
            self.bump(); // #
        }
        if self.peek_char() != Some('"') {
            return Err(self.unclosed(start, "原始字符串 `r#\"`"));
        }
        self.bump();
        loop {
            match self.peek_char() {
                None => return Err(self.unclosed(start, "原始字符串 `r#\"`")),
                Some('"') => {
                    let mut closed = true;
                    for k in 1..=hashes {
                        if self.byte_at(self.pos + k) != Some(b'#') {
                            closed = false;
                            break;
                        }
                    }
                    self.bump();
                    if closed {
                        self.pos += hashes;
                        return Ok(());
                    }
                }
                Some(_) => {
                    self.bump();
                }
            }
        }
    }

    /// 区分字符字面量与生命周期（§3.5.3）。
    fn skip_char_or_lifetime(&mut self) -> Result<(), Diagnostic> {
        let start = self.pos;
        self.bump(); // '
        match self.peek_char() {
            None => Err(self.unclosed(start, "字符字面量 `'`")),
            Some('\\') => {
                self.bump(); // 反斜杠
                if matches!(self.peek_char(), Some('u') | Some('U')) {
                    self.bump();
                    if self.peek_char() == Some('{') {
                        self.pos += 1;
                        let mut depth = 1usize;
                        while depth > 0 {
                            match self.peek_char() {
                                None => return Err(self.unclosed(start, "字符字面量 `'`")),
                                Some('{') => {
                                    depth += 1;
                                    self.bump();
                                }
                                Some('}') => {
                                    depth -= 1;
                                    self.bump();
                                }
                                Some(_) => {
                                    self.bump();
                                }
                            }
                        }
                    }
                } else if self.peek_char().is_some() {
                    self.bump();
                }
                if self.peek_char() != Some('\'') {
                    return Err(self.unclosed(start, "字符字面量 `'`"));
                }
                self.bump();
                Ok(())
            }
            Some(c) if is_ident_start(c) => {
                self.skip_ident_run();
                if self.peek_char() == Some('\'') {
                    self.bump(); // char 字面量 'a'
                }
                // 否则是生命周期 'a：位置已停在标识符之后，无需再消费
                Ok(())
            }
            Some(_) => {
                // `'x'` 形式的单字符字面量：`'{'`、`')'`、`'"'`、多字节字符等
                self.bump();
                if self.peek_char() == Some('\'') {
                    self.bump();
                    Ok(())
                } else {
                    Err(self.unclosed(start, "字符字面量 `'`"))
                }
            }
        }
    }

    fn skip_number(&mut self) {
        loop {
            match self.peek_char() {
                Some(c) if c.is_ascii_alphanumeric() || c == '_' => {
                    self.bump();
                }
                // `1..10` 的 `..` 不是小数点：只有 `.` 后接数字才吞
                Some('.') if matches!(self.char_at(self.pos + 1), Some(d) if d.is_ascii_digit()) => {
                    self.bump();
                }
                _ => return,
            }
        }
    }

    fn skip_ident_run(&mut self) {
        while matches!(self.peek_char(), Some(c) if is_ident_continue(c)) {
            self.bump();
        }
    }

    // ------------------------------------------------------------- 关键字

    /// 词边界感知的单词匹配（**消费**）。
    pub fn eat_word(&mut self, w: &str) -> bool {
        self.skip_trivia_lossy();
        if !self.match_word(w) {
            return false;
        }
        true
    }

    /// 词边界感知的单词匹配（不消费）。
    pub fn peek_word(&mut self, w: &str) -> bool {
        let save = self.pos;
        let hit = self.eat_word(w);
        self.restore(save);
        hit
    }

    /// 多词关键字组块匹配（消费），如 `go with` / `exit go with` / `extern rust`。
    ///
    /// 失败时**回滚到调用前的位置**（只跳过前导空白），否则 `force go with` 试匹配失败
    /// 会吃掉 `force` 导致后续 `force read with` 永远匹配不上。
    pub fn eat_phrase(&mut self, phrase: &str) -> bool {
        self.skip_trivia_lossy();
        let save = self.pos;
        let words: Vec<&str> = phrase.split(' ').collect();
        if words.is_empty() {
            return false;
        }
        if !self.word_boundary_before() {
            return false;
        }
        for (i, w) in words.iter().enumerate() {
            if i > 0 {
                self.skip_trivia_lossy();
            }
            if !self.match_word(w) {
                self.restore(save);
                return false;
            }
        }
        true
    }

    /// 多词关键字组块匹配（不消费）。
    pub fn peek_phrase_str(&mut self, phrase: &str) -> bool {
        let save = self.pos;
        let hit = self.eat_phrase(phrase);
        self.restore(save);
        hit
    }

    fn match_word(&mut self, w: &str) -> bool {
        if !self.word_boundary_before() {
            return false;
        }
        if self.src.len() - self.pos < w.len() {
            return false;
        }
        // 结束偏移可能落在多字节字符中间（中文注释/标识符），必须先确认字符边界
        let after = self.pos + w.len();
        if !self.src.is_char_boundary(after) || &self.src[self.pos..after] != w {
            return false;
        }
        // 词尾边界：后一个字符不得是标识符续写字符
        if let Some(c) = self.char_at(after) {
            if is_ident_continue(c) {
                return false;
            }
        }
        self.pos = after;
        true
    }

    fn word_boundary_before(&self) -> bool {
        if self.pos == 0 {
            return true;
        }
        let prev = self.src[..self.pos].chars().next_back();
        match prev {
            None => true,
            Some(c) => !is_ident_continue(c),
        }
    }

    /// 从当前位置扫描一条"下一语句的起始"：命中 DSL 关键字返回关键字词，否则 `None`
    /// （调用方转 Rust 透传，§3.5.1）。
    ///
    /// 命中多词关键字时按**词序 + 词边界**整体匹配（§7.2），返回最长匹配，如
    /// `exit go with`、`go with`、`extern rust`。本方法会先跳过前导空白与注释。
    ///
    /// **只识别 DSL 关键字**；顶层 Rust item 关键字（`use` / `struct` / `pub` …）请用
    /// [`Lexer::peek_stmt_start`]。
    pub fn peek_stmt_keyword(&mut self) -> Result<Option<&'static str>, Diagnostic> {
        self.skip_trivia()?;
        let save = self.pos;
        let mut found = None;
        for (name, words) in KEYWORD_PHRASES {
            if self.eat_phrase_multi(words) {
                found = Some(*name);
                break;
            }
            self.restore(save);
        }
        self.restore(save);
        Ok(found)
    }

    /// 语句起始关键字识别 = DSL 关键字（§7.2）∪ 顶层 Rust item 关键字（§3.5.4），
    /// 取最长匹配。
    ///
    /// Parser 用它在顶层与 cell 体内做"这条语句是什么"的分派：命中 Rust item 关键字
    /// 时按 §3.5.4 整体透传；命中 `cell` / `entry` / `extern rust` 时为顶层 item 边界；
    /// 命中其它 DSL 关键字时按 DSL 规则解析；未命中则按 §3.5.1 走 Rust 透传。
    pub fn peek_stmt_start(&mut self) -> Result<Option<&'static str>, Diagnostic> {
        if let Some(kw) = self.peek_stmt_keyword()? {
            return Ok(Some(kw));
        }
        let save = self.pos;
        for kw in RUST_ITEM_ALL {
            if self.eat_word(kw) {
                self.restore(save);
                return Ok(Some(kw));
            }
            self.restore(save);
        }
        Ok(None)
    }

    fn eat_phrase_multi(&mut self, words: &[&str]) -> bool {
        if words.is_empty() {
            return false;
        }
        if !self.word_boundary_before() {
            return false;
        }
        for (i, w) in words.iter().enumerate() {
            if i > 0 {
                self.skip_trivia_lossy();
            }
            if !self.match_word(w) {
                return false;
            }
        }
        true
    }

    // ------------------------------------------------------------ 基础读取

    /// 读取一个标识符（词边界感知，跳过前导空白与注释）。
    ///
    /// DSL 保留字（§3.2）会被拒绝，防止 `jump when ...` 这类片段被误吞成目标名。
    pub fn read_ident(&mut self) -> Result<(String, Span), Diagnostic> {
        self.skip_trivia()?;
        let start = self.pos;
        match self.peek_char() {
            Some(c) if is_ident_start(c) => {}
            Some(c) => {
                return Err(Diagnostic::error(
                    code::E030,
                    format!("语法错误：期望标识符，遇到 `{c}`"),
                    self.span(start, start),
                ))
            }
            None => {
                return Err(Diagnostic::error(
                    code::E030,
                    "语法错误：文件结束，期望标识符",
                    self.span(start, start),
                ))
            }
        }
        self.skip_ident_run();
        let end = self.pos;
        let text = self.src[start..end].to_string();
        if is_reserved_word(&text) {
            return Err(Diagnostic::error(
                code::E030,
                format!("语法错误：`{text}` 是 DSL 保留字，不能用作标识符"),
                self.span(start, end),
            )
            .with_detail("保留字清单见规格 §3.2"));
        }
        Ok((text, self.span(start, end)))
    }

    /// 消费 `;`。
    pub fn expect_semi(&mut self) -> Result<(), Diagnostic> {
        self.skip_trivia()?;
        if self.peek_char() == Some(';') {
            self.bump();
            Ok(())
        } else {
            Err(Diagnostic::error(
                code::E030,
                "语法错误：期望 `;` 结束语句",
                self.span(self.pos, self.pos),
            ))
        }
    }

    /// 若当前是 `;` 则消费（用于块后多余分号，§3.1 规则 3）。
    pub fn eat_semi(&mut self) -> bool {
        self.skip_trivia_lossy();
        if self.peek_char() == Some(';') {
            self.bump();
            true
        } else {
            false
        }
    }

    /// 消费指定字符。
    pub fn expect_char(&mut self, c: char) -> Result<(), Diagnostic> {
        self.skip_trivia()?;
        if self.peek_char() == Some(c) {
            self.bump();
            Ok(())
        } else {
            let found = self.peek_char().map(|x| x.to_string()).unwrap_or_else(|| "文件结束".to_string());
            Err(Diagnostic::error(
                code::E030,
                format!("语法错误：期望 `{c}`，实际遇到 {found}"),
                self.span(self.pos, self.pos),
            ))
        }
    }

    /// 若当前是指定字符则消费。
    pub fn eat_char(&mut self, c: char) -> bool {
        self.skip_trivia_lossy();
        if self.peek_char() == Some(c) {
            self.bump();
            true
        } else {
            false
        }
    }

    /// 当前是否为字面量文本 `s`（不消费）。
    pub fn peek_str(&mut self, s: &str) -> bool {
        self.skip_trivia_lossy();
        if self.src.len() - self.pos < s.len() {
            return false;
        }
        let end = self.pos + s.len();
        self.src.is_char_boundary(end) && &self.src[self.pos..end] == s
    }

    /// 错误恢复：跳到下一个 depth 0 的 `;` 并消费；遇到 `}`、DSL 语句起始或 EOF 停止。
    pub fn skip_to_semi(&mut self) {
        /// DSL 语句起始词（恢复时在此停下，避免把后续语句一起吞掉）。
        const STOP_WORDS: &[&str] = &["jump", "die", "out", "cell", "entry", "sync"];
        loop {
            if self.at_eof() {
                return;
            }
            match self.peek_char() {
                Some(';') => {
                    self.bump();
                    return;
                }
                Some('}') => return,
                Some(c) if c.is_alphabetic() || c == '_' => {
                    let at_stmt_start = STOP_WORDS.iter().any(|w| self.peek_word(w))
                        || self.peek_phrase_str("exit go with");
                    if at_stmt_start {
                        return;
                    }
                }
                _ => {}
            }
            if self.bump().is_none() {
                return;
            }
        }
    }

    // ---------------------------------------------------------------- 扫描

    /// 按 §3.5.3 规则扫描一段 Rust 透传文本，直到命中 `stops` 中的任一停止条件。
    ///
    /// 扫描过程中跳过字符串/字符/原始串/行注释/块注释/数字/生命周期，并按 `()[]{}`
    /// 配对（**泛型 `<` `>` 不配对**：§3.5.3 首版保守策略）。停止符本身不消费。
    pub fn scan_raw(&mut self, stops: Stops) -> Result<RawScan, Diagnostic> {
        let start = self.pos;
        let mut depth: u32 = 0;
        let stop = 'outer: loop {
            if self.pos >= self.src.len() {
                if depth > 0 {
                    return Err(self.unclosed_group(start));
                }
                break StopKind::Eof;
            }
            if self.skip_noise()? {
                continue;
            }
            let Some(c) = self.peek_char() else { continue };
            if depth == 0 {
                for w in stops.words {
                    if self.peek_phrase_str(w) {
                        break 'outer StopKind::Keyword;
                    }
                }
                match c {
                    ';' if stops.semi => break StopKind::Semi,
                    ',' if stops.comma => break StopKind::Comma,
                    ')' => break StopKind::CloseParen,
                    ']' => break StopKind::CloseBracket,
                    '}' => break StopKind::CloseBrace,
                    '{' if stops.open_brace => break StopKind::OpenBrace,
                    '=' if stops.arrow && self.char_at(self.pos + 1) == Some('>') => {
                        break StopKind::Arrow
                    }
                    '-' if stops.dash_gt && self.char_at(self.pos + 1) == Some('>') => {
                        break StopKind::DashGt
                    }
                    '|' if stops.pipe_gt && self.char_at(self.pos + 1) == Some('>') => {
                        break StopKind::PipeGt
                    }
                    _ => {}
                }
            }
            match c {
                '(' | '[' | '{' => {
                    depth += 1;
                    self.bump();
                }
                ')' | ']' | '}' => {
                    depth = depth.saturating_sub(1);
                    self.bump();
                }
                _ => {
                    self.bump();
                }
            }
        };
        Ok(RawScan { start, end: self.pos, stop })
    }

    /// 扫描一条 Rust 透传语句（**不透明**，块内不做 DSL 分派）：到 `;` 或到配平的
    /// `{...}` 块为止（§3.5.1 规则 2）。`;` 会包含在返回文本内；块后多余 `;` 被忽略
    /// （§3.1 规则 3，**裁定依据 A1**：块语句的 text 不含分号）。
    pub fn scan_raw_stmt(&mut self) -> Result<RawRust, Diagnostic> {
        let start = self.pos;
        let r = self.scan_raw(Stops::STMT_HEAD)?;
        match r.stop {
            StopKind::Semi => {
                self.bump();
                Ok(self.raw(start, self.pos))
            }
            StopKind::Eof => Ok(self.raw(start, r.end)),
            StopKind::OpenBrace => {
                let block = self.scan_braced_block()?;
                // 头文本与块文本在源文本中相邻，拼接后仍是整条语句的逐字切片
                Ok(self.raw(start, r.end + block.text.len()))
            }
            other => Err(Diagnostic::error(
                code::E030,
                format!("语法错误：Rust 透传语句意外终止（{other:?}）"),
                self.span(r.end, r.end),
            )),
        }
    }

    /// 扫描到配平的 `{ ... }`（**含**两侧花括号），块内不透明：跳过
    /// 字符串/字符/原始串/行注释/块注释/数字/生命周期。EOF 未配平报 RSC-E030。
    pub fn scan_braced_block(&mut self) -> Result<RawRust, Diagnostic> {
        self.skip_trivia()?;
        let open = self.pos;
        if self.peek_char() != Some('{') {
            return Err(Diagnostic::error(
                code::E030,
                "语法错误：期望 `{` 开始 Rust 块",
                self.span(open, open),
            ));
        }
        self.bump();
        let mut depth = 1u32;
        loop {
            if self.pos >= self.src.len() {
                return Err(self.unclosed(open, "Rust 块 `{`"));
            }
            if self.skip_noise()? {
                continue;
            }
            let Some(c) = self.peek_char() else { continue };
            match c {
                '{' | '(' | '[' => {
                    depth += 1;
                    self.bump();
                }
                '}' | ')' | ']' => {
                    depth -= 1;
                    self.bump();
                    if depth == 0 {
                        return Ok(self.raw(open, self.pos));
                    }
                }
                _ => {
                    self.bump();
                }
            }
        }
    }

    /// 跳过配平的 `( )` / `[ ]`（调用前当前字符须为开括号）。
    pub fn skip_balanced(&mut self) -> Result<(), Diagnostic> {
        let start = self.pos;
        self.skip_trivia()?;
        let Some(open) = self.peek_char() else {
            return Err(self.unclosed_group(start));
        };
        if open != '(' && open != '[' {
            return Ok(());
        }
        self.bump();
        let mut depth = 1u32;
        loop {
            if self.pos >= self.src.len() {
                return Err(self.unclosed_group(start));
            }
            if self.skip_noise()? {
                continue;
            }
            let Some(c) = self.peek_char() else { continue };
            match c {
                '(' | '[' | '{' => {
                    depth += 1;
                    self.bump();
                }
                ')' | ']' | '}' => {
                    depth -= 1;
                    self.bump();
                    if depth == 0 {
                        return Ok(());
                    }
                }
                _ => {
                    self.bump();
                }
            }
        }
    }

    // ---------------------------------------------------------------- 诊断

    fn unclosed(&self, at: usize, what: &str) -> Diagnostic {
        Diagnostic::error(
            code::E030,
            format!("词法错误：未闭合的{what}"),
            self.span(at, at),
        )
        .with_detail("§7.4：未闭合的 `\"` / `r\"` / `/*` / 括号均报 RSC-E030")
    }

    fn unclosed_group(&self, at: usize) -> Diagnostic {
        Diagnostic::error(
            code::E030,
            "词法错误：未闭合的括号 `(` / `[` / `{`",
            self.span(at, at),
        )
        .with_detail("§7.4：未配平的括号报 RSC-E030")
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_ident_continue(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}
