//! rscel-cli 集成测试：check / build / parse 子命令。
//!
//! `build` 用例会真实调用 `rustc` 并**执行生成的可执行文件**，因此默认参与
//! `cargo test --workspace`（与 `rscel-gen` 的端到端测试一致，没有任何被跳过的用例）。

use rscel_cli::run;
use std::path::PathBuf;

fn write_tmp(name: &str, src: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("rscel-cli-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, src).unwrap();
    path
}

const VALID_SRC: &str = r#"
cell main;
receive();
jump a go with();

cell a;
receive();
exit go with();
"#;

/// 端到端样例：`out(...)` 打印出被跳转到下游 Cell 的实参值。
const OUT_SRC: &str = r#"
cell main;
receive();
jump a go with(6 -> x);

cell a;
receive(x);
out("rscel-out=", x);
exit go with();
"#;

#[test]
fn parse_help_exits_zero() {
    assert_eq!(run(&["rscel".to_string(), "--help".to_string()]), 0);
}

#[test]
fn parse_version_exits_zero() {
    assert_eq!(run(&["rscel".to_string(), "--version".to_string()]), 0);
}

#[test]
fn parse_unknown_subcommand_exits_2() {
    // 未知子命令 → 退出码 2（参数错误，归到 rustc/参数错，按 §4.1 退 2）
    // 实际 cli.rs 写的是返回 2
    assert_eq!(run(&["rscel".to_string(), "frobnicate".to_string()]), 2);
}

#[test]
fn check_missing_file_exits_3() {
    // 不存在的文件 → IO 错误，退出 3
    let code = run(&[
        "rscel".to_string(),
        "check".to_string(),
        std::env::temp_dir()
            .join("rscel-cli-tests")
            .join("does-not-exist.rscel")
            .to_string_lossy()
            .into_owned(),
    ]);
    assert_eq!(code, 3);
}

#[test]
fn check_valid_file_exits_zero() {
    let file = write_tmp("valid.rscel", VALID_SRC);
    let code = run(&[
        "rscel".to_string(),
        "check".to_string(),
        file.to_string_lossy().into_owned(),
    ]);
    assert_eq!(code, 0);
}

#[test]
fn build_missing_o_flag_exits_2() {
    let file = write_tmp("valid.rscel", VALID_SRC);
    let code = run(&[
        "rscel".to_string(),
        "build".to_string(),
        file.to_string_lossy().into_owned(),
    ]);
    assert_eq!(code, 2);
}

#[test]
fn build_end_to_end_outputs_executable() {
    let file = write_tmp("build_target.rscel", VALID_SRC);
    let out = std::env::temp_dir().join("rscel-cli-tests").join("build_target.exe");
    let code = run(&[
        "rscel".to_string(),
        "build".to_string(),
        file.to_string_lossy().into_owned(),
        "-o".to_string(),
        out.to_string_lossy().into_owned(),
        "--release".to_string(),
    ]);
    assert_eq!(code, 0, "build 应该返回 0");
    assert!(std::path::Path::new(&out).exists(), "应该生成可执行文件");
}

/// 全链路验收：DSL 源 → rscel build → rustc → 运行 → stdout 符合预期。
#[test]
fn build_then_run_prints_expected_output() {
    let file = write_tmp("build_out_target.rscel", OUT_SRC);
    let out = std::env::temp_dir()
        .join("rscel-cli-tests")
        .join("build_out_target.exe");
    let code = run(&[
        "rscel".to_string(),
        "build".to_string(),
        file.to_string_lossy().into_owned(),
        "-o".to_string(),
        out.to_string_lossy().into_owned(),
        "--release".to_string(),
    ]);
    assert_eq!(code, 0, "build 应该返回 0");

    let output = std::process::Command::new(&out)
        .output()
        .expect("应该能执行生成的可执行文件");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("rscel-out=") && stdout.contains('6'),
        "stdout 应含 `rscel-out=` 与实参 6，实际 {stdout:?}"
    );
}

