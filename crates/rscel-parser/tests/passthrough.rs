//! 透传区矩阵（§3.5 全部条目 + 任务书第 5.8 条逐条覆盖）。
//!
//! 每条用例都断言 `RawRust.text` **逐字相等**（A1：零拷贝原文透传）或 AST 字段，
//! 而不是"能解析不报错"。

use rscel_parser::ast::*;
use rscel_parser::parse_program;

fn ok(src: &str) -> Program {
    let out = parse_program(src, 0);
    assert!(
        !out.has_errors(),
        "期望无错误，实际: {:?}",
        out.diagnostics.iter().map(|d| d.render("t.rscel")).collect::<Vec<_>>()
    );
    out.program
}

fn body(src: &str) -> Vec<Stmt> {
    ok(src).cells().next().expect("至少一个 cell").body.clone()
}

/// 递归收集 Rust 块内重新分派得到的所有 DSL 语句（§3.5.2）。
fn dsl_stmts(block: &RawBlock) -> Vec<String> {
    let mut out = Vec::new();
    collect(block, &mut out);
    out
}

fn collect(block: &RawBlock, out: &mut Vec<String>) {
    for p in &block.parts {
        match p {
            RawBlockPart::Text(_) => {}
            RawBlockPart::Stmt(Stmt::RustBlock { block }) => collect(block, out),
            RawBlockPart::Stmt(Stmt::Jump(j)) => {
                out.push(format!("jump {:?} -> {:?}", j.kind, j.targets));
            }
            RawBlockPart::Stmt(Stmt::Die { .. }) => out.push("die".to_string()),
            RawBlockPart::Stmt(Stmt::Exit { args, .. }) => {
                let keys: Vec<String> = args.iter().map(|a| a.effective_key()).collect();
                out.push(format!("exit go with({})", keys.join(",")));
            }
            RawBlockPart::Stmt(Stmt::Sync { group, .. }) => out.push(format!("sync {group}")),
            RawBlockPart::Stmt(Stmt::Out { .. }) => out.push("out".to_string()),
            RawBlockPart::Stmt(Stmt::ExternRustBlock { .. }) => out.push("extern rust".to_string()),
            RawBlockPart::Stmt(Stmt::RawRust(r)) => out.push(format!("raw {}", r.text)),
        }
    }
}

/// 递归收集块内所有透传文本片段（按顺序）。
fn texts(block: &RawBlock) -> Vec<String> {
    let mut out = Vec::new();
    collect_texts(block, &mut out);
    out
}

fn collect_texts(block: &RawBlock, out: &mut Vec<String>) {
    for p in &block.parts {
        match p {
            RawBlockPart::Text(t) => out.push(t.text.clone()),
            RawBlockPart::Stmt(Stmt::RustBlock { block }) => collect_texts(block, out),
            _ => {}
        }
    }
}

fn expect_block(s: &Stmt) -> &RawBlock {
    match s {
        Stmt::RustBlock { block } => block,
        other => panic!("期望 RustBlock，实际 {other:?}"),
    }
}

/// 整个 cell body 逐字还原（仅当全部语句都是透传语句时可用）。
fn body_source(src: &str) -> String {
    let body = body(src);
    body.iter()
        .map(|s| match s {
            Stmt::RawRust(r) => r.text.clone(),
            other => panic!("期望 RawRust，实际 {other:?}"),
        })
        .collect()
}

// ------------------------------------------------- 8.1 let 与类型注解

#[test]
fn let_statement_is_verbatim() {
    let b = body("cell a;\nreceive(x);\nlet x = 5;\ndie;\n");
    match &b[0] {
        Stmt::RawRust(r) => {
            assert_eq!(r.text, "let x = 5;", "语句型透传含结尾 `;`（A1）");
            assert_eq!(r.span.col, 1);
            assert_eq!(r.span.len as usize, 10);
        }
        other => panic!("期望 RawRust，实际 {other:?}"),
    }
}

