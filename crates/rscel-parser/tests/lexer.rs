//! 词法分析器（§7）与 Span/诊断的单元测试。
//!
//! 覆盖：trivia 跳过、多词关键字的词边界匹配、标识符读取、透传扫描、块扫描、
//! 词法错误（E030）与诊断渲染（§15.1）。

use rscel_parser::ast::RawRust;
use rscel_parser::diag::{code, Diagnostic, Severity};
use rscel_parser::lexer::{Lexer, StopKind, Stops};
use rscel_parser::span::{locate, span_between, Span};

fn lex(src: &str) -> Lexer<'_> {
    Lexer::new(src, 3)
}

fn text(l: &Lexer<'_>, start: usize, end: usize) -> String {
    l.raw(start, end).text
}

// ------------------------------------------------------------------ trivia

#[test]
fn skip_trivia_consumes_ws_and_comments() {
    let src = "  \n\t // 行注释\n /* 块\n注释 */ x";
    let mut l = lex(src);
    l.skip_trivia().expect("trivia 无误");
    assert_eq!(l.pos(), src.find('x').unwrap());
    assert_eq!(l.peek_char(), Some('x'));
}

#[test]
fn nested_block_comment_is_one_trivia_unit() {
    let src = "/* a /* b */ c */ x";
    let mut l = lex(src);
    l.skip_trivia().expect("嵌套块注释合法");
    assert_eq!(l.peek_char(), Some('x'));
}

#[test]
fn unclosed_block_comment_is_e030() {
    let mut l = lex("/* 没有闭合");
    let d = l.skip_trivia().expect_err("应报未闭合");
    assert_eq!(d.code, code::E030);
    assert!(d.message.contains("未闭合"), "{}", d.message);
    assert_eq!(d.span.line, 1);
}

#[test]
fn skip_trivia_does_not_swallow_numbers_or_strings() {
    // 回归用例：trivia 只跳空白与注释，数字/字符串必须留给结构分析
    let mut l = lex("123 \"abc\" x");
    l.skip_trivia().expect("无错");
    assert_eq!(l.peek_char(), Some('1'));
}

// --------------------------------------------------------------- 关键字

#[test]
fn stmt_keyword_single_words() {
    for (src, want) in [
        ("cell a;", Some("cell")),
        ("receive(x);", Some("receive")),
        ("jump b;", Some("jump")),
        ("die;", Some("die")),
        ("out(a);", Some("out")),
        ("entry a;", Some("entry")),
        ("let x = 1;", None),
        ("123", None),
    ] {
        let mut l = lex(src);
        assert_eq!(l.peek_stmt_keyword().unwrap(), want, "源: {src:?}");
    }
}

#[test]
fn stmt_keyword_multi_word_phrases() {
    // §7.2：按词序 + 词边界整体匹配
    for (src, want) in [
        ("exit go with(x);", Some("exit go with")),
        ("go with(x);", Some("go with")),
        ("read with(x);", Some("read with")),
        ("extern rust {", Some("extern rust")),
        ("extern rust read {", Some("extern rust")),
        ("concurrent_limit=4;", Some("concurrent_limit")),
        // 词边界：标识符里含关键字不得命中
        ("logo with(x);", None),
        ("exit_go with(x);", None),
        // 孤立形式
        ("go;", Some("go")),
        ("with;", Some("with")),
        ("exit;", Some("exit")),
    ] {
        let mut l = lex(src);
        assert_eq!(l.peek_stmt_keyword().unwrap(), want, "源: {src:?}");
    }
}

#[test]
fn eat_word_respects_word_boundary() {
    let mut l = lex("go_with go_with_");
    assert!(!l.eat_word("go"), "go_with 不是 go");
    assert!(!l.eat_word("with"));
    assert!(l.eat_word("go_with"), "完整标识符可整体匹配");
    assert!(!l.eat_word("go"), "后面是下划线续写");
}

#[test]
fn eat_phrase_rolls_back_on_failure() {
    // 回归用例：force go with 试匹配失败后必须回滚，否则 force read with 永远匹配不上
    let mut l = lex("force read with(x);");
    assert!(!l.eat_phrase("force go with"));
    assert!(l.eat_phrase("force read with"));
    assert_eq!(l.peek_char(), Some('('));
}

#[test]
fn stmt_start_includes_rust_item_keywords() {
    // §3.5.4 + 文档扩展
    for (src, want) in [
        ("use std::io;", Some("use")),
        ("struct S;", Some("struct")),
        ("impl X {}", Some("impl")),
        ("pub fn f() {}", Some("pub")),
        ("macro_rules! m {}", Some("macro_rules")),
        ("extern \"C\" fn f() {}", Some("extern")),
        // DSL 多词优先于 Rust item 关键字
        ("extern rust {", Some("extern rust")),
    ] {
        let mut l = lex(src);
        assert_eq!(l.peek_stmt_start().unwrap(), want, "源: {src:?}");
    }
}