/// `build --debug` 必须真的能编译（§16.1）。
///
/// 历史缺陷：debug 模式给 rustc 传的是 **C/C++ 的 `-O0`**，rustc 直接
/// `error: Unrecognized option: '0'` —— 也就是说 `--debug` 这条路径
/// 从来没有跑通过，而测试只覆盖了 `--release`。rustc 的正确写法是
/// `-C opt-level=0`，并且 debug 还要带 `-g`。
#[test]
fn build_debug_mode_compiles_and_runs() {
    let file = write_tmp("debug_target.rscel", OUT_SRC);
    let out = std::env::temp_dir()
        .join("rscel-cli-tests")
        .join("debug_target.exe");
    let code = run(&[
        "rscel".to_string(),
        "build".to_string(),
        file.to_string_lossy().into_owned(),
        "-o".to_string(),
        out.to_string_lossy().into_owned(),
        "--debug".to_string(),
    ]);
    assert_eq!(code, 0, "--debug 编译失败（退出码 {code}）");
    assert!(out.exists(), "--debug 应产出可执行文件");
    let run_out = std::process::Command::new(&out)
        .output()
        .expect("运行 --debug 产物");
    assert!(run_out.status.success(), "--debug 产物运行失败");
    let stdout = String::from_utf8_lossy(&run_out.stdout).to_string();
    assert!(stdout.contains("6"), "--debug 产物输出应含 6，实际: {stdout:?}");
}

/// §15.3：`--debug` 的产物把事件流打到 **stderr**，位置是 **DSL** 行号。
///
/// 这条是编辑器拓扑调试的数据通路：`rscel edit` 的 `/api/build` 会把这段 stderr 原样
/// 回传（见 `editor_test::debug_mode_build_endpoint_streams_trace_events`），
/// 前端拿 `@ 行:列` 就能高亮回 DSL 源。
///
/// 同时钉住两条只有真跑才能验的事：
/// - `out(...)` 的结果仍在 **stdout**，埋点一行都不许混进去；
/// - 带 `when` 的 jump **条件不成立时一行都不打**（否则追踪流会显示一次根本没发生的传输）。
#[test]
fn build_debug_mode_streams_trace_events_on_stderr() {
    let src = "entry start;\n\
               cell start;\n\
               receive(val: i64);\n\
               jump one when val == 1 go with(val -> n);\n\
               die;\n\
               \n\
               cell one;\n\
               receive(n: i64);\n\
               exit go with(n -> back);\n";
    let file = write_tmp("trace_debug.rscel", src);
    let out = std::env::temp_dir().join("rscel-cli-tests").join("trace_debug.exe");
    let code = run(&[
        "rscel".to_string(),
        "build".to_string(),
        file.to_string_lossy().into_owned(),
        "-o".to_string(),
        out.to_string_lossy().into_owned(),
        "--debug".to_string(),
    ]);
    assert_eq!(code, 0, "--debug 编译失败（退出码 {code}）");

    // 命中分支：spawn → jump（带这次 Packet 的键名）→ 被展开的 one 的 spawn/exit。
    let hit = std::process::Command::new(&out)
        .arg("1")
        .output()
        .expect("运行 --debug 产物");
    assert!(hit.status.success());
    let hit_out = String::from_utf8_lossy(&hit.stdout).to_string();
    let hit_err = String::from_utf8_lossy(&hit.stderr).to_string();
    assert!(
        !hit_out.contains("[rsc]"),
        "埋点必须走 stderr，stdout 不能被污染: {hit_out:?}"
    );
    assert!(
        hit_err.contains("[rsc] cell start spawn @ 2:1 recv=[val]"),
        "spawn 行要带 DSL 位置与入边键名，实际:\n{hit_err}"
    );
    assert!(
        hit_err.contains("[rsc] cell start jump -> one @ 4:1 send=[n]"),
        "plain jump 行要带目标与出边键名，实际:\n{hit_err}"
    );
    assert!(
        hit_err.contains("[rsc] cell one spawn @ 7:1 recv=[n]")
            && hit_err.contains("[rsc] cell one exit @ 9:1 send=[back]"),
        "`one` 只有一个 plain 调用点、已被实体展开，spawn/exit 仍要逐条出现:\n{hit_err}"
    );

    // 未命中分支：三条 jump 一行都不许出现，只留 die 那一行（位置 = DSL 第 5 行）。
    let miss = std::process::Command::new(&out)
        .arg("9")
        .output()
        .expect("运行 --debug 产物");
    let miss_err = String::from_utf8_lossy(&miss.stderr).to_string();
    assert!(
        miss_err.contains("[rsc] cell start die @ 5:1"),
        "die 行要给出 DSL 行号，实际:\n{miss_err}"
    );
    assert!(
        !miss_err.contains("jump ->"),
        "when 没命中的 jump 不该留下任何传输事件:\n{miss_err}"
    );
}