#[test]
fn let_mut_with_type_and_string_containing_semicolon() {
    // 字符串里的 `;` 不能被当成语句结束
    let b = body("cell a;\nreceive(x);\nlet mut s: String = String::from(\"a;b\");\ndie;\n");
    match &b[0] {
        Stmt::RawRust(r) => assert_eq!(r.text, "let mut s: String = String::from(\"a;b\");"),
        other => panic!("期望 RawRust，实际 {other:?}"),
    }
}

// ------------------------------------------- 8.2 字符串里含 DSL 关键字

#[test]
fn dsl_keywords_inside_string_literal() {
    // 字符串里的 `jump die exit go with` 既不构成 DSL 语句，也不影响语句边界
    assert_eq!(
        body_source("cell a;\nreceive(x);\nlet s = \"jump die exit go with\";\n"),
        "let s = \"jump die exit go with\";"
    );
    let b = body("cell a;\nreceive(x);\nlet s = \"jump die exit go with\";\ndie;\n");
    assert!(matches!(b[0], Stmt::RawRust(_)));
    assert!(matches!(b[1], Stmt::Die { .. }));
}

#[test]
fn raw_string_forms() {
    // r"..." 与 r#"..."# 内部的花括号/分号都不参与结构分析
    assert_eq!(body_source("cell a;\nreceive(x);\nlet s = r\"raw { } ; text\";\n"), "let s = r\"raw { } ; text\";");
    assert_eq!(body_source("cell a;\nreceive(x);\nlet s = r#\"has \" quote and ;\"#;\n"), "let s = r#\"has \" quote and ;\"#;");
    assert_eq!(body_source("cell a;\nreceive(x);\nlet s = r##\"a \"# b ; { }\"##;\n"), "let s = r##\"a \"# b ; { }\"##;");
}

// ----------------------------------------------------- 8.3 注释里的关键字

#[test]
fn dsl_keywords_inside_line_comment() {
    // 注释里的 `jump die;` 不得被识别
    let b = body("cell a;\nreceive(x);\n// jump die;\nlet x = 1;\n");
    assert_eq!(b.len(), 1, "注释不产生语句");
    assert_eq!(body_source("cell a;\nreceive(x);\n// jump die;\nlet x = 1;\n"), "let x = 1;");
}

#[test]
fn dsl_keywords_inside_block_comment_with_semicolons() {
    // 跨行块注释里的 `;` 不得终止语句
    let b = body("cell a;\nreceive(x);\n/* jump A go with(x);\n   die; block comment ; */\nlet y = 2;\n");
    assert_eq!(b.len(), 1);
    assert_eq!(body_source("cell a;\nreceive(x);\n/* jump A go with(x);\n   die; block comment ; */\nlet y = 2;\n"), "let y = 2;");
}

#[test]
fn nested_block_comment() {
    let b = body("cell a;\nreceive(x);\n/* outer /* inner ; */ still outer ; */\nlet y = 2;\n");
    assert_eq!(b.len(), 1);
    assert_eq!(body_source("cell a;\nreceive(x);\n/* outer /* inner ; */ still outer ; */\nlet y = 2;\n"), "let y = 2;");
}

// --------------------------------------- 8.4 块内语句重新做 DSL 分派

#[test]
fn if_else_block_redispatches_dsl() {
    // §3.5.2 核心用例：if 整体是透传，块内 jump / die 是 DSL 语句
    let b = body("cell a;\nreceive(x);\nif cond { jump A go with(x); } else { die; }\n");
    assert_eq!(b.len(), 1, "if 整体是一条 Rust 块语句");
    let block = expect_block(&b[0]);
    assert!(block.has_dsl());
    assert_eq!(dsl_stmts(block), vec!["jump Plain -> [\"A\"]", "die"]);
    assert_eq!(texts(block), vec!["if cond ", " else "], "块头与 else 连接文本逐字保留（右花括号由嵌套块渲染）");
    // jump 的实参与模式也要落到块内语句上
    match &block.parts[1] {
        RawBlockPart::Stmt(Stmt::RustBlock { block: arm }) => match &arm.parts[0] {
            RawBlockPart::Stmt(Stmt::Jump(j)) => {
                assert_eq!(j.mode, Some(TransMode::GoWith));
                assert_eq!(j.args[0].expr.text, "x");
            }
            other => panic!("期望 Jump，实际 {other:?}"),
        },
        other => panic!("期望嵌套块，实际 {other:?}"),
    }
}

