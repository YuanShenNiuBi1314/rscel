//! 合法程序的 C1 契约校验（任务书 §3.3 矩阵第一行）。

use rscel_check::check_program;
use rscel_parser::parse_program;

fn run(src: &str) -> rscel_check::CheckOutcome {
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "解析失败: {:?}", out.diagnostics);
    check_program(&out.program)
}

#[test]
fn legal_program_yields_no_errors() {
    // §3.3 jump 段顺序：kind → targets → [when] → [mode] → [return] → [over]
    //                 → [concurrent_limit] → [group] → ;
    // 修复点：`when` 必须出现在 `go with` 之前。
    // 注：未引入 `jump and ... group / jump sync` —— 见 contracts_err_test 中 E035 的
    // 独立用例。本测试专注于段顺序修复与单纯 plain jump 链。
    let src = r#"
cell main;
receive();
jump a go with(7 -> x);

cell a;
receive(x);
jump b go with(x -> n);

cell b;
receive(n);
exit go with(n -> result);
"#;
    let outcome = run(src);
    let errors: Vec<_> = outcome
        .diagnostics
        .iter()
        .filter(|d| d.severity == rscel_parser::Severity::Error)
        .collect();
    assert!(errors.is_empty(), "合法程序不应有错误，实际: {:#?}", errors);
}

#[test]
fn legal_program_with_loop() {
    // §9.2 / §11.1：jump loop 实参个数 == receive 键数。
    // 修复点：扩 receive 为 2 键 (n, acc)，jump loop 实参同步为 2 个
    // （贴近开发说明 §13.4 附录 A.1 的阶乘 loop 版）。
    let src = r#"
cell main;
receive();
jump fact go with(5 -> n, 1 -> acc);

cell fact;
receive(n, acc) as (num, prev_acc);
jump loop fact when num > 1 go with((num - 1) -> n, (num * prev_acc) -> acc);
exit go with(prev_acc -> res);
"#;
    let outcome = run(src);
    let errors: Vec<_> = outcome
        .diagnostics
        .iter()
        .filter(|d| d.severity == rscel_parser::Severity::Error)
        .collect();
    assert!(errors.is_empty(), "合法 loop 程序不应有错误: {:#?}", errors);
}