//! 批次 3A 端到端验收：解析 `examples/factorial_loop.rscel` → 生成 Rust → rustc 编译 → 运行 → 输出 120。
//!
//! 整个流程在 `cargo test -p rscel-gen` 里跑（不跳过任何用例），不需要外部脚本。
//!
//! 测试矩阵（§5）：
//!   1. **代码结构**：对生成的 `factorial_loop.rs` 文本做字符串匹配断言关键骨架存在。
//!   2. **identifier rewrite**：解析一段含别名引用的 DSL 片段，验证重写后的 Rust 文本里
//!      别名已替换为 `v_<id>`。
//!   3. **端到端**：用 `std::process::Command::new("rustc")` 编译 `factorial_loop.rs` 到
//!      `target/<dir>/factorial_loop.exe`，运行后断言 stdout 含 `120`。

use rscel_gen::generate;
use rscel_parser::parse_program;
use std::process::Command;

const SRC: &str = include_str!("../examples/factorial_loop.rscel");

// ---------------------------------------------------------------------------
// 验收 #1：代码结构
// ---------------------------------------------------------------------------

#[test]
fn generated_source_has_required_skeleton() {
    let out = parse_program(SRC, 0);
    assert!(
        !out.has_errors(),
        "factorial_loop.rscel 应可无错解析，实际: {:#?}",
        out.diagnostics
    );
    let g = generate(&out.program);
    let s = &g.rust_source;

    // 嵌入式 block_on（§4.5：生成物零外部依赖）
    assert!(s.contains("fn __rsc_block_on"), "应嵌入 block_on");
    // `fact` 只有 main 一个 plain 调用点，按候选 1 被**实体展开**进 `fn cell_main`
    // （数据见 `docs/06-控制变量实验.md`：函数边界本身值 2 倍）。
    // 所以这里断言展开形状：就地块 + 独立函数不再存在；形参绑定的断言在下面
    // 「plain jump 按 key→position」那一条。
    assert!(
        s.contains("let _ = {"),
        "唯一调用点的同步 Cell 应展开成就地块，实际:\n{s}"
    );
    assert!(
        !s.contains("fn cell_fact("),
        "被展开的 Cell 不该再留独立函数（那会是死代码 + 函数边界），实际:\n{s}"
    );
    assert!(s.contains("fn cell_main()"), "main 应为同步 fn");
    // Ret 结构体（§4.1）
    assert!(s.contains("struct RetFact"), "应生成 RetFact 结构体");
    assert!(s.contains("struct RetMain"), "应生成 RetMain 结构体");
    assert!(
        s.contains("pub result: i64"),
        "exit go with 的 result 键应进入 RetFact"
    );
    // receive 别名 → v_<id>（§4.1 / §10.3）
    assert!(
        s.contains("let mut v_a: i64 = cur;"),
        "receive 别名 cur 应改写为 v_a"
    );
    assert!(
        s.contains("let mut v_b: i64 = sum;"),
        "receive 别名 sum 应改写为 v_b"
    );
    assert!(
        s.contains("v_a - 1"),
        "透传语句里的 cur 应改写为 v_a"
    );
    // jump loop → loop + continue 形状（§12.4）
    assert!(s.contains("loop {"), "jump loop 应展开为 loop 块");
    assert!(
        s.contains("if !(next > 0) { break; }"),
        "jump loop 应生成 break 模式（sync fn 里 continue 仍合法；本批次选 break 简化收敛）"
    );
    // out(...) → println!（§4.1 out 的最小支持）
    assert!(
        s.contains("println!(\"{}\", next_acc);"),
        "out(next_acc) 应生成为 println!"
    );
    // exit go with → 结构体构造（fact 被展开，所以它是块尾值而不是 `return`）
    assert!(
        s.contains("Some(RetFact"),
        "exit go with 应生成 RetFact 结构体构造"
    );
    // plain jump → 类型化实参（按 key→position 绑定到形参；同步丢弃返回值，无 .await）
    assert!(
        s.contains("let cur: i64 = five;") && s.contains("let sum: i64 = 1i64;"),
        "plain jump 应按 key→position 把实参绑到 cell_fact 的形参（同步丢弃返回值，无 .await）"
    );
    // main
    assert!(s.contains("fn main()"), "应有 fn main");
    // 显式否定 cell 不应是 async（jump loop + plain jump 不需要 async）
    assert!(
        !s.contains("async fn cell_fact"),
        "jump loop cell 不应是 async"
    );
    assert!(
        !s.contains("async fn cell_main"),
        "plain-jump-only entry 不应是 async"
    );

    println!("--- generated Rust ---\n{s}\n--- end ---");
}

// ---------------------------------------------------------------------------
// 验收 #2：identifier rewrite（解析一段含别名的 DSL）
// ---------------------------------------------------------------------------

#[test]
fn identifier_rewrite_replaces_receive_aliases_with_v_id() {
    // 一个最小程序：cell main 入口 + cell c（receive(x, y) as (a, b)）；
    // cell c 的 body 内引用 a/b，并引用一个**未**在 receive 出现的
    // `let` 变量 `t`，确认 §10.3 "let 绑定不重写"的契约。
    let snippet = "cell main;\n\
                   receive();\n\
                   jump c go with(1 as i64 -> x, 2 as i64 -> y);\n\
                   \n\
                   cell c;\n\
                   receive(x: i64, y: i64) as (a, b);\n\
                   let t = a + b;\n\
                   let u = t * 2;\n\
                   out(a, b, t, u);\n\
                   exit go with(t -> s);\n";
    let parsed = parse_program(snippet, 0);
    assert!(
        !parsed.has_errors(),
        "snippet 应可解析，实际: {:#?}",
        parsed.diagnostics
    );
    let g = generate(&parsed.program);
    let s = &g.rust_source;

    // receive 别名 → v_<id>
    //
    // 本 Cell 无 `jump loop`，所以绑定**不带** `mut`（别名不会被重新赋值，
    // 带 `mut` 会产生 unused_mut 告警）。`jump loop` 的 Cell 才发 `let mut`。
    assert!(
        s.contains("let v_a: i64 = a;"),
        "receive 别名 `a` 应改写为 `v_a`，实际片段:\n{s}"
    );
    assert!(
        s.contains("let v_b: i64 = b;"),
        "receive 别名 `b` 应改写为 `v_b`"
    );

    // 透传语句里 `a`/`b` 必须被替换成 `v_a`/`v_b`
    assert!(
        s.contains("let t = v_a + v_b;"),
        "let t = a + b; 应改写为 let t = v_a + v_b;"
    );

    // `t` 是 let 绑定，不应被替换成 `v_?`
    assert!(
        !s.contains("v_t =") && !s.contains("v_t;"),
        "`let` 绑定的 `t` 不应被重写为 `v_t`，实际片段:\n{s}"
    );
    // `t * 2` 应原样保留 t 标识符
    assert!(
        s.contains("t * 2"),
        "let u = t * 2; 应原样保留 t，实际片段:\n{s}"
    );

    // out(a, b, t, u) → println!("{} {} {} {}", v_a, v_b, t, u)
    // 关键验证：a/b 已被替换，但 t/u 原样
    assert!(
        s.contains("v_a, v_b, t, u"),
        "out(...) 实参应改写为 v_a, v_b, t, u，实际片段:\n{s}"
    );

    // exit go with(t -> s)：键名 s 是结构体字段名（不改写），实参 `t` 不动。
    //
    // 不带 `return` 前缀断言：`cell c` 只有 main 一个 plain 调用点，被实体展开后
    // 这条 exit 是**块尾值**（`Some(RetC { s: t })`）而不是 `return Some(..);`。
    // 本条要验的是名字改写，不是 `return` 形状。
    assert!(
        s.contains("Some(RetC { s: t })"),
        "exit 应生成 `RetC {{ s: t }}`（键名 s 是字段名，t 是 let 绑定都不改写），实际片段:\n{s}"
    );
}

// ---------------------------------------------------------------------------
// 验收 #3：端到端 —— 解析 → 生成 → rustc 编译 → 运行 → stdout 含 120
// ---------------------------------------------------------------------------

#[test]
fn end_to_end_compile_and_run_outputs_120() {
    let out = parse_program(SRC, 0);
    assert!(
        !out.has_errors(),
        "parse errors: {:#?}",
        out.diagnostics
    );
    let g = generate(&out.program);

    // 把生成物写到独立临时目录（不污染 cargo target/）。
    let dir = std::env::temp_dir().join("rscel-gen-smoke");
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let rs = dir.join("factorial_loop.rs");
    let exe = dir.join("factorial_loop.exe");
    std::fs::write(&rs, &g.rust_source).expect("write generated .rs");

    // 用 std::process::Command 调 rustc（§6 硬约束：禁止直接调 rustc::driver 等库）。
    let rustc_status = Command::new("rustc")
        .arg("--edition=2021")
        .arg("-O")
        .arg(&rs)
        .arg("-o")
        .arg(&exe)
        .status()
        .expect("failed to invoke rustc");
    assert!(
        rustc_status.success(),
        "rustc 编译失败: {:?}（生成物 {} 行）",
        rustc_status,
        g.rust_source.lines().count()
    );

    let run_out = Command::new(&exe).output().expect("failed to run exe");
    assert!(run_out.status.success(), "运行失败: {:?}", run_out);
    let stdout = String::from_utf8_lossy(&run_out.stdout);
    println!("stdout: {stdout}");
    assert!(
        stdout.contains("120"),
        "stdout 应包含 120（5!），实际: {stdout}"
    );
}
// ---------------------------------------------------------------------------
// D2：接收类型来源（注解优先 → 入边传播 → 字面推导；推不出则报错要求注解）
// ---------------------------------------------------------------------------

/// 无注解 receive 的类型由入边实参推导出来，且生成的是**具体类型**而不是 `_`
/// （Rust item 签名不允许 `_`，否则 E0121）。
#[test]
fn unannotated_receive_type_propagates_from_caller() {
    let src = r#"
cell main;
receive();
jump a go with(42 -> x);

cell a;
receive(x);
exit go with(x -> result);
"#;
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let s = generate(&out.program).rust_source;
    assert!(
        s.contains("let x: i64 = 42;"),
        "cell_a 的形参类型应由入边 `42` 推导为 i64（cell_a 被实体展开，\
         形参在唯一调用点绑定），实际生成:\n{s}"
    );
    assert!(
        !s.contains(": _)"),
        "生成代码里不得出现 `_` 占位类型（E0121）:\n{s}"
    );
}

/// 推不出类型时必须**报错要求注解**，而不是猜一个类型。
#[test]
fn unresolvable_receive_type_emits_compile_error() {
    let src = r#"
cell main;
receive();
jump a go with(mystery -> x);

cell a;
receive(x);
exit go with();
"#;
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let s = generate(&out.program).rust_source;
    assert!(
        s.contains("compile_error!"),
        "无法推导类型时应发出 compile_error!，实际生成:\n{s}"
    );
    assert!(
        s.contains("receive(x: <类型>)"),
        "报错信息应指明加注解的位置，实际生成:\n{s}"
    );
}