#[test]
fn for_loop_block_redispatches_dsl() {
    let b = body("cell a;\nreceive(x);\nfor i in 0..10 { jump worker go with(i); }\n");
    let block = expect_block(&b[0]);
    assert_eq!(dsl_stmts(block), vec!["jump Plain -> [\"worker\"]"]);
    assert_eq!(texts(block), vec!["for i in 0..10 "]);
    match &block.parts[1] {
        RawBlockPart::Stmt(Stmt::RustBlock { block: inner }) => match &inner.parts[0] {
            RawBlockPart::Stmt(Stmt::Jump(j)) => assert_eq!(j.args[0].expr.text, "i"),
            other => panic!("期望 Jump，实际 {other:?}"),
        },
        other => panic!("期望嵌套块，实际 {other:?}"),
    }
}

#[test]
fn match_arms_with_blocks_redispatch_dsl() {
    let b = body("cell a;\nreceive(v);\nmatch v { 1 => { jump A; } _ => { die; } }\n");
    let block = expect_block(&b[0]);
    assert_eq!(dsl_stmts(block), vec!["jump Plain -> [\"A\"]", "die"], "两个分支各自重新分派");
    assert_eq!(
        texts(block),
        vec!["match v ", "1 =>", " _ => "],
        "match 头与分支头（`pattern =>`）逐字保留"
    );
}

#[test]
fn match_arms_without_blocks() {
    // 分支体直接是 DSL 语句：Rust 语法允许分支体不带分号，块内据此放宽结束符
    let b = body("cell a;\nreceive(v);\nmatch v { 1 => jump A go with(v), _ => die }\n");
    let block = expect_block(&b[0]);
    assert_eq!(dsl_stmts(block), vec!["jump Plain -> [\"A\"]", "die"]);
    assert_eq!(texts(block), vec!["match v ", "1 =>", ",", "_ =>"], "分支头与分支分隔逗号逐字保留");
}

#[test]
fn match_arms_with_plain_rust_bodies() {
    // 分支体是普通 Rust 表达式：整条仍是纯透传
    assert_eq!(
        body_source("cell a;\nreceive(v);\nmatch v { 1 => 2, _ => 3 }\n"),
        "match v { 1 => 2, _ => 3 }"
    );
    assert_eq!(
        body_source("cell a;\nreceive(v);\nmatch v { Some(x) if x > 1 => x, None => 0 }\n"),
        "match v { Some(x) if x > 1 => x, None => 0 }"
    );
}
#[test]
fn pure_rust_block_is_collapsed_to_verbatim_raw() {
    // 块内没有 DSL 语句时整条合并为一个逐字切片（§3.5.1 透传）
    assert_eq!(
        body_source("cell a;\nreceive(x);\nif cond { println!(\"{}\", x); }\n"),
        "if cond { println!(\"{}\", x); }"
    );
    assert_eq!(
        body_source("cell a;\nreceive(x);\nfor i in 0..3 { let y = i + 1; }\n"),
        "for i in 0..3 { let y = i + 1; }"
    );
    // 块后的尾随分号被忽略且不计入 text（§3.1 规则 3）
    assert_eq!(
        body_source("cell a;\nreceive(x);\nif cond { let y = 1; };\n"),
        "if cond { let y = 1; }"
    );
    // 匿名块
    assert_eq!(body_source("cell a;\nreceive(x);\n{ let y = 1; }\n"), "{ let y = 1; }");
}

