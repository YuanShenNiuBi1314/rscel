//! # rscel-cli —— 命令行编排层（批次 3B + §16 前端工具链）
//!
//! 子命令：
//! - `rscel check <file.rscel>`：parse + check，输出诊断到 stderr，退出码 1 表示有错。
//! - `rscel build <file.rscel> -o <out>` [--release | --debug] [--no-cache]：
//!   parse + check + gen + rustc（§16.2 的增量缓存默认开，见 [`build_cache`]）。
//! - `rscel graph <file.rscel>` [--mermaid]：§16.4 的调用链路报告（复用 topology）。
//! - `rscel trace <file.rscel>` [--max-items N] [--arg v]…：§16.5 的干跑模拟。
//! - `rscel edit [file.rscel] [--port 8421]`：只绑 127.0.0.1 的本地编辑器后端。
//! - `rscel --version` / `--help`。
//!
//! 退出码（§4.1）：
//! - 0：成功
//! - 1：前端错误（check/build/graph/trace 均适用）
//! - 2：rustc 编译错误
//! - 3：参数/IO 错误（含 `edit` 端口被占用）
//!
//! 错误反映射（§15.2）：rustc 以 `--error-format=json` 调用并捕获 stderr，
//! `errmap` 查 `.map.json` 把生成文件的行号换回 DSL 位置（RSC-E900 / RSC-E901）。
//! 注意：`--message-format` 是 **cargo** 的 flag，rustc 认的是 `--error-format`
//! （见 `errmap` 模块头的实测记录）。
//!
//! 模块分工：
//! - [`cli`]：参数解析 + `Handlers` 装配；
//! - [`graph`] / [`trace_cli`]：§16.4 / §16.5 的两个新子命令；
//! - [`serve`]：HTTP 传输（绑定、请求解析、路由、每连接一线程）；
//! - [`api`]：端点载荷（JSON 数据形状，`rscel edit` 与 CLI 共用同一份判定）；
//! - [`json`]：手写 JSON 写出器 + 表单 percent-decode（零依赖，std 没有 JSON）。
//!
//! 整个 crate **零外部依赖**（依赖内部 crates）。

use rscel_check::check_program;
use rscel_gen::srcmap::SrcMap;
use rscel_gen::{generate_as_with, GenOptions};
use rscel_parser::parse_program;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub mod api;
pub mod build_cache;
pub mod cli;
pub mod diag_print;
pub mod errmap;
pub mod graph;
pub mod json;
pub mod serve;
pub mod trace_cli;

// ---------------------------------------------------------------- check

/// 跑 `rscel check`：parse + check，输出诊断到 stderr，返回退出码。
pub fn run_check(rscel_file: &Path) -> i32 {
    let src = match std::fs::read_to_string(rscel_file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("RSC: cannot read {}: {e}", rscel_file.display());
            return 3;
        }
    };
    let parsed = parse_program(&src, 0);
    if parsed.has_errors() {
        for d in &parsed.diagnostics {
            eprintln!("{}", d.render(&rscel_file.display().to_string()));
        }
        return 1;
    }
    let outcome = check_program(&parsed.program);
    if outcome.diagnostics.is_empty() {
        eprintln!("check OK: {} cells, 0 errors", parsed.program.cells().count());
        return 0;
    }
    let n_err = outcome.diagnostics.iter().filter(|d| d.severity == rscel_parser::Severity::Error).count();
    let n_warn = outcome.diagnostics.iter().filter(|d| d.severity == rscel_parser::Severity::Warning).count();
    for d in &outcome.diagnostics {
        let cell_name = d.cell.clone().unwrap_or_default();
        eprintln!("{}", diag_print::format_diag(&rscel_file.display().to_string(), &cell_name, d));
    }
    eprintln!("check: {} cells, {} errors, {} warnings", parsed.program.cells().count(), n_err, n_warn);
    if n_err > 0 { 1 } else { 0 }
}

// ---------------------------------------------------------------- build