// --------------------------------------------------------------- 标识符

#[test]
fn read_ident_basics() {
    let mut l = lex("  calc_cell ");
    let (name, span) = l.read_ident().expect("标识符");
    assert_eq!(name, "calc_cell");
    assert_eq!((span.line, span.col, span.len), (1, 3, 9));
    assert_eq!(l.peek_char(), Some(' '));
}

#[test]
fn read_ident_accepts_unicode_and_rejects_reserved() {
    let mut l = lex("计算");
    let (name, span) = l.read_ident().expect("中文标识符");
    assert_eq!(name, "计算");
    assert_eq!(span.len, 6);

    let mut l2 = lex("cell");
    let d = l2.read_ident().expect_err("保留字不可作标识符");
    assert_eq!(d.code, code::E030);
    assert!(d.message.contains("保留字"), "{}", d.message);

    let mut l3 = lex("123");
    assert!(l3.read_ident().is_err());
}

#[test]
fn expect_semi_and_chars() {
    let mut l = lex(" ; ");
    l.expect_semi().expect("分号");
    let mut l2 = lex("x");
    assert!(l2.expect_semi().is_err());
    let mut l3 = lex("中文");
    assert_eq!(l3.peek_char(), Some('中'));
    assert_eq!(l3.bump(), Some('中'));
    assert_eq!(l3.peek_char(), Some('文'));
    assert_eq!(l3.bump(), Some('文'));
    assert_eq!(l3.bump(), None);
}

// --------------------------------------------------------------- 扫描

#[test]
fn scan_raw_stmt_keeps_semicolon_and_inner_text() {
    let mut l = lex("let mut s: String = String::from(\"a;b\"); rest");
    let raw = l.scan_raw_stmt().expect("透传扫描");
    assert_eq!(raw.text, "let mut s: String = String::from(\"a;b\");");
    assert_eq!(l.pos(), raw.text.len());
    assert_eq!(l.peek_char(), Some(' '));
}

#[test]
fn scan_raw_stmt_stops_at_balanced_block() {
    let mut l = lex("fn f() -> u64 { 1 } tail");
    let raw = l.scan_raw_stmt().expect("item 扫描");
    assert_eq!(raw.text, "fn f() -> u64 { 1 }");
    assert!(raw.text.contains('{'));
}

#[test]
fn scan_raw_stops_config() {
    let mut l = lex("a, b) c");
    l.skip_trivia().unwrap();
    let r = l.scan_raw(Stops::ARG).expect("实参扫描");
    assert_eq!(r.stop, StopKind::Comma);
    assert_eq!((r.start, r.end), (0, 1));
    l.bump();
    l.skip_trivia().unwrap();
    let r2 = l.scan_raw(Stops::ARG).expect("实参扫描");
    assert_eq!(r2.stop, StopKind::CloseParen);
    assert_eq!(text(&l, r2.start, r2.end), "b");
}

#[test]
fn scan_raw_unbalanced_paren_is_e030() {
    let mut l = lex("f(;");
    l.skip_trivia().unwrap();
    let d = l.scan_raw(Stops::STMT_HEAD).expect_err("未配平括号");
    assert_eq!(d.code, code::E030);
    assert!(d.message.contains("括号"), "{}", d.message);
}

#[test]
fn scan_braced_block_skips_literals_and_comments() {
    let src = "{ let s = \"}\"; let c = '{'; /* } */ // }\n let r = r#\"}\"#; } tail";
    let mut l = lex(src);
    let raw: RawRust = l.scan_braced_block().expect("配平块");
    assert_eq!(raw.text, "{ let s = \"}\"; let c = '{'; /* } */ // }\n let r = r#\"}\"#; }");
    assert_eq!(l.pos(), raw.text.len());
    assert!(l.src()[l.pos()..].starts_with(" tail"));
}

#[test]
fn scan_braced_block_unclosed_is_e030() {
    let mut l = lex("{ let a = 1;");
    l.skip_trivia().unwrap();
    let d = l.scan_braced_block().expect_err("未闭合块");
    assert_eq!(d.code, code::E030);
    assert!(d.message.contains("未闭合"), "{}", d.message);
}

#[test]
fn scan_braced_block_requires_open_brace() {
    let mut l = lex("fn f()");
    l.skip_trivia().unwrap();
    let d = l.scan_braced_block().expect_err("缺少 `{`");
    assert_eq!(d.code, code::E030);
}