#[test]
fn while_loop_and_nested_if() {
    let b = body("cell a;\nreceive(x);\nwhile i < n { if done { die; } else { jump A go with(x); } }\n");
    let block = expect_block(&b[0]);
    assert_eq!(dsl_stmts(block), vec!["die", "jump Plain -> [\"A\"]"]);
    assert_eq!(texts(block), vec!["while i < n ", "if done ", " else "]);
}

// --------------------------------- 8.5 闭包 / 生命周期 / 字符字面量

#[test]
fn closures_with_pipes_and_arrow() {
    // 闭包 `|f| f.path` 里的 `|` 与 `->` 不参与 DSL 分派
    assert_eq!(body_source("cell a;\nreceive(x);\nlet p = |f| f.path;\n"), "let p = |f| f.path;");
    assert_eq!(body_source("cell a;\nreceive(x);\nlet g = |x| { x + 1 };\n"), "let g = |x| { x + 1 };");
    assert_eq!(body_source("cell a;\nreceive(x);\nlet h = |x: u32| -> u32 { x * 2 };\n"), "let h = |x: u32| -> u32 { x * 2 };");
    // 闭包块整体不透明：块内即使有 DSL 关键字也不分派
    let b = body("cell a;\nreceive(x);\nlet f = |y| { let s = \"die\"; s };\n");
    assert_eq!(b.len(), 1);
    assert_eq!(body_source("cell a;\nreceive(x);\nlet f = |y| { let s = \"die\"; s };\n"), "let f = |y| { let s = \"die\"; s };");
}

#[test]
fn lifetimes_and_char_literals() {
    assert_eq!(body_source("cell a;\nreceive(s);\nlet r: &'a str = s;\n"), "let r: &'a str = s;");
    assert_eq!(body_source("cell a;\nreceive(x);\nlet c = '{';\n"), "let c = '{';");
    assert_eq!(body_source("cell a;\nreceive(x);\nlet c = '\\'';\n"), "let c = '\\'';");
    assert_eq!(body_source("cell a;\nreceive(x);\nlet c = '\\\\';\n"), "let c = '\\\\';");
    assert_eq!(body_source("cell a;\nreceive(x);\nlet c = 'a';\n"), "let c = 'a';");
    // 静态生命周期
    assert_eq!(body_source("cell a;\nreceive(x);\nlet n: &'static str = \"a\";\n"), "let n: &'static str = \"a\";");
    // unicode 转义
    assert_eq!(body_source("cell a;\nreceive(x);\nlet e = '\\u{1F600}';\n"), "let e = '\\u{1F600}';");
}

#[test]
fn lifetime_does_not_break_struct_lit() {
    // `Foo<'a> { .. }`：尖括号不配对（§3.5.3 保守策略），但整条仍是一个 RawRust
    assert_eq!(
        body_source("cell a;\nreceive(x);\nlet s = Foo<'a> { a: 1, b: 2 };\n"),
        "let s = Foo<'a> { a: 1, b: 2 };"
    );
}

// --------------------------------------------------- 8.6 宏与泛型

#[test]
fn macro_invocations() {
    assert_eq!(body_source("cell a;\nreceive(v);\nprintln!(\"{}\", v);\n"), "println!(\"{}\", v);");
    assert_eq!(body_source("cell a;\nreceive(v);\nvec![1,2,3];\n"), "vec![1,2,3];");
    assert_eq!(body_source("cell a;\nreceive(v);\nlet r = matches!(v, Some(_));\n"), "let r = matches!(v, Some(_));");
    // 大括号宏体：不透明块
    assert_eq!(
        body_source("cell a;\nreceive(v);\nlet q = quote! { let a = 1; };\n"),
        "let q = quote! { let a = 1; };"
    );
    // 宏体里含 DSL 关键字也不分派
    assert_eq!(
        body_source("cell a;\nreceive(v);\nlet q = quote! { jump A; die; };\n"),
        "let q = quote! { jump A; die; };"
    );
}