/// 跑 `rscel build`：parse + check + gen + `.rs`/`.map.json` 落盘 + rustc + §15.2 反映射。
///
/// `cache` 决定这次构建能不能读/写 §16.2 的增量缓存（CLI 的 `--no-cache` 传 `Disabled`）。
/// 这个参数是 `cli::Handlers` 要的 `fn` 指针形状，所以从命令行一路传到这儿，
/// 不在实现里读全局状态。
///
/// 诊断一律走 stderr（§4.2），所以这里就是真实用户路径；
/// 想在测试里断言诊断文本请用 [`run_build_with`]。
pub fn run_build(rscel_file: &Path, output: &Path, mode: BuildMode, cache: build_cache::CachePolicy) -> i32 {
    run_build_with_policy(rscel_file, output, mode, cache, &mut |line| eprintln!("{line}"))
}

/// [`run_build`] 的实现体，诊断写进可注入的 `sink`，**默认允许用增量缓存**。
///
/// **为什么要开这个口子**：`eprintln!` 打到进程真实 stderr，集成测试拿不到，
/// 于是「build 失败必须报 RSC-E900 + 正确的 cell @ 行:列」这条验收只能靠静态字符串断言 ——
/// 那正是规约禁止的假测试。注入 sink 后，测试跑的是**真 rustc、真 codegen、真反映射**，
/// 只是把输出换个地方收。
pub fn run_build_with(
    rscel_file: &Path,
    output: &Path,
    mode: BuildMode,
    sink: &mut dyn FnMut(&str),
) -> i32 {
    run_build_with_policy(rscel_file, output, mode, build_cache::CachePolicy::Enabled, sink)
}