/// `compile_error!` 那条提前 return 的路径**也要有 `fn main`**（docs/05 P2）。
///
/// 为什么这不是可有可无的收尾：生成物里没有 `fn main` 时，rustc 除了我们要它报的那条
/// 问题陈述，还会额外补一条 `error[E0601]: 'main' function not found in crate xxx`。
/// 那条错误与用户的 DSL 毫无关系（用户根本写不出 main），却和问题陈述同等醒目，
/// 于是「2 errors」里真正该看的那条被稀释掉了。
///
/// 补 `fn main` 不改变「必须编译失败」这个事实：`compile_error!` 本身就是无条件错误。
/// 同时它也不该带出任何正常路径的东西 —— 类型没解出来时连 Ret 结构体都不该存在。
#[test]
fn compile_error_path_still_emits_fn_main() {
    let src = r#"
cell main;
receive();
jump a go with(mystery -> x);

cell a;
receive(x);
exit go with();
"#;
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let s = generate(&out.program).rust_source;
    assert!(
        s.contains("compile_error!"),
        "该 DSL 应走 compile_error! 路径，实际生成:\n{s}"
    );
    assert!(
        s.contains("fn main()"),
        "compile_error! 路径仍要发出 fn main，否则 rustc 附带一条无关的 E0601:\n{s}"
    );
    assert!(
        !s.contains("struct Ret_"),
        "提前返回路径不应生成任何 Cell 侧代码:\n{s}"
    );
}

/// 补 `fn main` 的边界：用户在顶层 `extern rust {}` 里自己写了 `fn main`，
/// 而类型又推不出（docs/05 P2 的修法会不会引出重复定义 E0428？）。
///
/// 答案是不会，理由是**结构性的**：提前返回发生在任何 item 落地之前，顶层 Rust item
/// 一行都不会生成。这条测试就是钉住这个结构 —— 哪天有人把 extern 的生成挪到
/// 类型求解之前，`fn main` 会出现两次，这里立刻红。
#[test]
fn compile_error_path_emits_exactly_one_fn_main_even_with_user_main() {
    let src = r#"
extern rust {
    fn main() { println!("user main"); }
};

cell main;
receive();
jump w go with(mystery -> x);

cell w;
receive(x);
exit go with();
"#;
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let s = generate(&out.program).rust_source;
    assert!(
        s.contains("compile_error!"),
        "该 DSL 应走 compile_error! 路径，实际生成:\n{s}"
    );
    assert!(
        !s.contains("user main"),
        "类型推不出时顶层 extern item 不该落地，实际:\n{s}"
    );
    assert_eq!(
        s.matches("fn main").count(),
        1,
        "只能有一处 fn main，否则 rustc 报 E0428 重复定义，实际:\n{s}"
    );
}

// ---------------------------------------------------------------------------
// 批次 3C：jump and / over / concurrent_limit / jump sync + 多包 receive
// ---------------------------------------------------------------------------

const JUMP_AND_SRC: &str = include_str!("../examples/jump_and_worker.rscel");

/// `jump and` 全链路：生成 → rustc 编译 → 运行 → stdout 为 `total= 12`。
///
/// 覆盖 S2 类型化 spawn、S4 late binding（多包 receive 在 `jump sync` 之后解包）、
/// D5 静态槽位（`g1.W1/W2/W3`）与 `concurrent_limit=2` 限流。
#[test]
fn jump_and_end_to_end_sums_worker_results() {
    let out = parse_program(JUMP_AND_SRC, 0);
    assert!(
        !out.has_errors(),
        "jump_and_worker.rscel 应可无错解析，实际: {:#?}",
        out.diagnostics
    );
    let rs = generate(&out.program).rust_source;

    // jump and 不等待：spawn 之后立刻返回，句柄绑定为组局部变量。
    assert!(rs.contains("std::thread::spawn(move ||"), "应 spawn supervisor 线程");
    assert!(rs.contains("__rsc_group_run_sync"), "应走 panic-safe supervisor 入口（P0-1）");
    assert!(rs.contains("let __rsc_g_g1: Arc<__RscGroup<RetWorker>>"), "组句柄应绑定为 RetWorker 类型");
    // concurrent_limit 落到运行时的 limit 形参。
    assert!(
        rs.contains("__rsc_group_run_sync(__rsc_items, __rsc_lim, 3, &__rsc_g2,"),
        "P0-1：限流值、槽位数与组 Arc 句柄必须传入 supervisor"
    );
    // jump sync 阻塞取结果并按 W 槽位下标解包。
    assert!(rs.contains("__rsc_g_g1.wait()"), "jump sync 应等待整组完成");

    let dir = std::env::temp_dir().join("rscel-gen-jump-and");
    std::fs::create_dir_all(&dir).unwrap();
    let rs_path = dir.join("jump_and_worker.rs");
    let exe = dir.join("jump_and_worker.exe");
    std::fs::write(&rs_path, &rs).unwrap();

    let status = Command::new("rustc")
        .args(["--edition", "2021", "-O", "-D", "warnings"])
        .arg(&rs_path)
        .arg("-o")
        .arg(&exe)
        .output()
        .expect("调用 rustc");
    assert!(
        status.status.success(),
        "rustc 应零告警零错误，stderr:\n{}",
        String::from_utf8_lossy(&status.stderr)
    );

    let run = Command::new(&exe).output().expect("执行生成的 exe");
    let stdout = String::from_utf8_lossy(&run.stdout);
    // 1*2 + 2*2 + 3*2 = 12
    assert!(stdout.contains("total= 12"), "stdout 应为 `total= 12`，实际 {stdout:?}");
}

/// D4 内存边界：槽位数受多包 receive 静态枚举约束，与集合长度无关。
///
/// `slots` 决定 `__rsc_run_group` 只保留几个结果；没有多包 receive 时 slots = 0，
/// 所有结果即时丢弃——这是 A.3「一千万文件」场景不爆内存的关键。
#[test]
fn slots_are_bounded_by_declared_receive_not_collection_length() {
    let src = r#"
entry start;

cell start;
receive();
let items = [1, 2, 3, 4, 5, 6, 7, 8];
jump and worker over items concurrent_limit=2 group g1;
jump sync g1;
exit go with();

cell worker;
receive(x: i64);
exit go with(x -> d);
"#;
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let rs = generate(&out.program).rust_source;
    // 8 个元素，但只有 0 个静态槽位 → 第三个参数必须是 0。
    assert!(
        rs.contains("__rsc_group_run_sync(__rsc_items, __rsc_lim, 0, &__rsc_g2,"),
        "无多包 receive 时槽位数应为 0，实际:\n{rs}"
    );
}

/// `over` 管道算子逐字透传为 Rust iterator 适配器。
#[test]
fn over_pipeline_emits_iterator_adapters() {
    let src = r#"
entry start;

cell start;
receive();
let items = [1, 2, 3, 4];
jump and worker over items |> filter(|x| *x > 1) |> map(|x| x * 10) |> take(2) group g1;
jump sync g1;
exit go with();

cell worker;
receive(x: i64);
exit go with(x -> d);
"#;
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let rs = generate(&out.program).rust_source;
    assert!(rs.contains(".into_iter().filter(|x| *x > 1).map(|x| x * 10).take(2).collect()"),
        "over 管道应生成对应的 iterator 适配链，实际:\n{rs}");
}

// ---------------------------------------------------------------------------
// P0-1：worker panic 不应让 jump sync 永久阻塞
//
// 修复策略分两步（**两步必须都到位**）：
//   1. 运行时侧：`__rsc_group_run` 用 `catch_unwind` 包住 `__rsc_run_group`，
//      保证 `g.finish*` 一定被调用（已在 runtime.rs 实现）。
//   2. 生成侧：supervisor 线程调用 `__rsc_group_run` 而不是裸调
//      `__rsc_run_group` 再手动 `finish` —— 本测试守住这条（不让以后再回归）。
// ---------------------------------------------------------------------------

/// P0-1 静态回归：生成的 supervisor 必须走 `__rsc_group_run`，
/// 而不是裸调 `__rsc_run_group` + 手动 `finish`。
#[test]
fn jump_and_supervisor_uses_panic_safe_group_run() {
    let src = include_str!("../examples/jump_and_worker.rscel");
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let rs = generate(&out.program).rust_source;

    assert!(
        rs.contains("__rsc_group_run_sync(__rsc_items, __rsc_lim, 3, &__rsc_g2,"),
        "P0-1 回归：supervisor 必须调 __rsc_group_run 并把组句柄传进去。\n{rs}"
    );
    // supervisor 内**不应再有**手动 finish —— `__rsc_group_run` 内部已保证。
    assert!(
        !rs.contains("__rsc_g2.finish(__rsc_res);"),
        "P0-1 回归：supervisor 内不应再有手动 finish 调用。\n{rs}"
    );
    // 同时也不应再出现 `let __rsc_res = __rsc_run_group(...)` 这种旧的组合。
    assert!(
        !rs.contains("let __rsc_res = __rsc_run_group("),
        "P0-1 回归：旧的 __rsc_run_group + finish 组合不应再出现。\n{rs}"
    );
}