#[test]
fn lexical_errors_are_e030() {
    for (src, needle) in [
        ("let s = \"未闭合", "字符串"),
        ("let s = r\"未闭合", "原始字符串"),
        ("let s = r#\"未闭合", "原始字符串"),
        ("let s = \"跨行\n字符串\";", "字符串"),
    ] {
        let mut l = lex(src);
        l.skip_trivia().unwrap();
        let d = l.scan_raw(Stops::STMT_HEAD).expect_err(src);
        assert_eq!(d.code, code::E030, "源: {src:?}");
        assert!(d.message.contains(needle), "源 {src:?} 消息 {}", d.message);
    }
}

#[test]
fn unterminated_char_literal_is_treated_as_lifetime() {
    // 有意的宽松处理：`'a` 形式无法与"EOF 处的生命周期"区分，按生命周期放过，
    // 交给 rustc 报未闭合字符字面量（§11.8：块内/透传区合法性由 rustc 兜底）
    let mut l = lex("let c = 'a;");
    l.skip_trivia().unwrap();
    let r = l.scan_raw(Stops::STMT_HEAD).expect("按生命周期放过");
    assert_eq!(r.stop, StopKind::Semi);
}

#[test]
fn lifetime_is_not_char_literal() {
    // 生命周期不得被当成未闭合字符字面量
    let src = "let r: &'a T = v; let s = &'static str = v;";
    let mut l = lex(src);
    l.skip_trivia().unwrap();
    let r = l.scan_raw(Stops::STMT_HEAD).expect("生命周期扫描");
    assert_eq!(text(&l, r.start, r.end), "let r: &'a T = v", "扫描在 depth0 的 `;` 之前停止（停止符不消费）");
}

#[test]
fn multibyte_source_never_panics() {
    // 所有扫描路径遇到多字节字符都必须安全切片
    let src = "cell 计算; let s = \"中文; { }\"; 变量;";
    let mut l = lex(src);
    l.skip_trivia().unwrap();
    l.eat_word("cell");
    l.skip_trivia().unwrap();
    l.read_ident().expect("中文 cell 名");
    l.expect_semi().expect("分号");
    l.skip_trivia().unwrap();
    let r = l.scan_raw(Stops::STMT_HEAD).expect("扫描");
    let raw = l.raw(r.start, r.end);
    assert_eq!(raw.text, "let s = \"中文; { }\"", "扫描在 depth0 的 `;` 之前停止（停止符不消费）");
    assert!(l.src().is_char_boundary(r.end));

    // 非字符边界偏移被向内对齐，绝不 panic
    let aligned = l.raw(1, 4);
    assert!(l.src().is_char_boundary(1) || aligned.text.is_char_boundary(0));
    assert!(!aligned.text.is_empty());
}

// --------------------------------------------------------------- span/diag

#[test]
fn locate_and_span_between() {
    let src = "cell a;\nreceive(x);\nlet y = 1;\n";
    assert_eq!(locate(src, 0), (1, 1));
    assert_eq!(locate(src, src.find("receive").unwrap()), (2, 1));
    assert_eq!(locate(src, src.find("let").unwrap()), (3, 1));
    let s = span_between(src, 5, src.find("let").unwrap(), src.find("let").unwrap() + 10);
    assert_eq!((s.file, s.line, s.col, s.len), (5, 3, 1, 10));
}

#[test]
fn span_join_and_helpers() {
    let a = Span::new(1, 2, 3, 4);
    let b = Span::new(1, 2, 8, 2);
    assert_eq!(a.join(b), Span::new(1, 2, 3, 7));
    assert_eq!(a.end_col(), 7);
    assert!(!a.is_empty());
    assert!(Span::default().is_empty());
    assert_eq!(Span::at(9, 1, 1), Span::new(9, 1, 1, 0));
}

#[test]
fn diagnostic_construction_and_render() {
    let d = Diagnostic::error(code::E030, "词法错误：未闭合的字符串", Span::new(0, 4, 9, 5))
        .with_detail("遇到 EOF")
        .with_cell("fact");
    assert_eq!(d.severity, Severity::Error);
    assert_eq!(d.render("a.rscel"), "RSC-E030: 词法错误：未闭合的字符串；遇到 EOF；位置: cell fact @ 4:9");

    let w = Diagnostic::warning(code::W001, "cell foo 不可达", Span::new(0, 12, 1, 0));
    assert_eq!(w.severity, Severity::Warning);
    assert_eq!(w.render("x.rscel"), "RSC-W001: cell foo 不可达；位置: x.rscel @ 12:1");
}