/// 同 [`run_build_with`]，但把「能不能用增量缓存」也做成参数。
pub fn run_build_with_policy(
    rscel_file: &Path,
    output: &Path,
    mode: BuildMode,
    policy: build_cache::CachePolicy,
    sink: &mut dyn FnMut(&str),
) -> i32 {
    let src = match std::fs::read_to_string(rscel_file) {
        Ok(s) => s,
        Err(e) => {
            sink(&format!("RSC: cannot read {}: {e}", rscel_file.display()));
            return 3;
        }
    };
    let parsed = parse_program(&src, 0);
    if parsed.has_errors() {
        for d in &parsed.diagnostics {
            sink(&d.render(&rscel_file.display().to_string()));
        }
        return 1;
    }
    let check_outcome = check_program(&parsed.program);
    let n_err = check_outcome.diagnostics.iter().filter(|d| d.severity == rscel_parser::Severity::Error).count();
    if n_err > 0 {
        for d in &check_outcome.diagnostics {
            let cell_name = d.cell.clone().unwrap_or_default();
            sink(&diag_print::format_diag(&rscel_file.display().to_string(), &cell_name, d));
        }
        sink(&format!("check failed: {n_err} errors"));
        return 1;
    }

    // gen：`.rs` 与 `.map.json` 一次算出（§12.9）。
    // `generate_as_with` 传的是 `.rs` 的文件名 —— rustc 报的 `file_name` 以它结尾，
    // errmap 用这个后缀校验「诊断确实来自我们的生成文件」。
    //
    // §16.1 的模式表把 `build --debug` 定成「全流水线 + rustc `-C opt-level=0 -g` + 埋点」，
    // 而 §15.3 的埋点是**生成期插入的语句**，所以这一维必须由 gen 侧决定，不是 rustc flag。
    // 只有 Debug 模式开：Release 的生成物是性能门禁（`scripts/bench/run.sh`）的被测对象，
    // 埋点一行都不许落进去 —— 这条不变式由 `rscel-gen` 的两条测试钉住：
    // `release_build_has_no_trace_artifact_in_any_shape`（`--debug` 关时零埋点）与
    // `debug_output_adds_only_whole_lines_apart_from_the_permit_closure`
    // （开埋点不改动 release 已有的任何一行）。
    let opts = GenOptions { trace: mode == BuildMode::Debug };
    let rs_path = output.with_extension("rs");
    let map_path = output.with_extension("map.json");
    let rs_file_name = rs_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "generated.rs".to_string());
    let gen = generate_as_with(&parsed.program, &rs_file_name, opts);
    if let Err(e) = std::fs::write(&rs_path, &gen.rust_source) {
        sink(&format!("RSC: cannot write {}: {e}", rs_path.display()));
        return 3;
    }

    // §4.2 的中间产物 `<name>.map.json`：与 `.rs` 同批落地，缺一 build 就不算成功。
    let Some(map_text) = gen.source_map.as_deref() else {
        sink("RSC: 内部错误 —— rscel-gen 未产出源映射（§12.9）；拒绝继续构建");
        return 3;
    };
    if let Err(e) = std::fs::write(&map_path, map_text) {
        sink(&format!("RSC: cannot write {}: {e}", map_path.display()));
        return 3;
    }
    // 把落盘的文本读回结构化映射表再喂给 errmap：
    //   - 「写出 -> 读回」这条路让 gen 的序列化器与 CLI 的解析器在**每次构建**时互验，
    //     格式一旦漂移立刻在这里失败，而不是等用户撞上 rustc 报错才发现映射是坏的；
    //   - 表直接从内存递下去，不再依赖刚写的文件可读（少一层 IO 失败面）。
    let map = match rscel_gen::srcmap::parse_srcmap_json(map_text) {
        Ok(m) => m,
        Err(e) => {
            sink(&format!("RSC: 内部错误 —— 源映射自校验失败（{e}）；拒绝继续构建"));
            return 3;
        }
    };
    sink(&format!(
        "Generated {} ({} lines), {}",
        rs_path.display(),
        gen.rust_source.lines().count(),
        map_path.display()
    ));

    // rustc：`--error-format=json` + 捕获 stderr（§15.2；注意 cargo 才用 --message-format）。
    //
    // §16.2 增量缓存在这里插一根岔路：**冷构建与命中的区别只有"要不要真跑 rustc"**，
    // 前面那段（parse/check/gen/落盘/映射表自校验）一行都没跳过，所以诊断永远实时产生；
    // 命中时把条目里存的 rustc stderr 原文喂给**同一个** `emit_rustc_warnings`，
    // 用户与 `scripts/bench/run.sh` 看到的文本因此和冷构建一致（告警信号不会被命中吞掉）。
    let rustc_args = rustc_command_args(mode);
    let cache = match policy {
        build_cache::CachePolicy::Enabled => build_cache::Cache::open(rscel_file),
        build_cache::CachePolicy::Disabled => None,
    };
    // `rustc -vV` 全文进身份：换了工具链（含 host triple / LLVM）就不能吃旧产物。
    // 但那次 spawn 自己就要 52–67ms，所以它**只在有东西可验的时候**才串行付（`docs/05` P2-③b）：
    // 条目根（名字不含版本）不存在 ⇒ 这次调用从没编过，此时把版本查询非阻塞地发出去、
    // 与下面的编译重叠跑，编译完再收来写条目。拿不到版本一律"不写条目、构建照常"。
    let call = build_cache::RustcCall {
        rs_path: &rs_path,
        output,
        args: &rustc_args,
        generated_rs: gen.rust_source.as_bytes(),
    };
    let dsl_display = rscel_file.display().to_string();
    let mut version_now: Option<String> = None;
    let mut overlapped_version: Option<std::process::Child> = None;
    if let Some(cache) = cache.as_ref() {
        if cache.base_dir(&call).is_dir() {
            version_now = build_cache::rustc_version();
            if let Some(version) = version_now.as_deref() {
                let identity = build_cache::RustcIdentity { call, rustc_version: version };
                if let Some((exe, stderr_json)) = cache.lookup(&identity) {
                    if let Err(e) = std::fs::write(output, &exe) {
                        sink(&format!("RSC: cannot write {}: {e}", output.display()));
                        return 3;
                    }
                    sink(&format!(
                        "RSC: 增量缓存命中，跳过 rustc（{} 字节产物来自同一条命令行的构建）",
                        exe.len()
                    ));
                    emit_rustc_warnings(&stderr_json, &map, &dsl_display, sink);
                    sink(&format!("Output: {}", output.display()));
                    return 0;
                }
            }
        } else {
            overlapped_version = build_cache::spawn_rustc_version();
        }
    }

    let rustc = match invoke_rustc(&rs_path, output, &rustc_args) {
        Ok(r) => r,
        Err(e) => {
            // 这条路上还挂着那个重叠跑的版本查询：必须收，`Child` 就地丢弃等于
            // 留一个没人 reap 的子进程（编译失败与否都不该把它当垃圾）。
            if let Some(child) = overlapped_version.take() {
                let _ = build_cache::collect_rustc_version(child);
            }
            sink(&format!("RSC: rustc invocation failed: {e}"));
            return 3;
        }
    };
    // 无条件收一次（它的值只在写条目时用得上，失败路径直接丢掉）：
    // 上面两个分支本来互斥，但"必须 reap 子进程"这件事不该靠那个不变式成立。
    let overlapped = overlapped_version
        .take()
        .and_then(build_cache::collect_rustc_version);
    let version = version_now.or(overlapped);
    if !rustc.status.success() {
        let rendered = errmap::render_diagnostics(&rustc.stderr_json, &map, &dsl_display, false);
        for line in rendered.lines() {
            sink(line);
        }
        sink(&format!(
            "rustc 编译失败 (exit {:?}): {} 条带位置的 error 诊断",
            rustc.status.code(),
            errmap::count_errors(&rustc.stderr_json)
        ));
        // 失败的构建**不进缓存**：一次 rustc 非零退出的产物根本不存在，
        // 而"该不该报错"由实时流水线决定，缓存失败信息只会掩盖源码变化。
        return 2;
    }
    // rustc 退出 0：§16.1 只有非零退出才算编译失败，所以 warning 只报告、不改变结果。
    if let (Some(cache), Some(version)) = (cache.as_ref(), version.as_deref()) {
        let identity = build_cache::RustcIdentity { call, rustc_version: version };
        if let Err(e) = cache.store(&identity, &rustc.stderr_json) {
            // 说得出原因，但不把「缓存没写成」冒充成构建失败。
            sink(&format!("RSC: 增量缓存未写入（{e}）；本次构建结果不受影响"));
        }
    }
    emit_rustc_warnings(&rustc.stderr_json, &map, &dsl_display, sink);
    sink(&format!("Output: {}", output.display()));
    0
}