#[test]
fn generics_with_unbalanced_angle_brackets() {
    // §3.5.3：泛型 `<` `>` 不配对，泛型内的 DSL 关键字不识别
    assert_eq!(body_source("cell a;\nreceive(x);\nlet v: Vec<String> = Vec::new();\n"), "let v: Vec<String> = Vec::new();");
    assert_eq!(
        body_source("cell a;\nreceive(x);\nlet m: HashMap<u64, Order> = HashMap::new();\n"),
        "let m: HashMap<u64, Order> = HashMap::new();"
    );
    assert_eq!(
        body_source("cell a;\nreceive(x);\nlet n = a < b && c > d;\n"),
        "let n = a < b && c > d;"
    );
}

// ------------------------------------------------- 8.7 Rust item 透传

#[test]
fn nested_items_inside_cell_body() {
    let b = body("cell a;\nreceive(x);\nfn helper() -> u64 { 1 }\nstruct S { a: u8 }\nenum E { A }\nuse std::io;\nconst K: u64 = 1;\nstatic S2: u64 = 2;\ntype T = u64;\ntrait Tr { fn m(&self); }\nimpl T { fn m(&self) {} }\nmod m {}\ndie;\n");
    assert_eq!(b.len(), 11);
    for (i, expected) in [
        "fn helper() -> u64 { 1 }",
        "struct S { a: u8 }",
        "enum E { A }",
        "use std::io;",
        "const K: u64 = 1;",
        "static S2: u64 = 2;",
        "type T = u64;",
        "trait Tr { fn m(&self); }",
        "impl T { fn m(&self) {} }",
        "mod m {}",
    ]
    .iter()
    .enumerate()
    {
        match &b[i] {
            Stmt::RawRust(r) => assert_eq!(&r.text, expected, "第 {i} 条 item 透传"),
            other => panic!("第 {i} 条期望 RawRust，实际 {other:?}"),
        }
    }
    assert!(matches!(b[10], Stmt::Die { .. }));
}

#[test]
fn item_with_attribute_and_pub_visibility() {
    // §3.5.4 关键字集合的文档扩展：pub / async / unsafe / macro_rules
    let b = body("cell a;\nreceive(x);\n#[derive(Debug)]\nstruct S { a: u8 }\npub fn f() {}\npub(crate) fn g() {}\nasync fn h() {}\nunsafe fn i() {}\nmacro_rules! m { ($x:expr) => { $x + 1 } }\ndie;\n");
    let texts: Vec<String> = b
        .iter()
        .map(|s| match s {
            Stmt::RawRust(r) => r.text.clone(),
            _ => "?".to_string(),
        })
        .collect();
    assert_eq!(texts[0], "#[derive(Debug)]\nstruct S { a: u8 }");
    assert_eq!(texts[1], "pub fn f() {}");
    assert_eq!(texts[2], "pub(crate) fn g() {}");
    assert_eq!(texts[3], "async fn h() {}");
    assert_eq!(texts[4], "unsafe fn i() {}");
    assert_eq!(texts[5], "macro_rules! m { ($x:expr) => { $x + 1 } }");
    assert!(matches!(b[6], Stmt::Die { .. }));
}

#[test]
fn item_block_body_is_opaque() {
    // fn/impl 的块内不做 DSL 分派
    let b = body("cell a;\nreceive(x);\nfn helper() { if c { jump A go with(x); } die; }\ndie;\n");
    assert_eq!(b.len(), 2);
    match &b[0] {
        Stmt::RawRust(r) => assert_eq!(r.text, "fn helper() { if c { jump A go with(x); } die; }"),
        other => panic!("期望 RawRust（不透明 item），实际 {other:?}"),
    }
}