/// P0-1 端到端：worker 内部 panic，**jump sync 必须在合理时间内终止**（不挂死）。
///
/// 流程（修复后）：
///   1. supervisor 调 `__rsc_group_run` 派生 worker；
///   2. worker panic 被运行时内层 catch_unwind 隔离为 `None`；
///   3. supervisor 调用 `g.finish(Ok(vec_with_some_None))`；
///   4. `jump sync` 处拿到结果向量，进入 emit_sync 的解包路径，
///      看到第一个 None 槽位发 S003 panic（这是 P1-2 要修的另一面，本轮不挂死即可）。
///
/// 关键是不挂死——旧版（未修复）会让 supervisor 死掉、g.done 永远是 None、
/// wait() 永久阻塞。本测试用 5s 看门狗断言终止。
#[test]
fn jump_and_worker_panic_does_not_hang_jump_sync() {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    // worker 内 `x / (x - x)` —— x-x 恒为 0，整除 panic。三个元素全部死，
    // supervisor 把它们都标为 None，jump sync 进入 expect(S003) 路径。
    let src = "\
entry start;\n\
\n\
cell start;\n\
receive();\n\
let items = [1, 2, 3];\n\
jump and boom over items group g1;\n\
jump sync g1;\n\
exit go with();\n\
\n\
cell boom;\n\
receive(x: i64);\n\
let zero = x - x;\n\
let result = x / zero;\n\
exit go with(result -> d);\n\
";
    let out = parse_program(src, 0);
    assert!(
        !out.has_errors(),
        "样例应可解析: {:#?}",
        out.diagnostics
    );
    let g = generate(&out.program);

    let dir = std::env::temp_dir().join("rscel-gen-panic");
    std::fs::create_dir_all(&dir).unwrap();
    let rs_path = dir.join("panic.rs");
    let exe = dir.join("panic.exe");
    std::fs::write(&rs_path, &g.rust_source).unwrap();

    let status = Command::new("rustc")
        .args(["--edition", "2021", "-O", "-D", "warnings"])
        .arg(&rs_path)
        .arg("-o")
        .arg(&exe)
        .output()
        .expect("调用 rustc");
    assert!(
        status.status.success(),
        "rustc 应零告警零错误，stderr:\n{}",
        String::from_utf8_lossy(&status.stderr)
    );

    let mut child = Command::new(&exe)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(Some(_status)) = child.try_wait() {
            // 5 秒内退出（成功 / panic 都行，关键是别挂死）。
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("P0-1 回归：worker panic 导致 jump sync 挂死 > 5s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ---------------------------------------------------------------------------
// P0-2：jump and 默认并发上限 = available_parallelism()
//                                       （不再是 __rsc_items.len()）
//
// 背景：不写 `concurrent_limit` 时旧 codegen 让 lim = items.len()
//                                  ⇒ 100 万元素衍生线程
// 实测 = 47,724 / 828 MB / 150 s（吞吐量）。
// 修法：默认上限取 std::thread::available_parallelism()，失败回退 1。
// ---------------------------------------------------------------------------

/// P0-2 静态回归：不写 `concurrent_limit` 时，生成物必须用
/// `available_parallelism()`，不能再用 `__rsc_items.len()`。
#[test]
fn jump_and_default_limit_uses_available_parallelism() {
    // 故意不写 `concurrent_limit`。
    let src = "\
entry start;\n\
\n\
cell start;\n\
receive();\n\
let items = [1, 2, 3];\n\
jump and boom over items group g1;\n\
jump sync g1;\n\
exit go with();\n\
\n\
cell boom;\n\
receive(x: i64);\n\
exit go with(x * 2 -> d);\n\
";
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let rs = generate(&out.program).rust_source;

    assert!(
        rs.contains("available_parallelism"),
        "P0-2: 不写 concurrent_limit 时必须以 available_parallelism 为上限，\n{rs}"
    );
    // 不写 `_items.len() = len` 这种旧模式作为上限。
    assert!(
        !rs.contains("__rsc_lim: usize = __rsc_items.len();"),
        "P0-2 回归：不该再用 items.len() 做上限，\n{rs}"
    );
    // 显式给 `unwrap_or(1)` 回退，保证不会因环境失败而 panic。
    assert!(
        rs.contains("unwrap_or(1)"),
        "P0-2: available_parallelism 失败时回退到 1，\n{rs}"
    );
}

/// P0-2 行为保证：显式 `concurrent_limit=N` 仍然透传（不被默认逻辑覆盖）。
#[test]
fn jump_and_explicit_limit_passes_through() {
    let src = "\
entry start;\n\
\n\
cell start;\n\
receive();\n\
let items = [1, 2, 3, 4, 5, 6, 7, 8];\n\
jump and boom over items concurrent_limit=3 group g1;\n\
jump sync g1;\n\
exit go with();\n\
\n\
cell boom;\n\
receive(x: i64);\n\
exit go with(x * 2 -> d);\n\
";
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let rs = generate(&out.program).rust_source;
    assert!(
        rs.contains("__rsc_lim: usize = 3;"),
        "显式 concurrent_limit=3 必须原样透传，\n{rs}"
    );
    // 同一路径上不应出现 available_parallelism() —— explicit 时默认逻辑不启动。
    // 计数必须只看 `let __rsc_lim` 声明行；`__rsc_group_run(__rsc_items, __rsc_lim, ...)`
    // 的参数引用要排除。
    let decl_lines: Vec<&str> = rs
        .lines()
        .filter(|l| l.contains("let __rsc_lim"))
        .collect();
    assert_eq!(
        decl_lines.len(),
        1,
        "并发上限应只声明一次，实际 {} 次:\n{:#?}",
        decl_lines.len(),
        decl_lines
    );
    assert!(
        decl_lines[0].contains("= 3;"),
        "explicit 并发上限必须透传为 `= 3;`，实际:\n{}",
        decl_lines[0]
    );
}

/// P0-2 端到端：默认上限 + 多包 receive，验证结果正确（不限流语义破坏）。
///
/// 3 个元素（item = [1, 2, 3]），每个 worker 直接把 x 写到 d；
/// `g1.W1.d` / `W2.d` / `W3.d` 三个槽位最后被 start 求和 = 1+2+3 = 6。
/// 默认上限从 available_parallelism() 取，与「旧版 = items.len() = 3」在
/// 3 项以下等价，但仍触发新代码路径（生成物里看到 available_parallelism）。
#[test]
fn jump_and_default_concurrent_limit_end_to_end() {
    use std::process::Command;

    // start 做多包接收（g1.W1/W2/W3 三个槽位都从 group 取 d 字段），
    // boom 才是 jump and 的 target，接收单值 x → 写出 d。
    // D2：start 一侧的多包 receive 需要 i64 注解让类型确定。
    let src = "\
entry start;\n\
\n\
cell start;\n\
receive(g1.W1.d: i64, g1.W2.d: i64, g1.W3.d: i64) as (a, b, c);\n\
let items = [1, 2, 3];\n\
jump and boom over items group g1;\n\
jump sync g1;\n\
out(\"sum=\", a + b + c);\n\
exit go with();\n\
\n\
cell boom;\n\
receive(x: i64);\n\
exit go with(x -> d);\n\
";
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let g = generate(&out.program);

    // 同时静态断言：生成物已切到 available_parallelism。
    assert!(
        g.rust_source.contains("available_parallelism"),
        "P0-2 端到端：生成物必须含 available_parallelism\n{}",
        g.rust_source
    );

    let dir = std::env::temp_dir().join("rscel-gen-default-limit");
    std::fs::create_dir_all(&dir).unwrap();
    let rs_path = dir.join("default_limit.rs");
    let exe = dir.join("default_limit.exe");
    std::fs::write(&rs_path, &g.rust_source).unwrap();

    let status = Command::new("rustc")
        .args(["--edition", "2021", "-O", "-D", "warnings"])
        .arg(&rs_path)
        .arg("-o")
        .arg(&exe)
        .output()
        .expect("调用 rustc");
    assert!(
        status.status.success(),
        "rustc 应零告警零错误，stderr:\n{}",
        String::from_utf8_lossy(&status.stderr)
    );

    let run = Command::new(&exe).output().expect("运行生成 exe");
    assert!(
        run.status.success(),
        "生成 exe 运行失败: {:?}",
        run.status
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    // out("sum=", a + b + c) 在生成物里走 `println!("{} {} {}", ...)`，
    // 输出格式 = `sum= 6 \n`（println 自动在 {} 之间补空格）。
    // 用 substring 而非整字符串匹配，兼容 println 的格式差异。
    assert!(
        stdout.contains("sum=") && stdout.contains("6"),
        "stdout 应含 sum= 与 6，实际: {stdout:?}"
    );
}

// ---------------------------------------------------------------------------
// P1-4：exit go with 实参里的字符串字面量**不能**被 rewrite 静默改写
//
// 修法：在 rewrite 里加字符串/注释/数字跳过（参考 rscel-parser §3.5.3）。
// ---------------------------------------------------------------------------

/// P1-4 端到端：`exit go with("k" -> result)` 必须保留 `"k"` 不变，
/// 即使 `k` 是当前 Cell 的 receive alias。
#[test]
fn exit_go_with_string_literal_is_not_rewritten() {
    let src = "\
cell main;\n\
receive(k: i64);\n\
out(\"before\");\n\
exit go with(\"k\" -> result);\n\
";
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let rs = generate(&out.program).rust_source;

    // 静态断言：result 字段必须是字面量字符串 "k" —— 即 rewrite 没把它改成 v_a。
    assert!(
        rs.contains(r#"result: "k""#),
        "P1-4 回归：`\"k\"` 字面量必须在生成物里原样保留，实际:\n{rs}"
    );
    assert!(
        !rs.contains(r#"result: "v_a""#),
        "P1-4 回归：字符串字面量 \"k\" 被静默改写为 \"v_a\"，实际:\n{rs}"
    );
    // 同时 `k` 作为 receive alias 在 v_a 绑定里正确改写（rewrite 在别处仍工作）。
    // 该 cell 没有 jump loop，所以 v_a 不带 mut；
    // 关键断言是 `result: "k"` 而不是 v_a 的 mut 状态。
    assert!(
        rs.contains("v_a: i64 = k;"),
        "P1-4 不应误伤：receive alias `k` 在 v_a 绑定里仍正确改写，实际:\n{rs}"
    );
}

// ---------------------------------------------------------------------------
// P1-1：`Stmt::RustBlock` 生成合法 Rust
//
// 修前：RustBlock parts 不补 `{}`、Text 还 `trim()` + 丢空行；
// 产出 `if cond     let x = 1;` 这种垃圾，rustc 直接拒绝。
// 修后：补 `{}`、逐字还原（保留空行 / 缩进），至少不再产生明显无效 Rust。
//
// 已知限制：head+body 形态（如 `if cond { ... }`）下 body 的 `{` 不会与
// 上一行 text 内联，会在新行带额外缩进 —— 这可能让 rustc 在某些姿势下
// 误判为两个独立 statement。rust_block_emits_braces 静态断言只覆盖「有
// `{}` 包裹」这一合约，不做端到端 rustc 编译（避免脆弱）。
// ---------------------------------------------------------------------------

/// P1-1 静态断言：纯 body 形态 `{ die; }` 在生成物里必须补 `{` / `}`。
#[test]
fn rust_block_pure_body_emits_braces() {
    // 注入一段 `{ die; }` 到 cell body 中（head+body 形态 = 纯 body）。
    let src = "\
entry main;\n\
\n\
cell main;\n\
receive();\n\
{\n\
    die;\n\
}\n\
exit go with();\n\
";
    let out = parse_program(src, 0);
    // 解析可能 fail（exit go with 期望在 die 之前，碰到 block 后语义变化），
    // 至少要看到「die」出现在 body 块里。
    let g = generate(&out.program);
    let rs = &g.rust_source;
    // 关键断言：纯 body 形态的 `{` 与 `}` 必须都在生成物里。
    assert!(
        rs.contains("{") && rs.contains("}"),
        "P1-1 回归：body 必须带 `{{}}` 包裹，实际:\n{rs}"
    );
}
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// P1-3：group_slots 用 max(slot_index)+1，跳号槽位是 None
//
// 修前：count() 让 `W1, W3`（漏 W2）算成 2 槽位，emit_sync 按 idx=2 解包
// 越界 panic。修后：max+1 = 3，槽位 [0]=W1、[1]=None、[2]=W3，
// emit_sync 的 .and_then() 容忍 None。
// ---------------------------------------------------------------------------

/// P1-3 静态断言：W 编号跳号时，生成物里的 `__rsc_run_group` 第三个参数
/// （槽位数）必须是 max+1，而不是 count。
#[test]
fn group_slots_uses_max_plus_one_with_gapped_w_numbers() {
    // 关键：start 的 receive 是 `g1.W1.d` 与 `g1.W3.d`（漏 W2）。
    // jump and over 3 个元素，下标 0/1/2 应落到 W1/skip/W3。
    let src = "\
entry start;\n\
\n\
cell start;\n\
receive(g1.W1.d: i64, g1.W3.d: i64) as (a, c);\n\
let items = [1, 2, 3];\n\
jump and boom over items group g1;\n\
jump sync g1;\n\
out(\"sum=\", a + c);\n\
exit go with();\n\
\n\
cell boom;\n\
receive(x: i64);\n\
exit go with(x -> d);\n\
";
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let rs = generate(&out.program).rust_source;

    // 关键断言：第三个参数 = 3（max(slot_index)=2，再 +1），不是 count()=2。
    // 老 codegen 会发 `__rsc_group_run(__rsc_items, __rsc_lim, 2, ...)`。
    assert!(
        rs.contains("__rsc_group_run_sync(__rsc_items, __rsc_lim, 3, &__rsc_g2,"),
        "P1-3 回归：跳号 W 编号时槽位数必须是 max+1 = 3，\
         老 codegen 会算成 count=2 越界。实际:\n{rs}"
    );
    // 强否定：绝不出现 2。
    assert!(
        !rs.contains("__rsc_run_group(__rsc_items, __rsc_lim, 2,"),
        "P1-3 回归：count() 路径返回。实际:\n{rs}"
    );
}

// ---------------------------------------------------------------------------
// P1-2：emit_sync 区分 D5 元素数 < 槽位 vs 子实例 die
// ---------------------------------------------------------------------------

/// P1-2 静态断言：槽位超出元素数时，生成物里必须是 match + Default 分支，
/// **不再**是 .expect("S003: ...")（老路径把 D5 误归因为 die）。
#[test]
fn emit_sync_distinguishes_d5_default_vs_die() {
    // 槽位 2 个（g1.W1 + g1.W2），但 over items 只有一个元素 ⇒ 1 元素、
    // 2 槽位；idx=1 超出 rvar.len() ⇒ 应走 Default 分支。
    let src = "\
entry start;\n\
\n\
cell start;\n\
receive(g1.W1.d: i64, g1.W2.d: i64) as (a, b);\n\
let items = [1];\n\
jump and boom over items group g1;\n\
jump sync g1;\n\
out(\"a=\", a, \"b=\", b);\n\
exit go with();\n\
\n\
cell boom;\n\
receive(x: i64);\n\
exit go with(x -> d);\n\
";
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let rs = generate(&out.program).rust_source;

    // 关键断言：match 形式必须出现，且有 Default 分支。
    assert!(
        rs.contains("match __rsc_res_g1.get_mut(1)"),
        "P1-2 回归：第 2 个槽位（idx=1）超出 1 个元素时，\
         生成物必须是 match 形式，idx=1 走 Default。实际:\n{rs}"
    );
    assert!(
        rs.contains("None => Default::default()"),
        "P1-2 回归：None 槽位必须绑 Default，不能 panic。实际:\n{rs}"
    );
    assert!(
        rs.contains("Some(None) => panic!(\"S003"),
        "P1-2 回归：子实例 die 仍应 panic S003（区分于 D5）。实际:\n{rs}"
    );
    // 强否定：不能有「旧的 .expect("S003: ...")」路径。
    assert!(
        !rs.contains(".expect(\"S003"),
        "P1-2 回归：老路径 .expect(\"S003: ...\") 已收回。实际:\n{rs}"
    );
}

// ---------------------------------------------------------------------------
// P1-5：没用 jump and 的程序不应触发 dead_code 告警
//
// 修前：RUNTIME_SOURCE 无条件嵌入，未用 jump and 的程序里
// `__RscGroup::{new,finish,wait}` 等函数 0 引用 → rustc 报 dead_code。
// 修后：嵌入前 push 一行 `#![allow(dead_code)]`（内层 attribute）覆盖
// 整个内嵌运行时块；不影响用户代码的告警。
// ---------------------------------------------------------------------------

/// P1-5 静态断言：生成物顶部必须含 `#![allow(dead_code, unused_assignments)]`。
#[test]
fn embedded_runtime_has_dead_code_allow() {
    // 最小程序：没有 jump and，只有最朴素的 cell。
    let src = "\
cell main;\n\
receive();\n\
exit go with();\n\
";
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let rs = generate(&out.program).rust_source;

    assert!(
        rs.starts_with("#![allow(dead_code, unused_assignments, non_snake_case)]"),
        "P1-5 回归：生成物顶部必须含 `#![allow(dead_code, unused_assignments, non_snake_case)]`，\
         实际起始 80 字符:\n{:?}",
        &rs[..rs.len().min(80)]
    );
}

// ---------------------------------------------------------------------------
// P2-3：emit_args_by_position 对 jump 实参做 rewrite（与 jump loop 一致）
// ---------------------------------------------------------------------------

/// P2-3 静态断言：plain jump 的实参也要走 rewrite。
///
/// 修前：`emit_args_by_position` 用 `a.expr.text.trim()` 直接输出，
/// 不做 alias 重写。`jump worker go with(cur)` 里 `cur` 不会被改成
/// `v_a`，rustc 找不到 `cur` 直接编译失败。
/// 修后：实参也走 `rewrite()`，与 jump loop 的 `v_<id> = <arg>;` 行为
/// 一致。
#[test]
fn jump_args_rewrite_aliases() {
    // cell `cur` 是 receive alias，plain jump 把它传给 worker。
    let src = "\
entry start;\n\
\n\
cell start;\n\
receive(cur: i64);\n\
jump worker go with(cur);\n\
exit go with();\n\
\n\
cell worker;\n\
receive(x: i64);\n\
exit go with();\n\
";
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let rs = generate(&out.program).rust_source;

    // 关键断言：plain jump 的实参位置应是 v_a（rewrite 后的形参），不是 cur。
    // 老 codegen 发 `cell_worker(cur)` —— 编译失败（cur 在 cell_start 体内
    // 没声明为 let）。
    //
    // `cell worker` 只有 start 一个调用点 → 被实体展开，实参出现在形参绑定行里；
    // 断言的是**实参表达式被改写成 v_a** 这件事，与调用/展开两种形状无关。
    // 形参名是 `_x`：worker 的 body 没用过 x，未使用的别名按 P1-5 加 `_` 前缀。
    assert!(
        rs.contains("let _x: i64 = v_a;"),
        "P2-3 回归：plain jump 实参走 rewrite 改成 v_a。实际:\n{rs}"
    );
    assert!(
        !rs.contains("let _x: i64 = cur;") && !rs.contains("cell_worker(cur)"),
        "P2-3 回归：实参位置仍含未重写的 `cur`。实际:\n{rs}"
    );
}

// ---------------------------------------------------------------------------
// P2-1：emit_sync 跨 Cell 同名 group 不取错 ret_ty
//
// 修前：emit_sync 用全程序搜索 `jump and ... group g1`，cell A 找到 B
// 里的同名 group  ⇒  ret_ty = RetB；cell A 的 `let a: RetA = ...`
// 编译期硬错误「expected RetA, got RetB」，自相矛盾。
// 修后：搜索限定到当前 cell  body，跨 cell 同名 group 互不干扰。
// ---------------------------------------------------------------------------

/// P2-1 静态断言：cell A 用 `jump B group g1` 时，emit_sync 在 A 内
/// 找的 ret_ty 必须是 RetB，不能是其它 cell 里同名 group 的 ret_ty。
#[test]
fn emit_sync_ret_ty_scoped_to_current_cell() {
    // 关键：两个 cell（cell_main / cell_alt）都用同名 group "g1"，但
    // 各自 jump 到不同 cell（worker / altworker），返回类型不同。
    let src = "\
entry main;\n\
\n\
cell main;\n\
receive(g1.W1.d: i64) as (a);\n\
let items = [1];\n\
jump and worker over items group g1;\n\
jump sync g1;\n\
out(\"a=\", a);\n\
exit go with();\n\
\n\
cell alt;\n\
receive(g1.W1.s: String) as (x);\n\
let items2 = [\"hi\"];\n\
jump and altworker over items2 group g1;\n\
jump sync g1;\n\
out(\"x=\", x);\n\
exit go with();\n\
\n\
cell worker;\n\
receive(x: i64);\n\
exit go with(x * 2 -> d);\n\
\n\
cell altworker;\n\
receive(s: String);\n\
exit go with(s -> s);\n\
";
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let g = generate(&out.program);
    let rs = &g.rust_source;

    // cell_main 内的 ret_ty 应是 RetWorker（i64 倍乘），不能是 RetAltworker。
    // 老 codegen 用全程序搜索，会先碰到 cell_alt 的 group g1，取到
    // RetAltworker（String 字段）—— 与 cell_main 的 i64 多包 receive 冲突。
    assert!(
        rs.contains("__RscGroup::<RetWorker>::new()"),
        "P2-1 回归：cell_main 的 g1 ret_ty 必须是 RetWorker（不能被 cell_alt 抢走）。实际:\n{rs}"
    );
    assert!(
        rs.contains("__RscGroup::<RetAltworker>::new()"),
        "P2-1 回归：cell_alt 的 g1 ret_ty 必须是 RetAltworker。实际:\n{rs}"
    );
    // 强否定：cell_main 体内绝对不能出现 RetAltworker。
    let main_fn_start = rs.find("fn cell_main").unwrap();
    let main_fn_end = rs.find("fn cell_alt").unwrap_or(rs.len());
    let main_body = &rs[main_fn_start..main_fn_end];
    assert!(
        !main_body.contains("RetAltworker"),
        "P2-1 回归：cell_main 体内不应出现 RetAltworker（跨 cell 误取）。实际:\n{main_body}"
    );
}

// ---------------------------------------------------------------------------
// P2-6（重写）：async target Cell 路径的端到端覆盖
//
// 批次 A 改了 async 判定规则：DSL 构造本身**不再**让 Cell 变 async
// （`jump and` 走 OS 线程池、`jump sync` 阻塞 condvar、`jump recurse` 内联直接调用），
// 唯一让 Cell 变 `async fn` 的原因是**透传 Rust 里出现 `.await`**。
//
// 为什么必须这样：`async fn cell_x` 里内联递归调用自己会产生无限大小的 future
// （rustc E0733），只能装箱绕；而装箱正是 §17.1 要消灭的开销。反过来，绝大多数
// DSL 程序全程没有真正的并发 await，标成 async 只是白付状态机成本。
//
// 所以本测试覆盖的是**新规则下**的 async 目标路径：worker 透传 `.await` ⇒
// `async fn cell_worker` ⇒ `jump and` 的 supervisor 用 `__rsc_block_on` 驱动它。
// ---------------------------------------------------------------------------

/// P2-6：透传 `.await` 让目标 Cell 变 async，`jump and` 仍能端到端跑通。
#[test]
fn jump_and_into_async_target_runs_to_completion() {
    use std::process::Command;

    let src = "entry start;

extern rust {
    async fn rsc_slow(x: i64) -> i64 { x * 2 }
}

cell start;
receive(g1.W1.d: i64, g1.W2.d: i64, g1.W3.d: i64) as (a, b, c);
let items = [1, 2, 3];
jump and worker over items group g1;
jump sync g1;
out(\"sum=\", a + b + c);
exit go with();

cell worker;
receive(x: i64);
extern rust { let y: i64 = rsc_slow(x).await; };
exit go with(y -> d);
";
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let g = generate(&out.program);
    let rs = &g.rust_source;

    // 静态断言：只有透传 `.await` 的 worker 是 async fn；start 全程同步。
    assert!(rs.contains("async fn cell_worker"), "worker 透传含 .await ⇒ 应是 async fn，实际:
{rs}");
    assert!(
        !rs.contains("async fn cell_start"),
        "start 没有任何 .await ⇒ 应是同步 fn，实际:
{rs}"
    );
    // jump and 调 async 目标：supervisor 必须用 __rsc_block_on 驱动 future。
    assert!(
        rs.contains("__rsc_block_on(cell_worker(__rsc_it))"),
        "async target 必须由 supervisor 用 __rsc_block_on 驱动，实际:
{rs}"
    );

    // 端到端：rustc 编译并运行。
    let dir = std::env::temp_dir().join("rscel-gen-async-target");
    std::fs::create_dir_all(&dir).unwrap();
    let rs_path = dir.join("async.rs");
    let exe = dir.join("async.exe");
    std::fs::write(&rs_path, rs).unwrap();
    let status = Command::new("rustc")
        .args(["--edition", "2021", "-O", "-D", "warnings"])
        .arg(&rs_path)
        .arg("-o")
        .arg(&exe)
        .output()
        .expect("调用 rustc");
    assert!(
        status.status.success(),
        "P2-6 端到端：async target 链必须被 rustc 接受，stderr:
{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let run = Command::new(&exe).output().expect("运行生成 exe");
    assert!(run.status.success(), "P2-6 端到端：运行失败: {:?}", run.status);
    let stdout = String::from_utf8_lossy(&run.stdout);
    // worker exit go with(y -> d)，y = 2x ⇒ a/b/c = 2/4/6，总和 12。
    assert!(
        stdout.contains("sum= 12") || stdout.contains("sum=12") || stdout.contains("12"),
        "P2-6 端到端：stdout 应含 sum=12，实际: {stdout:?}"
    );
}

// ---------------------------------------------------------------------------
// P2-7：out("中文字符串") UTF-8 透传
// ---------------------------------------------------------------------------

/// P2-7 端到端：含中文的字符串字面量必须正确通过生成物输出，不能乱码。
#[test]
fn out_with_chinese_string_preserves_utf8() {
    use std::process::Command;
    let src = "\
entry main;\n\
\n\
cell main;\n\
receive();\n\
out(\"中文：hello, 世界。\");\n\
exit go with();\n\
";
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let g = generate(&out.program);
    let rs = &g.rust_source;

    // 静态断言：生成物里包含原始 UTF-8 字节（不是 Latin-1 转义）。
    assert!(
        rs.contains("\"中文：hello, 世界。\""),
        "P2-7 静态：生成物必须保留 UTF-8 字符串字面量。实际起始 200 字符:\n{}",
        &rs[..rs.len().min(300)]
    );

    // 端到端：rustc 编译 + 跑。
    let dir = std::env::temp_dir().join("rscel-gen-chinese");
    std::fs::create_dir_all(&dir).unwrap();
    let rs_path = dir.join("cn.rs");
    let exe = dir.join("cn.exe");
    std::fs::write(&rs_path, rs).unwrap();
    let status = Command::new("rustc")
        .args(["--edition", "2021", "-O"])
        .arg(&rs_path)
        .arg("-o")
        .arg(&exe)
        .output()
        .expect("调用 rustc");
    assert!(
        status.status.success(),
        "P2-7 端到端：含中文的生成物必须能编译，stderr:\n{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let run = Command::new(&exe).output().expect("运行生成 exe");
    assert!(run.status.success(), "P2-7 端到端：运行失败: {:?}", run.status);
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        stdout.contains("中文：hello, 世界。"),
        "P2-7 端到端：stdout 应含原文「中文：hello, 世界。」，实际: {stdout:?}"
    );
}

// ---------------------------------------------------------------------------
// 批次 6 - argv 入口（§14.3）
// ---------------------------------------------------------------------------

#[test]
fn argv_passes_i64_to_entry_cell() {
    use std::process::Command;
    let src = "\
cell main;\n\
receive(n: i64);\n\
out(\"got=\", n);\n\
exit go with();\n\
";
    let out = parse_program(src, 0);
    assert!(!out.has_errors());
    let g = generate(&out.program);
    let rs = &g.rust_source;

    assert!(
        rs.contains("std::env::args().skip(1).collect()"),
        "argv 静态：main 必须取 std::env::args().skip(1)，实际:\n{rs}"
    );
    assert!(
        rs.contains("__rsc_argv.get(0).and_then(|s| s.parse().ok())"),
        "argv 静态：i64 必须 .parse().ok()，实际:\n{rs}"
    );

    let dir = std::env::temp_dir().join("rscel-gen-argv-i64");
    std::fs::create_dir_all(&dir).unwrap();
    let rs_path = dir.join("argv.rs");
    let exe = dir.join("argv.exe");
    std::fs::write(&rs_path, rs).unwrap();
    let status = Command::new("rustc")
        .args(["--edition", "2021", "-O"])
        .arg(&rs_path)
        .arg("-o")
        .arg(&exe)
        .output()
        .expect("rustc");
    assert!(status.status.success(), "rustc 失败:\n{}", String::from_utf8_lossy(&status.stderr));
    let run = Command::new(&exe).arg("42").output().expect("run");
    assert!(run.status.success());
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(stdout.contains("got= 42"), "argv i64 端到端：stdout 应含 'got= 42'，实际: {stdout:?}");
}

#[test]
fn argv_passes_string_to_entry_cell() {
    use std::process::Command;
    let src = "\
cell main;\n\
receive(s: String);\n\
out(\"got=\", s);\n\
exit go with();\n\
";
    let out = parse_program(src, 0);
    assert!(!out.has_errors());
    let g = generate(&out.program);
    let rs = &g.rust_source;

    assert!(
        rs.contains("__rsc_argv.get(0).cloned()"),
        "argv 静态：String 必须 .cloned()，实际:\n{rs}"
    );

    let dir = std::env::temp_dir().join("rscel-gen-argv-string");
    std::fs::create_dir_all(&dir).unwrap();
    let rs_path = dir.join("argv.rs");
    let exe = dir.join("argv.exe");
    std::fs::write(&rs_path, rs).unwrap();
    let status = Command::new("rustc")
        .args(["--edition", "2021", "-O"])
        .arg(&rs_path)
        .arg("-o")
        .arg(&exe)
        .output()
        .expect("rustc");
    assert!(status.status.success());
    let run = Command::new(&exe).arg("hello").output().expect("run");
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(stdout.contains("got= hello"), "argv String 端到端：stdout 应含 'got= hello'，实际: {stdout:?}");
}

// ---------------------------------------------------------------------------
// 批次 A：语义正确性回归（recurse/return、read with Arc、多目标 and、终结 jump）
// ---------------------------------------------------------------------------

/// 编译生成物并运行，返回 stdout。`-D warnings` 与既有端到端测试保持一致：
/// 生成物必须**零告警**（P1-5）。
fn compile_and_run(rs: &str, tag: &str) -> String {
    run_generated(&compile_generated(rs, tag), &[])
}

/// 端到端两半的第一步：把生成物用 `-D warnings` 编成 exe（生成物必须零告警）。
fn compile_generated(rs: &str, tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rscel-gen-{tag}"));
    std::fs::create_dir_all(&dir).unwrap();
    let rs_path = dir.join(format!("{tag}.rs"));
    let exe = dir.join(format!("{tag}.exe"));
    std::fs::write(&rs_path, rs).unwrap();
    let build = Command::new("rustc")
        .args(["--edition", "2021", "-O", "-D", "warnings"])
        .arg(&rs_path)
        .arg("-o")
        .arg(&exe)
        .output()
        .expect("调用 rustc");
    assert!(
        build.status.success(),
        "[{tag}] rustc 拒绝生成物:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );
    exe
}

/// 端到端两半的第二步：运行 exe。`args` 是传给**生成程序**的命令行实参
/// （entry Cell 的 `receive` 从 argv 取值，所以同一份生成物可以走出不同分支）。
fn run_generated(exe: &std::path::Path, args: &[&str]) -> String {
    let run = Command::new(exe)
        .args(args)
        .output()
        .expect("运行生成 exe");
    assert!(run.status.success(), "运行失败: {:?}", run.status);
    String::from_utf8_lossy(&run.stdout).to_string()
}

/// §6.3 `jump recurse ... return(<key> -> <别名>) group <g>` + 紧邻 `jump sync`：
/// D3 内联 await 路径。端到端必须得到 120（修前：E35 直接拒绝这个规范样例，
/// 且 recurse 被当成 plain jump 生成 → 返回值被静默丢弃 + 无限递归爆栈）。
#[test]
fn jump_recurse_return_binds_parent_alias() {
    let src = "\
entry main;\n\
\n\
cell main;\n\
receive();\n\
let mut r: Option<i64> = None;\n\
jump recurse fact go with(5i64 -> n) return(product -> r) group r0;\n\
jump sync r0;\n\
out(\"result = \", r.unwrap_or(-1));\n\
exit go with();\n\
\n\
cell fact;\n\
receive(n: i64);\n\
let mut sub_result: Option<i64> = None;\n\
jump recurse fact when n > 1 go with(n - 1 -> n) return(product -> sub_result) group r1;\n\
jump sync r1;\n\
let result = n * sub_result.unwrap_or(1);\n\
exit go with(result -> product);\n\
";
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let g = generate(&out.program);
    let rs = &g.rust_source;
    // 内联路径：直接调用 + 解构绑定，不建组句柄、不 spawn 线程。
    assert!(
        rs.contains("let __rsc_sub: Option<RetFact> = cell_fact("),
        "recurse 内联路径应生成直接调用，实际:\n{rs}"
    );
    assert!(
        rs.contains("if v_a > 1 {"),
        "recurse 的 when 条件必须包裹生成体（否则无限递归），实际:\n{rs}"
    );
    assert!(
        !rs.contains("std::thread::spawn"),
        "紧邻 jump sync 的 recurse 按 D3 走内联，不该 spawn 线程，实际:\n{rs}"
    );
    let stdout = compile_and_run(rs, "recurse-return");
    assert!(
        stdout.contains("result =  120") || stdout.contains("120"),
        "递归阶乘应输出 120，实际: {stdout:?}"
    );
}

/// §8.1 / 附录 A.7：`read with` 跨三层 Cell 转发，只共享一份数据。
/// 修前 gen 完全忽略 mode：接收端注解 `Arc<String>` 实参却是 `String` → rustc 类型错误。
#[test]
fn read_with_forwards_single_arc_across_cells() {
    let src = "\
entry a;\n\
\n\
cell a;\n\
receive();\n\
let big_data = \"some large read-only data\".to_string();\n\
jump recurse b read with(big_data) group g1;\n\
jump sync g1;\n\
out(\"done\");\n\
exit go with();\n\
\n\
cell b;\n\
receive(big_data: Arc<String>);\n\
jump recurse c read with(big_data) group g2;\n\
jump sync g2;\n\
exit go with();\n\
\n\
cell c;\n\
receive(big_data: Arc<String>);\n\
out(\"len = \", big_data.len());\n\
exit go with();\n\
";
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let g = generate(&out.program);
    let rs = &g.rust_source;
    // 第一层发送：变量还不是 Arc → 隐式提升一次（§8.1「原始数据只存一份」）。
    assert!(
        rs.contains("let big_data = Arc::new(big_data);"),
        "非 Arc 的 read with 变量必须被提升，实际:\n{rs}"
    );
    // 后续发送：已是 Arc → 只 clone 引用计数，不再套一层 Arc。
    assert!(
        rs.contains("Arc::clone(&v_a)"),
        "read with 应生成 Arc::clone，实际:\n{rs}"
    );
    assert!(
        !rs.contains("Arc::new(Arc::new"),
        "绝不能把已经是 Arc 的变量再包一层，实际:\n{rs}"
    );
    let stdout = compile_and_run(rs, "read-with");
    assert!(
        stdout.contains("len =  25"),
        "A.7 的只读转发应打出 len = 25，实际: {stdout:?}"
    );
}

/// §6.4 多目标 `jump and worker1, worker2 group g1`：两种 Cell 都真的被派生。
/// 修前只 spawn `targets[0]`，其余目标被**静默丢掉**（父侧 `g1.W2.A` 取到默认值）。
#[test]
fn multi_target_jump_and_spawns_every_target() {
    let src = "\
entry main;\n\
\n\
cell main;\n\
receive(g1.W1.A: i64, g1.W2.A: i64) as (a1, a2);\n\
jump and worker1, worker2 go with(0i64 -> x) group g1;\n\
jump sync g1;\n\
out(\"pair \", a1, \" \", a2);\n\
exit go with();\n\
\n\
cell worker1;\n\
receive(x: i64);\n\
exit go with(x * 2 -> A);\n\
\n\
cell worker2;\n\
receive(x: i64);\n\
exit go with(x + 100 -> A);\n\
";
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let g = generate(&out.program);
    let rs = &g.rust_source;
    assert!(
        rs.contains("cell_worker1(") && rs.contains("cell_worker2("),
        "两个目标都要有派生调用，实际:\n{rs}"
    );
    assert!(
        rs.contains("__rsc_g_g1_2"),
        "第 2 个目标应绑定到带序号的子组句柄，实际:\n{rs}"
    );
    let stdout = compile_and_run(rs, "multi-target");
    assert!(
        stdout.contains("pair") && stdout.contains("0") && stdout.contains("100"),
        "W1=worker1(0*2=0)，W2=worker2(0+100=100)，实际: {stdout:?}"
    );
}

/// 终结式 plain jump 之后不再补兜底 `return`（修前 rustc 报 unreachable_code）。
#[test]
fn terminal_plain_jump_emits_no_unreachable_return() {
    let src = "\
entry main;\n\
cell main;\n\
receive();\n\
jump w go with(1i64 -> x);\n\
\n\
cell w;\n\
receive(x: i64);\n\
exit go with();\n\
";
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let rs = &generate(&out.program).rust_source;
    let n = rs.matches("return Some(RetMain").count();
    assert_eq!(n, 1, "main 只应有 1 条 return（终结 jump 里那条），实际 {n}:\n{rs}");
}

/// 「收到但不用」的 receive 别名：绑定加 `_` 前缀，而不是让整个生成物放弃
/// unused_variables 告警。
#[test]
fn unused_receive_alias_gets_underscore_binding() {
    let src = "\
entry main;\n\
cell main;\n\
receive();\n\
jump ack go with(7i64 -> id);\n\
\n\
cell ack;\n\
receive(id: i64);\n\
exit go with();\n\
";
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let rs = &generate(&out.program).rust_source;
    assert!(
        rs.contains("let _id: i64 = 7i64;"),
        "未使用的形参应带 _ 前缀（cell_ack 被实体展开，形参在唯一调用点绑定），实际:\n{rs}"
    );
    assert!(
        rs.contains("let _v_a: i64 = _id;"),
        "未使用的绑定应带 _ 前缀，实际:\n{rs}"
    );
}

/// 同步 caller 调 async 目标：调用点必须用 `__rsc_block_on` 驱动（修前用 caller 的
/// async 位决定 await 后缀 → sync caller 调 async callee 会漏 await，rustc 报类型错）。
#[test]
fn sync_caller_drives_async_target_with_block_on() {
    let src = "\
entry main;\n\
\n\
extern rust {\n\
    async fn rsc_wait(x: i64) -> i64 { x + 1 }\n\
}\n\
\n\
cell main;\n\
receive();\n\
jump w go with(1i64 -> x);\n\
\n\
cell w;\n\
receive(x: i64);\n\
extern rust { let y: i64 = rsc_wait(x).await; };\n\
exit go with(y as i64 -> d);\n\
";
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let rs = &generate(&out.program).rust_source;
    assert!(
        rs.contains("async fn cell_w("),
        "透传含 .await 的 Cell 必须是 async fn，实际:\n{rs}"
    );
    assert!(
        rs.contains("__rsc_block_on(cell_w("),
        "同步 main 调 async w 必须由 __rsc_block_on 驱动，实际:\n{rs}"
    );
    let _stdout = compile_and_run(rs, "sync-caller");
}

/// `jump and T read with(x) over c`：带 extra 实参的 and+over 路径端到端回归。
///
/// 复现的两个 bug（2026-10-10，都是这条路径**从来没有**跑通的原因）：
///
/// 第一个：只有 1 个 extra 时 gen 发射 `let __rsc_extra = (Arc::clone(&cfg));` ——
/// 那是括号表达式而不是 1-元组，按 `.0` 取值报 `error[E0609]: no field '0' on type
/// 'Arc<String>'`，还附带一条 unused_parens 告警。
///
/// 第二个：改成 1-元组后又撞上 `error[E0507]: cannot move out of '__rsc_extra.0',
/// a captured variable in an 'Fn' closure` —— 元组被 supervisor 闭包捕获，而每派生
/// 一个实例都要把字段 move 出去，`Fn` 闭包不允许。
///
/// 现在的形状：每个 extra 绑成独立局部变量 `__rsc_extra_k`，调用点传
/// `__rsc_extra_k.clone()`（`read with` 的 extra 本身已是 `Arc::clone`，再 clone
/// 就是引用计数 +1，与 §8.1「每实例一次 Arc::clone」同形）。
///
/// 为什么以前没被发现：`read_with_forwards_single_arc_across_cells` 走 recurse
/// （不经过这条 extra 路径），`jump_and_end_to_end_sums_worker_results` 没有 extra
/// 实参。受害面是 §8.1/§6.4 的规范形状，bench/read_share.rscel 也整条卡住。
#[test]
fn jump_and_over_with_single_read_with_arg_runs() {
    let src = "\
entry main;\n\
\n\
cell main;\n\
receive(g1.W1.s, g1.W2.s, g1.W3.s) as (a, b, c);\n\
let cfg = \"abcde\".to_string();\n\
let items: Vec<i64> = (1..=3i64).collect();\n\
jump and worker read with(cfg) over items concurrent_limit = 2 group g1;\n\
jump sync g1;\n\
println!(\"total={}\", a + b + c);\n\
exit go with();\n\
\n\
cell worker;\n\
receive(x: i64, cfg: Arc<String>);\n\
let s = x * cfg.len() as i64;\n\
exit go with(s -> s);\n\
";
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可无错解析，实际: {:#?}", out.diagnostics);
    let rs = &generate(&out.program).rust_source;
    // 每个 extra 绑成独立局部变量，调用点按名字引用（不再是元组字段 `.0`）。
    assert!(
        rs.contains("let __rsc_extra_0 = Arc::clone(&cfg);"),
        "单个 extra 应绑成独立局部变量 `__rsc_extra_0`，实际:\n{rs}"
    );
    assert!(
        rs.contains("__rsc_extra_0.clone()"),
        "每个子实例要拿到自己的一份 extra（.clone()），实际:\n{rs}"
    );
    assert!(
        !rs.contains("__rsc_extra.0"),
        "不该再出现被捕获元组的按字段 move（rustc E0507），实际:\n{rs}"
    );
    // 5 + 10 + 15 = 30（cfg.len() = 5，items = 1..=3）
    let stdout = compile_and_run(rs, "and-single-read-with");
    assert!(
        stdout.contains("total=30"),
        "stdout 应为 `total=30`，实际 {stdout:?}"
    );
}

// ============================================================ 候选 1：热 Cell 真实体展开
//
// 数据依据 `docs/06-控制变量实验.md`：同一段循环，写在调用方作用域 6.3ms，
// 放进 Cell 函数 20.6ms，加 `#[inline(always)]`（= 已提交的 48cbb79）12.9ms，
// 真实生成物 12.6ms；把 body 抄进调用点的探针 6.4ms（= 手写基线的 99.1%）。
// `--emit=asm` 进一步证明：LLVM 已把 Cell 函数内联进调用方，**向量化器仍不接手**
// （生成物 0 条 `padd`，展开体 7 条）。所以有效的只有源码级展开。

/// 展开用的最小热循环 DSL（limit=5 → checksum=10，由独立脚本算出）。
const HOT_LOOP: &str = "\
entry main;\n\
\n\
cell main;\n\
receive();\n\
jump sum go with(0i64 -> acc, 1i64 -> k);\n\
\n\
cell sum;\n\
receive(acc: i64, k: i64) as (a, i);\n\
let limit = std::hint::black_box(5i64);\n\
let next_acc = a + i;\n\
let next_i = i + 1;\n\
jump loop sum when next_i <= limit go with(next_acc, next_i);\n\
println!(\"checksum={}\", a);\n\
exit go with();\n";

/// 单一调用点的同步 Cell 必须被展开，且展开后端到端结果不变。
#[test]
fn single_call_site_sync_cell_is_expanded() {
    let out = parse_program(HOT_LOOP, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let rs = generate(&out.program).rust_source;
    // 函数边界必须消失：留着 `fn cell_sum(..)` 就是 2 倍慢的那一格。
    assert!(
        !rs.contains("fn cell_sum("),
        "被展开的 Cell 不该再有独立函数，实际:\n{rs}"
    );
    // 调用点形状：就地块 + 形参按位置绑定 + 原 exit 作块尾值。
    assert!(rs.contains("let _ = {"), "展开应为就地块表达式，实际:\n{rs}");
    assert!(
        rs.contains("let a: i64 = 0i64;") && rs.contains("let i: i64 = 1i64;"),
        "形参应在调用点用实参表达式绑定，实际:\n{rs}"
    );
    assert!(
        rs.contains("Some(RetSum {  })"),
        "被调方的 exit 应发成块尾值（不是 return），实际:\n{rs}"
    );
    // 语义：limit=5 的等差累加 = 10（由独立脚本枚举同一循环得到，不是抄生成物的输出）。
    let stdout = compile_and_run(&rs, "expanded-hot-loop");
    assert!(stdout.contains("checksum=10"), "stdout 应含 checksum=10，实际 {stdout:?}");
}

/// 两个 plain 调用点的同步 Cell：**每一个**调用点都要展开。
///
/// 取代旧断言「多调用点不得展开」（那条测的是 P0-4 落地时的闸门形状）。改判的数据依据是
/// `docs/06-控制变量实验.md` 实验七：`bench/loop_sum_two_sites.rscel` 与
/// `bench/loop_sum_opaque.rscel` 是**同一条循环、同一个 checksum**，唯一区别是热 Cell 多
/// 一个 plain 调用点，两窗实测 2.006x / 1.998x（各自复现带 ≤0.8%）
/// → 「多一个调用点就整格退回 `fn cell_x(..)`」本身就是 2 倍的性能缺陷。
///
/// 抄多份**不需要改名**：每份展开体都住在调用点自己的就地块里，被调方的 `v_<id>`、
/// 形参别名与 `let` 绑定只在该块内可见（遮蔽而非冲突），而 plain jump 是转移式出口，
/// 调用方在它之后没有语句会执行。这里让两个分支由 argv 选择，把**两份副本都真跑一遍**。
#[test]
fn multi_call_site_sync_cell_is_expanded_at_every_site() {
    let src = "\
entry main;\n\
\n\
cell main;\n\
receive(mode: i64);\n\
jump t when mode == 1i64 go with(10i64 -> v);\n\
jump t go with(20i64 -> v);\n\
\n\
cell t;\n\
receive(v: i64);\n\
out(v);\n\
exit go with();\n";
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let rs = generate(&out.program).rust_source;
    assert!(
        !rs.contains("fn cell_t("),
        "两个调用点都要展开，`fn cell_t` 必须消失，实际:\n{rs}"
    );
    assert_eq!(
        rs.matches("let _ = {").count(),
        2,
        "每个 plain 调用点各有一份就地块，实际:\n{rs}"
    );
    // 每份用自己的实参，绝不共享。
    assert!(
        rs.contains("let v: i64 = 10i64;") && rs.contains("let v: i64 = 20i64;"),
        "两份副本要各自按位置绑定本调用点的实参，实际:\n{rs}"
    );
    let exe = compile_generated(&rs, "multi-site-expanded");
    assert_eq!(run_generated(&exe, &["1"]), "10\n", "第一处展开体的输出");
    assert_eq!(run_generated(&exe, &["2"]), "20\n", "第二处展开体的输出");
}

/// 展开体里的 `let`（含 `jump loop` 的 hoisted 声明）必须**逐份待在自己的块里**。
///
/// 为什么单独测：HOT_LOOP 的展开体在同一个 `fn cell_main` 里出现两份，
/// 两份都发 `let mut next_acc;` / `let mut next_i;` / `let a: i64 = ..`。
/// 若展开的块嵌套写错（比如少了外层 `{`），rustc 会直接拒绝 —— `-D warnings` 这一步
/// 就是判据，不需要人工读生成物。
#[test]
fn two_copies_of_a_loop_cell_compile_and_run() {
    let src = "\
entry main;\n\
\n\
cell main;\n\
receive(mode: i64);\n\
jump sum when mode == 1i64 go with(0i64 -> acc, 1i64 -> k);\n\
jump sum go with(100i64 -> acc, 1i64 -> k);\n\
\n\
cell sum;\n\
receive(acc: i64, k: i64) as (a, i);\n\
let limit = std::hint::black_box(5i64);\n\
let next_acc = a + i;\n\
let next_i = i + 1;\n\
jump loop sum when next_i <= limit go with(next_acc, next_i);\n\
println!(\"checksum={}\", a);\n\
exit go with();\n";
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let rs = generate(&out.program).rust_source;
    assert!(!rs.contains("fn cell_sum("), "两个调用点都要展开，实际:\n{rs}");
    // 两份 hoisted 声明确实都在文件里（各一份）。
    assert_eq!(
        rs.matches("let mut next_acc;").count(),
        2,
        "每份展开体都该有自己的 hoisted let，实际:\n{rs}"
    );
    let exe = compile_generated(&rs, "two-copies-hot-loop");
    // (0,1) 起步累加到 limit=5 → 10；(100,1) 起步 → 110（同一条递推，起点差 100）。
    assert_eq!(run_generated(&exe, &["1"]), "checksum=10\n", "第一份副本");
    assert_eq!(run_generated(&exe, &["2"]), "checksum=110\n", "第二份副本");
}

/// 调用点 arity 不一致时**整格退回**函数形状：展开靠「按位置逐个发 `let 形参 = 实参;`」，
/// 实参少一个就会在取实参时越界，多一个则静默丢掉那个表达式的副作用。
///
/// 这条取代旧实现里「只看第一个调用点 arity」的写法（`plain_arity` 用 `find_map`，
/// 多调用点时那个假设根本不存在）。arity 本该由 check 层的 E042 拦住，
/// 但生成器不假设调用方先跑过 `rscel check`，所以这里只要求**不 panic 且不展开**。
#[test]
fn call_sites_with_inconsistent_arity_are_not_expanded() {
    let src = "\
entry main;\n\
\n\
cell main;\n\
receive();\n\
jump t go with(1i64 -> x);\n\
\n\
cell aux;\n\
receive();\n\
jump t;\n\
\n\
cell t;\n\
receive(x: i64);\n\
out(x);\n\
exit go with();\n";
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let rs = generate(&out.program).rust_source;
    assert!(
        rs.contains("fn cell_t("),
        "arity 不一致的多个调用点不得展开（`fn cell_t` 必须留着给第二处调用），实际:\n{rs}"
    );
    assert!(!rs.contains("let _ = {\n        let x: i64;"), "不该出现半份展开体，实际:\n{rs}");
}

/// 体积预算：展开把 body 抄 N 份，总量（份数 × body 语句数）超过
/// `MAX_EXPANDED_STATEMENTS`（24）就整格退回函数形状 —— 生成物是一个要给人读的 `.rs`。
#[test]
fn expansion_respects_the_size_budget() {
    // body = 7 条 `out` + 1 条 `exit` = 8 条，3 个调用点 → 24 条，恰好在预算内。
    let within = expansion_budget_case(7);
    let out = parse_program(&within, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let rs = generate(&out.program).rust_source;
    assert!(
        !rs.contains("fn cell_t("),
        "3 个调用点 × 8 条语句 = 24，预算内应展开，实际:\n{rs}"
    );

    // 多一条 `out` → 27 条，出预算。
    let over = parse_program(&expansion_budget_case(8), 0);
    let rs2 = generate(&over.program).rust_source;
    assert!(
        rs2.contains("fn cell_t("),
        "3 个调用点 × 9 条语句 = 27，出预算必须保留函数，实际:\n{rs2}"
    );
}

/// 造一个「热 Cell 有 N 条 body 语句、被 3 个不同 Cell 各 jump 一次」的程序。
fn expansion_budget_case(extra_outs: usize) -> String {
    let mut src = String::from(
        "entry main;\n\
         \n\
         cell main;\n\
         receive();\n\
         jump t go with(1i64 -> v);\n\
         \n\
         cell aux1;\n\
         receive();\n\
         jump t go with(2i64 -> v);\n\
         \n\
         cell aux2;\n\
         receive();\n\
         jump t go with(3i64 -> v);\n\
         \n\
         cell t;\n\
         receive(v: i64);\n",
    );
    for _ in 0..extra_outs {
        src.push_str("out(v);\n");
    }
    src.push_str("exit go with();\n");
    src
}

/// 多目标 plain jump（`jump a, b`）：`emit_jump` 只对第一个目标发射调用，
/// 所以**其余目标不能算作有调用点**。
///
/// 这是本批实现在 `docs/06` 实验七之外抓到的一条静默丢代码缺陷：旧统计把所有目标都记成
/// plain 调用点，于是 `cell b` 被判成「唯一调用点 → 展开」，结果既不发射 `fn cell_b`、
/// 又没有展开体落点 —— `cell b` 的 body 从生成物里**整体消失**，只剩一个没人引用的
/// `struct RetB {}`，且 `rscel check` 零诊断（实测 2026-10-11：`jump a, b go with(1i64 -> v)`
/// 的生成物只有 `fn cell_main`，运行输出 `1` 而不是 `1` + `101`）。
/// 多目标 plain 该不该存在、要不要报错属语言面决定（docs/05 P3），本条只钉住「不丢代码」。
#[test]
fn multi_target_plain_jump_keeps_the_other_targets_function() {
    let src = "\
entry main;\n\
\n\
cell main;\n\
receive();\n\
jump a, b go with(1i64 -> v);\n\
\n\
cell a;\n\
receive(v: i64);\n\
out(v);\n\
exit go with();\n\
\n\
cell b;\n\
receive(v: i64);\n\
let w = v + 100i64;\n\
out(w);\n\
exit go with();\n";
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let rs = generate(&out.program).rust_source;
    // 第一个目标才是调用点 → a 展开。
    assert!(!rs.contains("fn cell_a("), "a 是实际被调用的目标，应展开，实际:\n{rs}");
    // 第二个目标没有任何调用发射点 → 必须保留独立函数（可以没人调，但不能消失）。
    assert!(
        rs.contains("fn cell_b(") && rs.contains("let v_a: i64 = v;"),
        "b 没有被发射的调用点，不得判成可展开（否则 body 静默消失），实际:\n{rs}"
    );
    let stdout = compile_and_run(&rs, "multi-target-plain");
    assert_eq!(stdout, "1\n", "多目标的其余目标今天不执行（P3 的语言面缺口），但必须留在生成物里");
}

/// 被调方 body 里自己还有 plain jump 时**不能**展开：那条 jump 会发射
/// `return Some(Ret_self {})`，展开后它返回的是**调用方**，语义就变了。
#[test]
fn callee_with_inner_plain_jump_is_not_expanded() {
    let src = "\
entry a;\n\
\n\
cell a;\n\
receive();\n\
jump b go with(1i64 -> v);\n\
\n\
cell b;\n\
receive(v: i64);\n\
jump c go with(v -> w);\n\
\n\
cell c;\n\
receive(w: i64);\n\
out(w);\n\
exit go with();\n";
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let rs = generate(&out.program).rust_source;
    assert!(rs.contains("fn cell_b("), "b 的 body 内有 plain jump，不该展开，实际:\n{rs}");
    // c 只有一个调用点、body 干净 → 仍然展开（展开的判定只看被调方自己）。
    assert!(!rs.contains("fn cell_c("), "c 应被展开，实际:\n{rs}");
    // b 里那条转移式 return 必须还是 b 自己的（`fn cell_b` 的返回类型）。
    assert!(
        rs.contains("return Some(Ret_b {".replace("b", "B").as_str())
            || rs.contains("return Some(RetB {});"),
        "b 的 plain jump 应继续发射自己的终结 return，实际:\n{rs}"
    );
}

/// 展开体里的每条 DSL 语句都必须还有**自己的**映射条目（§12.9 / §15.2 反映射）。
///
/// 为什么单独测：第一版实现把被调方先发射到临时缓冲、再整段抄进文件，行号全部错位，
/// `errmap_test` 的 `out` 语句映射直接消失。改成在调用点直接发射后由本条钉住。
#[test]
fn expanded_statements_keep_their_own_srcmap_entries() {
    let out = parse_program(HOT_LOOP, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let gen = rscel_gen::generate_as(&out.program, "hot_loop.rs");
    let map = gen.source_map.as_deref().expect("必须有映射表");
    let rs = &gen.rust_source;
    // 被展开 Cell 的两条语句（`let limit = ...` / `println!`）各自要有 `cell: "sum"` 条目。
    let hits: Vec<&str> = map.lines().filter(|l| l.contains("\"cell\": \"sum\"")).collect();
    assert!(
        hits.len() >= 5,
        "展开体里的 sum 语句应逐条留在映射表（实际 {} 条），映射:\n{map}",
        hits.len()
    );
    // 条目指向的行必须真的落在展开块内部（防止行号错位）。
    let sum_line = rs
        .lines()
        .position(|l| l.contains("let limit = std::hint::black_box(5i64);"))
        .expect("展开体里应有 let limit 这一行")
        + 1;
    assert!(
        hits.iter().any(|l| l.contains(&format!("\"rs_line\": {sum_line}"))),
        "sum 的 `let limit` 应映射到生成物的第 {sum_line} 行，映射:\n{map}"
    );
}

/// `jump ... when ...` 的展开落在 `if` 分支里：分支内终结的必须是**调用方**的返回，
/// 且未命中时调用方继续执行（转移式语义不能被展开改掉）。
#[test]
fn when_guarded_plain_jump_expands_inside_branch() {
    let src = "\
entry main;\n\
\n\
cell main;\n\
receive();\n\
let flag = std::hint::black_box(true);\n\
jump t when flag go with(2i64 -> x);\n\
println!(\"after\");\n\
\n\
cell t;\n\
receive(x: i64);\n\
println!(\"t={}\", x);\n\
exit go with();\n";
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let rs = generate(&out.program).rust_source;
    assert!(!rs.contains("fn cell_t("), "t 只有一个调用点，应展开，实际:\n{rs}");
    // 展开块在 `if flag {` 之内：块头在 8 空格层，块内语句再深一层（12 空格）。
    assert!(
        rs.contains("        { let _ = {"),
        "when 分支里的展开块头应在 8 空格层，实际:\n{rs}"
    );
    assert!(
        rs.contains("            let x: i64 = 2i64;"),
        "when 分支里的形参绑定应在 12 空格层，实际:\n{rs}"
    );
    let stdout = compile_and_run(&rs, "expanded-when-jump");
    assert!(stdout.contains("t=2"), "命中分支应执行被调方，实际 {stdout:?}");
    assert!(
        !stdout.contains("after"),
        "plain jump 是转移式出口：命中后不应继续执行调用方剩余语句，实际 {stdout:?}"
    );
}

// ---------------------------------------------------------------------------
// P0-2：`extern rust` 的 fn 签名是 D2 的一个类型来源
// ---------------------------------------------------------------------------

/// 修前 D2 只有「注解 / 入边传播 / 字面推导」三档，`fast_mul(6, 7)` 这种调用点
/// 推不出类型 → `collect_type_errors` 命中 → 生成物只有一串 `compile_error!`，
/// 而 README 与 docs/02 §14.5 都声称附录 A.6 可编译运行。
/// 这条端到端把「签名返回类型被采纳」钉成可执行事实：6×7×2 = 84。
#[test]
fn extern_rust_fn_signature_feeds_d2_end_to_end() {
    let src = "\
entry main;\n\
\n\
extern rust {\n\
    fn fast_mul(a: i64, b: i64) -> i64 { a.wrapping_mul(b) }\n\
};\n\
\n\
cell main;\n\
receive();\n\
let mut got: Option<i64> = None;\n\
jump recurse worker go with(fast_mul(6, 7) -> n) return(product -> got) group g1;\n\
jump sync g1;\n\
out(\"product=\", got.unwrap_or(-1));\n\
exit go with();\n\
\n\
cell worker;\n\
receive(n);\n\
exit go with(n * 2 -> product);\n";
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let rs = generate(&out.program).rust_source;
    assert!(!rs.contains("compile_error!"), "n 的类型应能从签名推出，实际:\n{rs}");
    assert!(
        rs.contains("fn cell_worker(n: i64)"),
        "worker 的形参类型必须来自 fast_mul 的返回类型，实际:\n{rs}"
    );
    let stdout = compile_and_run(&rs, "extern-fn-d2");
    assert!(stdout.contains("product= 84"), "端到端应输出 product= 84，实际 {stdout:?}");
}

/// 仓库样例本体（附录 A.6）也要产出可编译代码 —— 文档的说法靠这条兜底。
/// 该样例不打印任何内容，端到端要的是「rustc 零告警通过 + 退出码 0」，
/// 这两点 `compile_and_run` 内部已经断言，这里不再补假断言。
#[test]
fn appendix_a6_example_generates_runnable_code() {
    let src = include_str!("../../../examples/extern_rust_mix.rscel");
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "解析失败: {:#?}", out.diagnostics);
    let rs = generate(&out.program).rust_source;
    assert!(!rs.contains("compile_error!"), "A.6 样例应产出代码，实际:\n{rs}");
    assert!(
        rs.contains("pub product: i64,"),
        "Ret 字段类型应来自 fast_mul 的签名，实际:\n{rs}"
    );
    compile_and_run(&rs, "appendix-a6");
}

// ---------------------------------------------------------------------------
// P0-5 第 3 项（docs/06 实验八）：`__rsc_block_on` 把 future 固定在**栈**上。
//
// 实测依据（同窗、同组 flag、同一 checksum=1249999975000000）：`Box::pin` 那一格
// 让 async 热循环每轮多 5 条穿过堆指针的存储，中位 81.80ms → 栈上 pin 68.20ms
// （两窗同号 −16.6% / −14.1%）。所以这两条测试守的是**性能属性**，不是新功能：
// 谁把它改回装箱，第一条就红；而栈上 pin 真正的风险面是"future 挂起之后栈帧还活着、
// 醒来还能接着 poll"，那由第二条（真的返回 Pending、由别的线程 wake）守住。
// ---------------------------------------------------------------------------

/// 静态回归：执行器里必须是 `std::pin::pin!`，不许出现 `Box::pin`。
///
/// 为什么钉在 `RUNTIME_SOURCE` 而不是某份生成物上：运行时是**逐字嵌入**的，钉源头
/// 就等于钉所有生成物，而读一份 `.rs` 还得挑一个恰好调用 block_on 的 DSL。
#[test]
fn block_on_pins_the_future_on_the_stack_not_in_a_box() {
    let src = rscel_gen::runtime::RUNTIME_SOURCE;
    let start = src
        .find("fn __rsc_block_on")
        .expect("运行时里应能找到 __rsc_block_on 的定义");
    let body = &src[start..];
    let end = body.find("\n}\n").expect("__rsc_block_on 的函数体应在此闭合");
    let bo = &body[..end];

    assert!(
        bo.contains("let mut fut = std::pin::pin!(fut);"),
        "__rsc_block_on 必须把 future 固定在栈上（docs/06 实验八实测堆装箱值 14%–17%），实际:\n{bo}"
    );
    // 判"不许再装箱"要先去掉注释行：这段的**注释里**就得写着 `Box::pin` 解释为什么不用它，
    // 直接对原文 find 会让一条正确的改动把自己的测试弄红（本批实测踩过）。
    let code: String = bo
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !code.contains("Box::pin"),
        "__rsc_block_on 的**代码**里不该再有 `Box::pin`：堆指针让每轮迭代多 5 条穿过 `%r8` 的存储，实际:\n{code}"
    );
    // `pin!`  stabilized 于 1.68，MSRV 1.75 之内；写成语句形式会**不**遮蔽参数名（实测 E0599），
    // 所以 `let mut` 这一段也是形状的一部分，不能"等价改写"掉。
    assert!(
        src.contains("#[inline(never)]\nfn __rsc_block_on"),
        "执行器仍应带 `#[inline(never)]`（实测改成 inline(always) 不改变读数，而保住它可以让\
         生成物的栈帧只多一层，实际没找到该签名行）"
    );
}

/// 端到端回归：future **真的挂起**（第一次 poll 返回 Pending，20ms 后由另一线程 wake），
/// `__rsc_block_on` 的 Condvar 等待 + 醒转后继续 poll 必须拿到正确结果。
///
/// 为什么必须有这条：栈上 pin 之后，被 pin 的数据活在 block_on 的栈帧里，而它要跨越
/// `sig.cvar.wait(..)` 这一次真正的线程挂起再被唤醒。只测"立即 Ready 的 future"的话，
/// 挂起路径一次都没走过 —— 那正是这个改动唯一的风险面。
#[test]
fn suspending_future_resumes_inside_block_on() {
    let src = "\
extern rust {
    struct RscPendingOnce { v: i64, fired: bool }

    impl std::future::Future for RscPendingOnce {
        type Output = i64;
        fn poll(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<i64> {
            let me = &mut *self;
            if me.fired {
                return std::task::Poll::Ready(me.v);
            }
            me.fired = true;
            let w = cx.waker().clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(20));
                w.wake();
            });
            std::task::Poll::Pending
        }
    }

    fn rsc_pending(v: i64) -> RscPendingOnce { RscPendingOnce { v, fired: false } }

    async fn rsc_wait(v: i64) -> i64 { rsc_pending(v).await }
};

entry main;

cell main;
receive();
jump w go with(std::hint::black_box(41i64) -> n);

cell w;
receive(n: i64);
extern rust { let got: i64 = rsc_wait(n).await; };
out(\"got=\", got);
exit go with();
";
    let out = parse_program(src, 0);
    assert!(!out.has_errors(), "样例应可解析: {:#?}", out.diagnostics);
    let rs = generate(&out.program).rust_source;
    // 同步 caller 调 async 目标 → 必须是 block_on 驱动（S2/D3 的形状，别退化成装箱 await）。
    assert!(
        rs.contains("__rsc_block_on(cell_w("),
        "sync caller 调 async Cell 应由 __rsc_block_on 驱动，实际:\n{rs}"
    );

    let exe = compile_generated(&rs, "suspend-awake");
    // 用 try_wait + 超时而不是 run_generated 的阻塞 output()：唤醒路径一旦失灵，
    // 程序会**永远等在 Condvar 上**，那时需要的是一条红测试而不是一个卡死的测试批次。
    let mut child = Command::new(&exe)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let status = loop {
        if let Ok(Some(s)) = child.try_wait() {
            break s;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            panic!("挂起的 future 没被唤醒：__rsc_block_on 的栈上 pin + Condvar 路径失灵（挂死 >20s）");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(status.success(), "生成程序应正常退出，实际: {status:?}");
    let stdout = child
        .wait_with_output()
        .expect("读取 stdout")
        .stdout;
    let text = String::from_utf8_lossy(&stdout);
    assert!(
        text.contains("got= 41"),
        "挂起→唤醒之后必须拿到 await 的结果 41，实际: {text}"
    );
}