/// §15.3「release 模式全部删除」的端到端版：同一份 DSL 走 `--release`，
/// stderr 必须**空**，stdout 与 `--debug` 逐字相同。
#[test]
fn build_release_mode_streams_no_trace_events() {
    let src = "cell main;\n\
               receive();\n\
               jump a go with(6 -> x);\n\
               \n\
               cell a;\n\
               receive(x);\n\
               out(\"rscel-out=\", x);\n\
               exit go with();\n";
    let file = write_tmp("trace_release.rscel", src);
    let out = std::env::temp_dir().join("rscel-cli-tests").join("trace_release.exe");
    let code = run(&[
        "rscel".to_string(),
        "build".to_string(),
        file.to_string_lossy().into_owned(),
        "-o".to_string(),
        out.to_string_lossy().into_owned(),
        "--release".to_string(),
    ]);
    assert_eq!(code, 0, "--release 编译失败（退出码 {code}）");
    let run_out = std::process::Command::new(&out).output().expect("运行产物");
    let stdout = String::from_utf8_lossy(&run_out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&run_out.stderr).to_string();
    assert!(stdout.contains("rscel-out=") && stdout.contains('6'), "实际 {stdout:?}");
    assert!(stderr.is_empty(), "release 产物的 stderr 必须是空的，实际: {stderr:?}");
    // 生成物文本里也不该有埋点（release 走的是 `GenOptions::trace = false`）。
    let rs = std::fs::read_to_string(out.with_extension("rs")).expect("读回生成物");
    assert!(!rs.contains("[rsc] "), "release 的 .rs 里出现了埋点");
}

/// 前端错误必须让 `build` 以 1 退出，并把诊断打到 stderr（§4.1）。
#[test]
fn build_reports_frontend_error_with_exit_1() {
    let bad = r#"
cell main;
receive();
let mut r: Option<i64> = None;
jump w go with(1 -> x) return(v -> r);

cell w;
receive(x: i64);
exit go with(x -> v);
"#;
    let file = write_tmp("bad_e046.rscel", bad);
    let out = std::env::temp_dir()
        .join("rscel-cli-tests")
        .join("bad_e046.exe");
    let code = run(&[
        "rscel".to_string(),
        "build".to_string(),
        file.to_string_lossy().into_owned(),
        "-o".to_string(),
        out.to_string_lossy().into_owned(),
    ]);
    assert_eq!(code, 1, "plain jump 挂 return(...) 应被判前端错误（E046）");
}

/// 段序写错的用户路径验收（docs/05 P1-1）：`check` 只报点名那一段的 RSC-E033，
/// 不再给出误导性的 RSC-E042（"over 只能供给 1 个实参"，而用户明明写了 `read with`）；
/// `build` 必须**什么都不落盘** —— 非法 Rust 不该留给 rustc 去发现。
#[test]
fn misplaced_jump_segment_reports_e033_and_blocks_generation() {
    let src = "entry main;\n\ncell main;\nreceive();\nlet payload = \"x\".repeat(8);\n\
               let items: Vec<i64> = (1..=4i64).collect();\n\
               jump and consumer over items read with(payload) concurrent_limit = 2 group g1;\n\
               jump sync g1;\nexit go with();\n\n\
               cell consumer;\nreceive(x: i64, payload: Arc<String>);\nexit go with();\n";

    let diag = rscel_cli::api::check_json(src).to_compact();
    assert!(diag.contains("\"code\":33"), "应报 RSC-E033，实际: {diag}");
    assert!(!diag.contains("\"code\":42"), "不该再有误导的 RSC-E042，实际: {diag}");

    let file = write_tmp("misordered_jump.rscel", src);
    let out = std::env::temp_dir().join("rscel-cli-tests").join("misordered_jump.exe");
    let rs = out.with_extension("rs");
    let map = out.with_extension("map.json");
    for stale in [&out, &rs, &map] {
        let _ = std::fs::remove_file(stale);
    }
    let code = run(&[
        "rscel".to_string(),
        "build".to_string(),
        file.to_string_lossy().into_owned(),
        "-o".to_string(),
        out.to_string_lossy().into_owned(),
    ]);
    assert_eq!(code, 1, "段序写错应被前端拦下（退出码 1）");
    assert!(!rs.exists(), "E033 之后不得生成 .rs");
    assert!(!map.exists(), "E033 之后不得生成 .map.json");
    assert!(!out.exists(), "E033 之后不得生成可执行文件");
}