/// rustc 的 stderr（NDJSON）→ 用户看到的告警行。冷构建与缓存命中共用，
/// 这样"命中"不会让诊断文本换一个形状。
fn emit_rustc_warnings(stderr_json: &str, map: &SrcMap, dsl_display: &str, sink: &mut dyn FnMut(&str)) {
    let rendered = errmap::render_diagnostics(stderr_json, map, dsl_display, true);
    for line in rendered.lines() {
        sink(line);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildMode {
    Debug,
    Release,
}

impl BuildMode {
    /// rustc 优化级别参数。
    ///
    /// **踩过的坑**：之前 debug 模式传的是 `-O0`——那是 C/C++ 的 flag，rustc 不认，
    /// 直接 `error: Unrecognized option: '0'` 并且整个 `--debug` 路径从来没跑通过。
    /// rustc 的正确写法是 `-C opt-level=N`；debug 还要 `-g` 出调试信息（§16.1）。
    /// release 用 `opt-level=3 + codegen-units=1`（与 workspace 的 `[profile.release]`
    /// 一致），这是 §17.3 基准比值能与"手写 Rust"对齐的前提——两边必须用同一组 flag。
    fn rustc_args(self) -> &'static [&'static str] {
        match self {
            BuildMode::Debug => &["-C", "opt-level=0", "-g"],
            BuildMode::Release => &["-C", "opt-level=3", "-C", "codegen-units=1"],
        }
    }
}

/// rustc 的**完整参数向量**（不含输入文件与 `-o`）。
///
/// 缓存身份与真正的 `Command` 都从这一份来 —— 分成两处写迟早会漂移，而身份里的 flag
/// 与实际跑的 flag 不一致，后果就是"用 A 组 flag 编出的产物去答 B 组 flag 的问题"。
pub fn rustc_command_args(mode: BuildMode) -> Vec<String> {
    let mut args: Vec<String> = vec!["--edition=2021".to_string()];
    args.extend(mode.rustc_args().iter().map(|a| a.to_string()));
    args.push("--error-format=json".to_string());
    args.push("--crate-type=bin".to_string());
    args
}

/// rustc 的一次运行结果：退出状态 + 捕获到的 stderr（NDJSON）。
struct RustcRun {
    status: std::process::ExitStatus,
    /// rustc 的 stderr 原文。加了 `--error-format=json` 后每行是一个 JSON 诊断，
    /// 但 flag 解析失败之类的启动期错误仍是纯文本，所以这里保留原文不做预处理。
    stderr_json: String,
}

/// 调 rustc 编译生成物。
///
/// **改处的地方**（§15.2）：原来是 `Command::status()`，rustc 的人肉输出直接透传给用户，
/// 报的是 `fl.rs:412:24` —— 那个文件用户既看不到也不该改。现在加 `--error-format=json`
/// 并把 stderr 捕获下来，交给 `errmap` 做反映射。
///
/// rustc 不存在 / 无法启动仍是 `Err(_)` → 上层退 3（保持既有行为）。
fn invoke_rustc(rs_path: &Path, output: &Path, args: &[String]) -> std::io::Result<RustcRun> {
    let mut cmd = Command::new("rustc");
    for a in args {
        cmd.arg(a);
    }
    cmd.arg(rs_path).arg("-o").arg(output);
    cmd.stdin(Stdio::null());
    // `output()` 同时捕获 stdout/stderr，子进程不会再往用户的终端写任何东西；
    // 生成物编译成功的 rustc 不往 stdout 写东西，故 stdout 丢弃是安全的。
    let out = cmd.output()?;
    Ok(RustcRun { status: out.status, stderr_json: String::from_utf8_lossy(&out.stderr).into_owned() })
}

// ---------------------------------------------------------------- public API 暴露给 bin

/// 五个子命令的实现集合。
///
/// 为什么是 `const` 的 fn 指针表而不是直接在各处调用：`cli::run` 因此可以被测试
/// 塞进假 handler（见 `cli::tests::run_dispatches_to_every_handler`），
/// 而 `run_build_with` 这类「可注入 sink」的口子也才有存在的理由 ——
/// 诊断文本要能被断言，不能只落到真实 stderr 上。
const HANDLERS: cli::Handlers = cli::Handlers {
    check: run_check,
    build: run_build,
    graph: graph::run_graph,
    trace: trace_cli::run_trace,
    edit: serve::run_edit,
};

/// 跑 `rscel` 的任一子命令（main.rs 调用）。
pub fn run(args: &[String]) -> i32 {
    cli::run(args, &HANDLERS)
}

// 内置 help / version 字符串
pub const VERSION: &str = concat!("rscel ", env!("CARGO_PKG_VERSION"));
pub const HELP: &str = "Usage:
  rscel check <file.rscel>
  rscel build <file.rscel> -o <out> [--release | --debug] [--no-cache]
  rscel graph <file.rscel> [--mermaid]
  rscel trace <file.rscel> [--max-items <N>] [--arg <值> | <键>=<值>]...
  rscel edit [file.rscel] [--port 8421]
  rscel --version | --help

子命令：
  check   parse + C1–C8 静态检查，诊断写 stderr，不调 rustc。
  build   全流水线 + rustc；中间产物 <out>.rs / <out>.map.json（§15.2 反映射靠它）。
          §16.2 增量缓存默认开：整次 rustc 调用（生成物字节 + .rs 路径 + -o 路径 + flag +
          rustc -vV）逐字段相同才算命中，命中就跳过 rustc 并重放它当初的 stderr；
          --no-cache 关掉读与写（冷构建与被缓存的构建产出同一份 .rs / .map.json）。
  graph   §16.4 调用链路报告：缩进树（标注 group / 并发上限 / 键名 / go with vs read with /
          风险点）；--mermaid 出 `graph TD`。只用 check 的拓扑模型，不走 gen。
  trace   §16.5 干跑模拟（不真正 spawn）：按 CFG 顺序打印 cell → jump(target, keys) → …
          以及 Packet 键名与 died 标记。--arg 给入口实参（`n=5` 具名 / `5` 按位置）；
          --max-items 控制 over 实例数 / loop 轮数 / recurse 层数（默认 4）。
          边界：when 条件只认「字面量 / 简单标识符 + == != < > <= >=」，其余标
          (uninterpretable) 并按不命中处理；Rust 表达式一律不求值。
  edit    起本地 HTTP 编辑器（**只监听 127.0.0.1**，默认 8421）：
          GET / 页面、GET /api/initial、GET /api/source?kind=rs|map|stdout、
          POST /api/check|graph|trace|build|save。
          保存只允许写启动时指定的文件与工作目录直接下面的 .rscel。

退出码：
  0 成功 / 1 前端错误 / 2 rustc 错误 / 3 参数或 IO 错误（含 edit 端口被占用）
";

// 内部：暴露 diag 打印给 CLI 测试用
pub use diag_print::format_diag_for_test;

/// 在 std::path::Path 上做工作区级路径解析（用于打印）
pub fn display_path(p: &Path) -> String {
    p.display().to_string()
}

/// 给 main.rs 用的 PathBuf 解析辅助（cli.rs 内用）
pub fn path(s: &str) -> PathBuf {
    PathBuf::from(s)
}