#[test]
fn extern_rust_block_is_opaque_inside_cell() {
    // §3.4.4：extern rust 块内不解析 `{}` 内部
    let b = body("cell a;\nreceive(x);\nextern rust { let s = \"die; jump\"; }\ndie;\n");
    match &b[0] {
        Stmt::ExternRustBlock { body, read_only, .. } => {
            assert!(!read_only);
            assert_eq!(body.text, "let s = \"die; jump\";");
        }
        other => panic!("期望 ExternRustBlock，实际 {other:?}"),
    }
}

// --------------------------------------- 透传与 DSL 交错的完整用例

#[test]
fn mixed_passthrough_and_dsl_body() {
    let b = body(
        "cell fact;\n\
         receive(n) as (num);\n\
         let mut sub_result: Option<i64> = None;\n\
         let next = num - 1;\n\
         jump recurse fact when num > 1 go with(next -> n) return(res -> sub_result) group r1;\n\
         jump sync r1;\n\
         let result = num * sub_result.unwrap();\n\
         exit go with(result);\n",
    );
    assert_eq!(b.len(), 6);
    assert!(matches!(b[0], Stmt::RawRust(_)));
    assert!(matches!(b[1], Stmt::RawRust(_)));
    assert!(matches!(b[2], Stmt::Jump(_)));
    assert!(matches!(b[3], Stmt::Sync { .. }));
    assert!(matches!(b[4], Stmt::RawRust(_)));
    assert!(matches!(b[5], Stmt::Exit { .. }));
}

#[test]
fn when_expression_keeps_rust_operators() {
    let b = body("cell a;\nreceive(val);\njump odd when val % 2 == 1 && !done go with(val);\ndie;\n");
    match &b[0] {
        Stmt::Jump(j) => assert_eq!(j.condition.as_ref().unwrap().text, "val % 2 == 1 && !done"),
        other => panic!("期望 Jump，实际 {other:?}"),
    }
}

#[test]
fn struct_literal_in_when_expression() {
    let b = body("cell a;\nreceive(o);\njump t when o == Point { x: 1, y: 2 } go with(o);\ndie;\n");
    match &b[0] {
        Stmt::Jump(j) => {
            assert_eq!(j.condition.as_ref().unwrap().text, "o == Point { x: 1, y: 2 }");
        }
        other => panic!("期望 Jump，实际 {other:?}"),
    }
}

#[test]
fn chinese_identifiers_and_comments() {
    // 源码允许中文注释与字符串：切片必须落在 UTF-8 字符边界上
    let src = "cell 计算;\nreceive(值);\n// 中文注释；含分号\nlet 变量 = \"中文\";\nexit go with(变量);\n";
    let p = ok(src);
    let c = p.cell("计算").expect("cell 计算");
    assert_eq!(c.receive.keys[0].name, "值");
    assert_eq!(c.body.len(), 2);
    match &c.body[0] {
        Stmt::RawRust(r) => assert_eq!(r.text, "let 变量 = \"中文\";"),
        other => panic!("期望 RawRust，实际 {other:?}"),
    }
    match &c.body[1] {
        Stmt::Exit { args, .. } => assert_eq!(args[0].expr.text, "变量"),
        other => panic!("期望 Exit，实际 {other:?}"),
    }
}

#[test]
fn multibyte_source_does_not_panic_anywhere() {
    // 逐字回切整个 body 必须等于源文本（零拷贝 A1 的端到端验证）
    let src = "cell a;\nreceive(x);\nlet s = \"中文 { } ; jump\";\n// 注释\nlet t = '中';\ndie;\n";
    let b = body(src);
    let joined: String = b
        .iter()
        .map(|s| match s {
            Stmt::RawRust(r) => format!("{}\n", r.text),
            _ => String::new(),
        })
        .collect();
    assert_eq!(joined, "let s = \"中文 { } ; jump\";\nlet t = '中';\n");
}
