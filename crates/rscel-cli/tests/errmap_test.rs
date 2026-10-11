//! §15.2 rustc 错误反映射的端到端验收。
//!
//! 与 `cli_smoke.rs` 的分工：那边验「build 能不能跑通」，这边验「跑不通的时候
//! 有没有把 rustc 的 `fl.rs:278:20` 换成 DSL 作者看得懂的 `cell main @ 3:1`」。
//!
//! **这些用例跑的是真 rustc、真 codegen、真映射表**，不是静态字符串断言：
//! 期望的 DSL 行号由源文本现场算出；stderr 通过 [`rscel_cli::run_build_with`] 的
//! sink 口子收集（`eprintln!` 打到真实 stderr，集成测试拿不到，
//! 而 workspace 里编译好的 `rscel` 二进制路径不稳定，`std::process::Command` 不可靠）。

use rscel_cli::errmap;
use rscel_cli::{run, run_build_with, BuildMode};
use rscel_gen::srcmap::parse_srcmap_json;
use rscel_parser::parse_program;
use std::path::PathBuf;

fn tmp_dir() -> PathBuf {
    let d = std::env::temp_dir().join("rscel-cli-errmap-tests");
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write_tmp(name: &str, src: &str) -> PathBuf {
    let path = tmp_dir().join(name);
    std::fs::write(&path, src).unwrap();
    path
}

/// DSL 源里第一条 trim 后等于 `needle` 的行号（1 起始）。
fn dsl_line(src: &str, needle: &str) -> usize {
    src.lines()
        .position(|l| l.trim() == needle)
        .unwrap_or_else(|| panic!("DSL 源里找不到 {needle:?}"))
        + 1
}

/// `build` 一个样例，返回 (退出码, 收集到的 stderr 文本)。
fn build(src: &str, name: &str, mode: BuildMode) -> (i32, String) {
    let file = write_tmp(&format!("{name}.rscel"), src);
    let out = tmp_dir().join(format!("{name}.exe"));
    let mut buf = String::new();
    let code = run_build_with(&file, &out, mode, &mut |line| {
        buf.push_str(line);
        buf.push('\n');
    });
    (code, buf)
}

// ------------------------------------------------------------------ RSC-E900

/// Cell 体内的透传 Rust 类型写错：check 放行（C 系列只查 DSL 语义，不查 Rust 类型），
/// rustc 报 E0308，反映射必须落回那条 DSL 语句自己的行列。
#[test]
fn rustc_type_error_is_mapped_back_to_dsl_position() {
    let src = "\
cell main;
receive();
let bad: i64 = \"字符串\";
exit go with();
";
    let (code, stderr) = build(src, "e900_case", BuildMode::Release);
    assert_eq!(code, 2, "rustc 失败应退 2（§4.1），实际 stderr:\n{stderr}");

    let line = dsl_line(src, "let bad: i64 = \"字符串\";");
    assert!(stderr.contains("RSC-E900"), "应报 RSC-E900，实际:\n{stderr}");
    assert!(
        stderr.contains(&format!("cell main @ {line}:1")),
        "映射后的 DSL 位置应是 cell main @ {line}:1，实际:\n{stderr}"
    );
    // 原消息摘要 + code 都要带上，否则用户没法判断是哪类错。
    assert!(stderr.contains("E0308"), "应带 rustc 的 E0308，实际:\n{stderr}");
    assert!(stderr.contains("mismatched types"), "应带 rustc 的 message，实际:\n{stderr}");
    // rustc 的汇总行（无 span）也必须出现在输出里：一条都不能丢。
    assert!(
        stderr.contains("aborting due to"),
        "rustc 汇总诊断不能被丢弃，实际:\n{stderr}"
    );
}

/// 走完整 CLI 入口（`rscel build ...`）也必须是退出码 2。
/// `run_build` 的签名是 `fn(&Path, &Path, BuildMode) -> i32`，`cli::run` 要的就是它，
/// 所以这条链路也要钉住。
#[test]
fn cli_entry_returns_two_on_rustc_error() {
    let src = "\
cell main;
receive();
let bad: i64 = 1.5;
exit go with();
";
    let file = write_tmp("e900_cli.rscel", src);
    let out = tmp_dir().join("e900_cli.exe");
    let code = run(&[
        "rscel".to_string(),
        "build".to_string(),
        file.to_string_lossy().into_owned(),
        "-o".to_string(),
        out.to_string_lossy().into_owned(),
        "--release".to_string(),
    ]);
    assert_eq!(code, 2, "rustc 类型错误应让 CLI 退 2");
}

// ------------------------------------------------------------------ RSC-E901

/// 错误位于 `extern rust { ... }` 块内：按 §15.2 只报 RSC-E901 + rustc 原文，
/// **不做** DSL 行映射（那块内容逐字来自用户，映射到 DSL 行反而误导）。
#[test]
fn error_inside_extern_rust_block_is_not_mapped() {
    let src = "\
cell main;
receive();
extern rust { let bad: i64 = \"字符串\"; };
exit go with();
";
    let (code, stderr) = build(src, "e901_case", BuildMode::Release);
    assert_eq!(code, 2, "实际 stderr:\n{stderr}");
    assert!(stderr.contains("RSC-E901"), "extern 块内的错误应报 RSC-E901，实际:\n{stderr}");
    assert!(stderr.contains("不做反映射"), "RSC-E901 要说明为什么不映射，实际:\n{stderr}");
    assert!(stderr.contains("mismatched types"), "要带上 rustc 原始 message，实际:\n{stderr}");

    let line = dsl_line(src, "extern rust { let bad: i64 = \"字符串\"; };");
    assert!(
        !stderr.contains(&format!("cell main @ {line}:1")),
        "extern 块不应该给出 DSL 行映射，实际:\n{stderr}"
    );
}

// ------------------------------------------------------------------ 未映射区间

/// 顶层 `extern rust`（§3.5.4 的隐含 extern）里的错误同样走 RSC-E901。
/// 这条同时验证「未映射区间不会把 rustc 的输出吃掉」——用户至少能看到原文。
#[test]
fn top_level_extern_rust_error_is_e901_too() {
    let src = "\
extern rust {
    fn broken() -> i64 { \"不是整数\" }
};

cell main;
receive();
exit go with();
";
    let (code, stderr) = build(src, "e901_top", BuildMode::Release);
    assert_eq!(code, 2, "实际 stderr:\n{stderr}");
    assert!(stderr.contains("RSC-E901"), "顶层 extern rust 的错误应报 RSC-E901，实际:\n{stderr}");
}

// ------------------------------------------------------------------ .map.json 落盘

/// §4.2：`build` 成功后同目录必须有 `<name>.map.json`，且内容能被自己的解析器读回
/// （`version == 1`）。顺带证明 `.rs` 与 `.map.json` 是同一次生成的、没有错位。
#[test]
fn successful_build_writes_readable_map_json() {
    let src = "\
cell main;
receive();
jump a go with(6 -> x);

cell a;
receive(x: i64);
out(\"mapped=\", x);
exit go with();
";
    let file = write_tmp("ok_case.rscel", src);
    let out = tmp_dir().join("ok_case.exe");
    let mut buf = String::new();
    let code = run_build_with(&file, &out, BuildMode::Release, &mut |l| buf.push_str(l));
    assert_eq!(code, 0, "build 应成功，输出: {buf}");

    let map_path = tmp_dir().join("ok_case.map.json");
    assert!(map_path.exists(), "应产出 {}", map_path.display());
    let text = std::fs::read_to_string(&map_path).unwrap();
    let map = parse_srcmap_json(&text).expect(".map.json 必须是合法 JSON 且能读回");
    assert_eq!(map.version, 1);
    assert_eq!(map.file, "ok_case.rs", "file 字段应等于同目录的生成 .rs 文件名");
    assert!(!map.entries.is_empty(), "映射表不该是空的");
    // 每条映射指向的生成行都必须真实存在（行号 1 起始，越界即错）。
    let rs = std::fs::read_to_string(tmp_dir().join("ok_case.rs")).unwrap();
    let n_lines = rs.lines().count();
    for e in &map.entries {
        assert!(e.rs_line_end <= n_lines, "映射区间 {}..={} 越过生成文件行数 {n_lines}", e.rs_line_start, e.rs_line_end);
    }
    assert!(buf.contains("ok_case.map.json"), "stderr 要报告两个产物: {buf}");
}

// ------------------------------------------------------------------ 手工构造的 rustc JSON

/// 用**真实生成物**的行号喂一段手工构造的 rustc NDJSON，验证渲染链路：
/// 「rustc 说的行 → 表里的区间 → DSL 的 cell + 行列」每一步都来自真实数据，
/// 只有 rustc 那一句是伪造的（因为不想为了凑诊断去改 codegen）。
#[test]
fn handcrafted_rustc_json_lands_on_the_right_dsl_statement() {
    let src = include_str!("../../rscel-gen/examples/factorial_loop.rscel");
    let parsed = parse_program(src, 0);
    assert!(!parsed.has_errors());
    let gen = rscel_gen::generate_as(&parsed.program, "factorial_loop.rs");
    let text = gen.source_map.as_deref().expect("必须有映射表文本");
    let map = parse_srcmap_json(text).expect("读回");

    // 找 `out(next_acc);` 那条语句的映射区间，拿区间的行号伪造一条 rustc 诊断。
    let dsl = dsl_line(src, "out(next_acc);");
    let entry = map.entries.iter().find(|e| e.dsl.line as usize == dsl).expect("out 语句应有映射");
    let fake = format!(
        r#"{{"$message_type":"diagnostic","message":"mismatched types","code":{{"code":"E0308","explanation":null}},"level":"error","spans":[{{"file_name":"out/factorial_loop.rs","byte_start":1,"byte_end":2,"line_start":{},"line_end":{},"column_start":12,"column_end":13,"is_primary":true,"text":[],"label":null,"suggested_replacement":null,"suggestion_applicability":null,"expansion":null}}],"children":[],"rendered":"..."}}"#,
        entry.rs_line_start, entry.rs_line_end
    );
    let rendered = errmap::render_diagnostics(&fake, &map, "factorial_loop.rscel", false);
    assert!(rendered.contains("RSC-E900"), "{rendered}");
    assert!(
        rendered.contains(&format!("cell fact @ {dsl}:1")),
        "应映射到 cell fact @ {dsl}:1（out 语句），实际: {rendered}"
    );
    assert!(rendered.contains("E0308"), "{rendered}");

    // 同一个诊断把行号换成运行时头部：必须走「未映射」而不是乱指一条语句。
    let in_runtime = fake.replace(
        &format!("\"line_start\":{}", entry.rs_line_start),
        &format!("\"line_start\":{}", map.runtime_lines / 2),
    );
    let rendered = errmap::render_diagnostics(&in_runtime, &map, "factorial_loop.rscel", false);
    assert!(rendered.contains("编译器自身生成区间"), "{rendered}");
    assert!(!rendered.contains("RSC-E900"), "{rendered}");
}

/// rustc 退出 0 但有 warning：只报告、**不判失败**（§16.1：只有非零退出才是编译失败）。
///
/// 样例用的是「Cell 体内一个没被读过的 `let`」——rustc 的 `unused_variables` 是稳定 lint，
/// 生成物的文件级 `#![allow(dead_code, unused_assignments, non_snake_case)]` 并不吞它，
/// 所以这条 warning 必然出现，测试不依赖 codegen 的边角行为。
/// 顺带验到「映射成功的 warning」用的是 RSC-E900 + 「rustc 警告」措辞（不是「编译错误」）。
#[test]
fn success_with_warnings_still_exits_zero_and_maps_them() {
    let src = "\
cell main;
receive();
let unused_var: i64 = 5;
exit go with();
";
    let (code, stderr) = build(src, "warn_case", BuildMode::Release);
    assert_eq!(code, 0, "有 warning 但 rustc 成功 → 仍应退 0，stderr:\n{stderr}");
    let line = dsl_line(src, "let unused_var: i64 = 5;");
    assert!(stderr.contains("unused variable"), "warning 必须报告到 stderr，实际:\n{stderr}");
    assert!(stderr.contains("RSC-E900"), "warning 也要走反映射，实际:\n{stderr}");
    assert!(stderr.contains("rustc 警告"), "warning 不该被写成「编译错误」，实际:\n{stderr}");
    assert!(
        stderr.contains(&format!("cell main @ {line}:1")),
        "warning 的位置应映射到 DSL 第 {line} 行，实际:\n{stderr}"
    );
}

// ---------------------------------------------- compile_error! 路径的诊断质量

/// 类型推不出（D2 的四档来源全部落空）时，codegen 只发一串 `compile_error!` 就提前返回。
/// 那条路径原先连 `fn main` 都没留下，于是 rustc 除了我们要它报的那条问题陈述，
/// 还会附带一条 `error[E0601]`：main 函数在这个 crate 里找不到（docs/05 P2）。
/// 用户既写不出也不需要 main，这条错误与他做的事毫无关系，却和真正的诊断同样醒目，
/// 结果「2 errors」里最该看的那一条被稀释掉了。
///
/// 这条走**真 rustc**：断言的是用户实际看到的那份输出，而不是生成物的字符串形状。
#[test]
fn compile_error_path_does_not_leak_e0601_noise() {
    let src = "\
cell main;
receive();
jump a go with(mystery -> x);

cell a;
receive(x);
exit go with();
";
    let (code, stderr) = build(src, "no_e0601_noise", BuildMode::Release);
    assert_eq!(
        code, 2,
        "compile_error! 必然让 rustc 失败（§4.1 退 2），实际:\n{stderr}"
    );
    assert!(
        stderr.contains("无法确定类型"),
        "rscel 自己的问题陈述必须保留，实际:\n{stderr}"
    );
    assert!(
        !stderr.contains("E0601") && !stderr.contains("function not found"),
        "不该出现与用户程序无关的 main-not-found 噪声，实际:\n{stderr}"
    );
    // 正面证据：带位置的 error 与 rustc 的汇总行都降到 1 条。
    assert!(
        stderr.contains("1 条带位置的 error 诊断"),
        "只剩我们那一条带位置诊断，实际:\n{stderr}"
    );
    assert!(
        stderr.contains("aborting due to 1 previous error"),
        "rustc 汇总也应只剩 1 条，实际:\n{stderr}"
    );
}
