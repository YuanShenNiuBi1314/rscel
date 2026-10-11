//! `rscel edit` 后端的集成测试：**真发 TCP 请求**，不打桩。
//!
//! 为什么必须走真 socket：这个后端的全部风险都在传输层 —— 头没读完、
//! Content-Length 对不上（前端 fetch 会永久挂住）、畸形请求把线程 panic 掉、
//! 保存路径越界。这些用「直接调 api::check_json」一个都测不到。
//!
//! 三件事是硬的：
//! 1. 端口用 `127.0.0.1:0` 让内核分配，测试之间不抢 8421；
//! 2. 每个连接都设 `set_read_timeout`，服务侧也有 `READ_TIMEOUT` ——
//!    **任何一条断言失败都只会让测试超时失败，不会永久阻塞**；
//! 3. `Server` 在测试结束时 drop（置停止标志），accept 线程随进程退出，
//!    不留后台进程。

use rscel_cli::json::form_encode;
use rscel_cli::serve::{EditOptions, Server};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 客户端读超时：服务侧是 15 s，这里给 10 s —— 真挂住时测试报告里能看到是客户端等爆的。
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

struct Reply {
    status: u16,
    headers: String,
    body: String,
}

/// 起一个绑在临时端口上的服务，保存根目录指向一个独立临时目录。
fn start_server(file: Option<PathBuf>) -> Server {
    let workdir = std::env::temp_dir().join(format!(
        "rscel-edit-test-workdir-{}-{}",
        std::process::id(),
        nanos()
    ));
    std::fs::create_dir_all(&workdir).expect("临时工作目录必须能建");
    let server = Server::start(&EditOptions { file, port: 0, workdir: Some(workdir) })
        .expect("127.0.0.1:0 绑定必须成功");
    // 端口 0 让内核分配；读回真实端口，测试才能连上来。
    assert_eq!(server.addr().ip().to_string(), "127.0.0.1", "只能绑回环，不能绑 0.0.0.0");
    server
}

fn nanos() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

/// 发一个请求并读满响应（`Connection: close` + 服务侧 shutdown(Write) 保证会结束）。
fn send(addr: SocketAddr, raw: &[u8]) -> Reply {
    let mut stream = TcpStream::connect(addr).expect("连接本地服务必须成功");
    stream.set_read_timeout(Some(CLIENT_TIMEOUT)).unwrap();
    stream.set_write_timeout(Some(CLIENT_TIMEOUT)).unwrap();
    stream.write_all(raw).expect("写请求必须成功");
    stream.flush().unwrap();
    let mut buf = Vec::new();
    // read_to_end：服务写完就 shutdown(SHUTDOWN_WRITE)，所以这里必然结束。
    stream.read_to_end(&mut buf).expect("读响应必须成功（超时会 panic 而不是永久阻塞）");

    let text = String::from_utf8_lossy(&buf).into_owned();
    let split = text.find("\r\n\r\n").expect("响应必须有头部与正文的分隔空行");
    let head = text[..split].to_string();
    let body = text[split + 4..].to_string();
    let status_line = head.lines().next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_else(|| panic!("状态行没有状态码: {status_line:?}"))
        .parse()
        .unwrap_or_else(|_| panic!("状态码不是数字: {status_line:?}"));

    // **每个响应都必须带 Content-Length，且与正文字节数一致**：
    // 少了它前端 fetch 会一直挂着；对不上它会读到半个 JSON。
    let declared: usize = body_len_declared(&head).unwrap_or_else(|| panic!("响应缺少 Content-Length:\n{head}"));
    assert_eq!(declared, body.len(), "Content-Length 必须等于正文字节数（响应头:\n{head}）");
    Reply { status, headers: head, body }
}

fn body_len_declared(head: &str) -> Option<usize> {
    for line in head.lines().skip(1) {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                return v.trim().parse().ok();
            }
        }
    }
    None
}

fn get(addr: SocketAddr, path: &str) -> Reply {
    send(addr, format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").as_bytes())
}

fn post_form(addr: SocketAddr, path: &str, pairs: &[(&str, &str)]) -> Reply {
    let body: String = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", form_encode(k), form_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let raw = form_request(path, &body, &[]);
    send(addr, raw.as_bytes())
}

/// 带额外请求头的表单 POST（专门用来发跨站 `Origin` / 非回环 `Host`）。
fn post_form_with_headers(addr: SocketAddr, path: &str, pairs: &[(&str, &str)], extra: &[(&str, &str)]) -> Reply {
    let body: String = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", form_encode(k), form_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let raw = form_request(path, &body, extra);
    send(addr, raw.as_bytes())
}

/// 组装一条**合法形状**的表单请求：`extra` 里的头**替换**同名默认头
/// （测试要发的正是「除了这一处，其它都与真实客户端一致」的形状）。
fn form_request(path: &str, body: &str, extra: &[(&str, &str)]) -> String {
    let mut headers: Vec<(String, String)> = vec![
        ("Host".to_string(), "127.0.0.1".to_string()),
        ("Content-Type".to_string(), "application/x-www-form-urlencoded".to_string()),
    ];
    for (k, v) in extra {
        match headers.iter_mut().find(|(name, _)| name.eq_ignore_ascii_case(k)) {
            Some(slot) => slot.1 = v.to_string(),
            None => headers.push((k.to_string(), v.to_string())),
        }
    }
    let mut raw = format!("POST {path} HTTP/1.1");
    for (k, v) in headers {
        raw.push_str(&format!("\r\n{k}: {v}"));
    }
    raw.push_str(&format!("\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()));
    raw
}

const LEGAL: &str = r#"entry main;

cell main;
receive();
jump and worker over [1, 2] group g1;
jump sync g1;
out("ok");
exit go with();

cell worker;
receive(x: i64);
exit go with(x * 2 -> d);
"#;

const ILLEGAL: &str = r#"cell main;
receive();
jump ghost go with(1 -> x);
"#;

/// 带多包 receive 的并行样例（附录 A.2 的形状）—— graph / trace 两条断言
/// 要看的 group 槽位、包限定键、sync 解包只有这种程序才有。
const CONCURRENT: &str = r#"entry main;

cell main;
receive(g1.W1.d, g1.W2.d) as (a, b);
jump and worker over [1, 2] group g1;
jump sync g1;
out("total=", a + b);
exit go with();

cell worker;
receive(x: i64);
exit go with(x * 2 -> d);
"#;

// ---------------------------------------------------------------- 页面与初始文档

#[test]
fn get_root_serves_the_editor_page() {
    let server = start_server(None);
    let r = get(server.addr(), "/");
    assert_eq!(r.status, 200);
    assert!(r.headers.to_ascii_lowercase().contains("text/html"), "{:?}", r.headers);
    assert!(r.body.contains("rscel"), "页面正文必须真的是那个编辑器");
    assert!(r.body.contains("/api/check"), "页面里应当能看到它调的端点");
    server.stop();
}

#[test]
fn initial_endpoint_returns_started_file_or_builtin_sample() {
    let dir = std::env::temp_dir().join(format!("rscel-edit-test-doc-{}", nanos()));
    std::fs::create_dir_all(&dir).unwrap();
    let doc = dir.join("given.rscel");
    std::fs::write(&doc, LEGAL).unwrap();

    let server = start_server(Some(doc.clone()));
    let r = get(server.addr(), "/api/initial");
    assert_eq!(r.status, 200);
    assert!(r.body.contains("given.rscel"), "{body}", body = &r.body[..r.body.len().min(400)]);
    assert!(r.body.contains("jump and worker"), "source 必须是启动文件的内容");

    let bare = start_server(None);
    let r2 = get(bare.addr(), "/api/initial");
    assert!(r2.body.contains("\"file\": null"), "{}", r2.body);
    assert!(r2.body.contains("cell fact"), "没带文件时要给内置样例");
    server.stop();
    bare.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- /api/check

#[test]
fn check_endpoint_returns_zero_errors_for_legal_program() {
    let server = start_server(None);
    let r = post_form(server.addr(), "/api/check", &[("source", LEGAL)]);
    assert_eq!(r.status, 200);
    assert!(r.body.contains("\"errors\": 0"), "{}", r.body);
    assert!(r.body.contains("\"warnings\": 0"), "{}", r.body);
    assert!(r.body.contains("\"cells\": 2"), "{}", r.body);
    assert!(r.body.contains("\"diagnostics\": []"), "{}", r.body);
    server.stop();
}

#[test]
fn check_endpoint_reports_error_code_and_dsl_line_not_rust_line() {
    let server = start_server(None);
    let r = post_form(server.addr(), "/api/check", &[("source", ILLEGAL)]);
    assert_eq!(r.status, 200, "诊断本身不是 HTTP 错误");
    assert!(r.body.contains("\"errors\": 1"), "{}", r.body);
    assert!(r.body.contains("ghost"), "诊断要说清是哪个目标：{}", r.body);
    assert!(r.body.contains("\"severity\": \"error\""), "{}", r.body);
    // E0xx：数字码（前端渲染成 RSC-E0NN），必须在附录 B / §6 扩展的区间里。
    let code = extract_number(&r.body, "code").expect("诊断里必须有 code 字段");
    assert!((10..=90).contains(&code), "RSC-E{code:03} 不在已知码表里：{}", r.body);
    // **行列必须是 DSL 行列**：`jump ghost` 在源文件第 3 行。
    let line = extract_number(&r.body, "line").expect("诊断里必须有 line 字段");
    assert_eq!(line, 3, "必须报 DSL 的行号（生成 Rust 的行号对用户无意义）：{}", r.body);
    let col = extract_number(&r.body, "col").expect("诊断里必须有 col 字段");
    assert_eq!(col, 1, "列也要是 DSL 的列：{}", r.body);
    server.stop();
}

fn extract_number(body: &str, key: &str) -> Option<u64> {
    let pat = format!("\"{key}\": ");
    let at = body.find(&pat)? + pat.len();
    let rest = body[at..].trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

// ---------------------------------------------------------------- /api/graph

#[test]
fn graph_endpoint_nodes_equal_cell_count_and_edges_carry_and_kind() {
    let server = start_server(None);
    let r = post_form(server.addr(), "/api/graph", &[("source", CONCURRENT)]);
    assert_eq!(r.status, 200);
    // `cols` 只有节点有（groups 也带 name，所以不能用 name 计数）。
    assert_eq!(count_occurrences(&r.body, "\"cols\": "), 2, "nodes 数必须等于 Cell 数：{}", r.body);
    assert!(r.body.contains("\"kind\": \"and\""), "edges 必须含 and：{}", r.body);
    assert!(r.body.contains("\"kind\": \"sync\""), "edges 必须含 sync：{}", r.body);
    // §16.4 要求图上能标注的三样东西：group / 并发上限的字段位 / 键名。
    assert!(r.body.contains("\"group\": \"g1\""), "{}", r.body);
    assert!(r.body.contains("\"slots\": 2"), "groups 要带多包槽位数：{}", r.body);
    assert!(r.body.contains("g1.W1"), "receive 里要能看到包限定键：{}", r.body);
    assert!(r.body.contains("\"is_entry\": true"), "{}", r.body);
    assert!(r.body.contains("\"needs_async\": false"), "没有 .await 的 Cell 不该被标 async：{}", r.body);
    assert!(r.body.contains("\"terminal\": \"exit go with\""), "terminal 要能对上 §16.4：{}", r.body);
    server.stop();
}

fn count_occurrences(hay: &str, needle: &str) -> usize {
    hay.match_indices(needle).count()
}

// ---------------------------------------------------------------- /api/trace

#[test]
fn trace_endpoint_yields_steps_with_packet_and_maxitems_is_honoured() {
    let server = start_server(None);
    let r = post_form(server.addr(), "/api/trace", &[("source", CONCURRENT), ("maxItems", "4")]);
    assert_eq!(r.status, 200);
    assert!(r.body.contains("\"steps\": ["), "{}", r.body);
    assert!(count_occurrences(&r.body, "\"seq\": ") >= 4, "至少 and/子实例/sync/exit 几步：{}", r.body);
    assert!(r.body.contains("\"kind\": \"and\""), "{}", r.body);
    assert!(r.body.contains("\"kind\": \"sync\""), "{}", r.body);
    assert!(r.body.contains("x * 2"), "packet 要带子实例的输出值文本：{}", r.body);
    assert!(r.body.contains("\"died\": false"), "{}", r.body);
    assert!(r.body.contains("g1.W1.d"), "sync 的 packet 要用完整多包键：{}", r.body);

    // maxItems=1 → over 的两个实例只模拟 1 个，并且 notes 里写明截断（不静默）。
    let t = post_form(server.addr(), "/api/trace", &[("source", CONCURRENT), ("maxItems", "1")]);
    assert!(t.body.contains("已截断，实际 2 个实例"), "截断必须写进 notes：{}", t.body);
    // 非法 maxItems → 400（而不是默默用默认值）。
    let bad = post_form(server.addr(), "/api/trace", &[("source", CONCURRENT), ("maxItems", "abc")]);
    assert_eq!(bad.status, 400, "{}", bad.body);
    server.stop();
}

// ---------------------------------------------------------------- /api/build

/// P2-③a：连点两次 build 必须**命中增量缓存**，而改了源码那一次必须**不命中**。
///
/// 为什么这条测试值得存在：`docs/05` P2 ③ 落地时就实测过「编辑器那条路径永远吃不到缓存」
/// （`build_cache.rs` 模块头写着这件事，键含产物名而产物名每次递增），而当时**没有任何运行时
/// 读数会红** —— 只有静态审查能看出来。现在产物名固定成 `<prefix>-last`，
/// 这一格的行为全靠这条测试钉住，两个方向都要钉：
/// ① 同一条源文本第二次必须命中（这是收益本身）；
/// ② **改了源文本绝不许命中**（这是稳定名的全部风险：一旦键少了生成的 `.rs` 字节，
///    用户点一下 build 拿到的是上一个程序的产物 —— 那比慢得多严重）。
#[test]
fn consecutive_builds_hit_the_cache_but_a_changed_source_does_not() {
    let server = start_server(None);
    let src = "cell main;\nreceive();\njump a go with(6 -> x);\n\ncell a;\nreceive(x);\nout(\"edited=\", x);\nexit go with();\n";

    let first = post_form(server.addr(), "/api/build", &[("source", src), ("mode", "debug")]);
    let exe1 = extract_string(&first.body, "exePath").expect("exePath");
    let err1 = extract_string(&first.body, "stderr").unwrap_or_default();
    assert_eq!(extract_number(&first.body, "exit"), Some(0), "冷构建该成功，stderr:\n{err1}");
    assert!(exe1.ends_with("-last.exe"), "产物名必须是稳定名（否则缓存键每次都不同）：{exe1}");
    assert!(!err1.contains("增量缓存命中"), "第一次是这条命令行的首次构建，不许报命中：{err1}");
    let out1 = extract_string(&first.body, "stdout").unwrap_or_default();
    assert!(out1.contains("edited=") && out1.contains('6'), "stdout 该来自真运行：{out1:?}");

    let second = post_form(server.addr(), "/api/build", &[("source", src), ("mode", "debug")]);
    let exe2 = extract_string(&second.body, "exePath").expect("exePath");
    let err2 = extract_string(&second.body, "stderr").unwrap_or_default();
    assert_eq!(extract_number(&second.body, "exit"), Some(0), "命中路径也该成功，stderr:\n{err2}");
    assert_eq!(exe1, exe2, "同一个会话的连点必须落在同一个产物名上");
    assert!(err2.contains("增量缓存命中"), "同一条命令行第二次必须命中，这才是 P2-③a 的收益：{err2}");
    assert_eq!(extract_string(&second.body, "stdout").unwrap_or_default(), out1, "命中产物运行结果要一致");

    // 改一个常数：产物名相同，但身份里的 `generated_rs` 变了 → 必须重编、必须跑出 7。
    let changed = src.replace("(6 -> x)", "(7 -> x)");
    let third = post_form(server.addr(), "/api/build", &[("source", &changed), ("mode", "debug")]);
    let err3 = extract_string(&third.body, "stderr").unwrap_or_default();
    assert_eq!(extract_number(&third.body, "exit"), Some(0), "改文本后的构建该成功，stderr:\n{err3}");
    assert_eq!(extract_string(&third.body, "exePath").expect("exePath"), exe1, "仍然用同一个稳定名");
    assert!(!err3.contains("增量缓存命中"), "改了源码还命中 = 把上一个程序的产物端给用户：{err3}");
    let out3 = extract_string(&third.body, "stdout").unwrap_or_default();
    assert!(out3.contains('7') && !out3.contains('6'), "产物必须真反映新文本：{out3:?}");
    server.stop();
}

/// P2-③a 的退路：稳定名那个位置**写不进去**时，退回递增名并把原因说出来。
///
/// 递增名原本绕开的就是「运行中的 exe 不能被替换」，所以固定名字之后必须有退路，
/// 否则上一次产物还在跑的这一类点击会直接变成 rustc 的 IO 失败。
/// 这里用**同名目录**模拟"写不进去"（以写方式打开目录必然失败），
/// 比真起一个进程占住 exe 稳定（那种写法会随调度先后随机红）。
#[test]
fn an_unwritable_stable_artifact_falls_back_to_an_incrementing_name_with_a_reason() {
    let server = start_server(None);
    let session = server.session();
    let blocker = session.artifact_dir.join(format!("{}-last.exe", session.build_prefix));
    std::fs::create_dir_all(&blocker).expect("占位目录必须能建");

    let src = "cell main;\nreceive();\njump a go with(6 -> x);\n\ncell a;\nreceive(x);\nout(\"edited=\", x);\nexit go with();\n";
    let r = post_form(server.addr(), "/api/build", &[("source", src), ("mode", "debug")]);
    let exe = extract_string(&r.body, "exePath").expect("exePath");
    let err = extract_string(&r.body, "stderr").unwrap_or_default();

    assert_eq!(extract_number(&r.body, "exit"), Some(0), "退路必须仍然构建成功，stderr:\n{err}");
    assert!(!exe.ends_with("-last.exe"), "那个位置写不进去却仍报稳定名 = 说了假话：{exe}");
    assert!(exe.contains(&session.build_prefix), "退回的名要还是本会话的：{exe}");
    assert!(err.contains("写不进去"), "必须说清楚为什么退：{err}");
    assert!(err.contains("重编 rustc"), "还必须说清代价（这一格必然 miss）：{err}");

    // 占位目录清掉，同一会话的下一次要能回到稳定名（这正是退路注释里承诺的行为）。
    std::fs::remove_dir(&blocker).expect("占位目录要能清掉");
    let back = post_form(server.addr(), "/api/build", &[("source", src), ("mode", "debug")]);
    let exe_back = extract_string(&back.body, "exePath").expect("exePath");
    let err_back = extract_string(&back.body, "stderr").unwrap_or_default();
    assert_eq!(extract_number(&back.body, "exit"), Some(0), "恢复后的构建该成功，stderr:\n{err_back}");
    assert!(exe_back.ends_with("-last.exe"), "占位清掉后必须回到稳定名：{exe_back}");
    server.stop();
}

/// `/api/build`：真跑 rustc、真运行产物、真取回映射后的诊断。
///
/// 这是最慢的一条（要编译），但任务书要求「四个端点各自是否真被测试打到」，
/// 而且 `stdout` 字段的语义（= 产物运行输出）只能靠真跑验证。
#[test]
fn build_endpoint_compiles_runs_and_exposes_artifacts() {
    let server = start_server(None);
    let src = "cell main;\nreceive();\njump a go with(6 -> x);\n\ncell a;\nreceive(x);\nout(\"edited=\", x);\nexit go with();\n";
    let r = post_form(server.addr(), "/api/build", &[("source", src), ("mode", "debug")]);
    assert_eq!(r.status, 200, "build 端点不该因编译失败而 500：{}", r.body);
    for key in ["\"exit\"", "\"rsPath\"", "\"mapPath\"", "\"exePath\"", "\"stdout\"", "\"stderr\"", "\"mapped\""] {
        assert!(r.body.contains(key), "缺字段 {key}：{}", r.body);
    }
    let exit = extract_number(&r.body, "exit").expect("必须有 exit 字段");
    assert_eq!(exit, 0, "build 应当成功，stderr:\n{}", extract_string(&r.body, "stderr").unwrap_or_default());
    // 生成物的 `out!` 是按空格串参的（`out("edited=", x)` → `edited= 6 `），
    // 所以这里只断言两个片段都在，不去猜它的分隔符。
    let printed = extract_string(&r.body, "stdout").unwrap_or_default();
    assert!(printed.contains("edited=") && printed.contains('6'), "stdout 要真来自运行产物，实际 {printed:?}：{}", r.body);

    // 上一次产物能被 GET /api/source 读回来（前端的「生成物」页签就靠它）。
    let rs = get(server.addr(), "/api/source?kind=rs");
    assert_eq!(rs.status, 200);
    assert!(rs.body.contains("fn") && rs.body.contains("main"), "生成的 .rs 文本：{}…", &rs.body[..rs.body.len().min(200)]);
    let map = get(server.addr(), "/api/source?kind=map");
    assert_eq!(map.status, 200);
    assert!(map.body.contains("\"map\""), "map.json 里应有映射表：{}…", &map.body[..map.body.len().min(200)]);

    // 未知 mode → 400；构建产物目录在 temp 下，不碰用户的源文件目录。
    let bad = post_form(server.addr(), "/api/build", &[("source", src), ("mode", "turbo")]);
    assert_eq!(bad.status, 400, "{}", bad.body);
    server.stop();
}

/// §15.3 × 终极目标 2：`mode=debug` 的构建把**运行期事件流**回传给前端。
///
/// 编辑器做拓扑调试时要的不是「编译过了」而是「运行时到底发生了些什么」：
/// `api.rs` 已经把产物的 stderr 原样带进 `stderr` 字段（`// 产物运行期 stderr` 那一节），
/// 这条测试钉住这条数据通路存在、且 release 模式确实一行都不带（§15.3「release 全部删除」）。
#[test]
fn debug_mode_build_endpoint_streams_trace_events() {
    let server = start_server(None);
    let src = "entry main;\n\ncell main;\nreceive();\nlet items = [1i64, 2i64];\njump and w over items concurrent_limit = 1 group g1;\njump sync g1;\nout(\"synced\");\nexit go with();\n\ncell w;\nreceive(x: i64);\ndie;\n";

    let r = post_form(server.addr(), "/api/build", &[("source", src), ("mode", "debug")]);
    assert_eq!(r.status, 200, "build 端点不该 500：{}", r.body);
    let exit = extract_number(&r.body, "exit").expect("必须有 exit 字段");
    let stderr = extract_string(&r.body, "stderr").unwrap_or_default();
    assert_eq!(exit, 0, "build 应当成功，stderr:\n{stderr}");
    assert!(
        stderr.contains("// 产物运行期 stderr"),
        "埋点走的是产物运行期 stderr 这一节：{stderr:?}"
    );
    assert!(
        stderr.contains("[rsc] group g1 derive main -> w limit=1 items=2 @ 6:1 send=[]"),
        "整组派生行要带运行期的真实 limit/items 与 DSL 位置，实际:\n{stderr}"
    );
    assert!(
        stderr.contains("[rsc] group g1 permit cell=w release died=true"),
        "每实例的 died 标记是「谁死了」的唯一真相来源，实际:\n{stderr}"
    );
    let stdout = extract_string(&r.body, "stdout").unwrap_or_default();
    assert!(
        !stdout.contains("[rsc] "),
        "埋点不能污染 stdout（前端的「结果」区仍以程序自己的输出为准）: {stdout:?}"
    );

    // 同一份 DSL 走 release：回传的 stderr 里一行埋点都不许有。
    let rel = post_form(server.addr(), "/api/build", &[("source", src), ("mode", "release")]);
    assert_eq!(rel.status, 200, "{}", rel.body);
    assert_eq!(
        extract_number(&rel.body, "exit").expect("exit"),
        0,
        "release 也应构建成功"
    );
    let rel_err = extract_string(&rel.body, "stderr").unwrap_or_default();
    assert!(
        !rel_err.contains("[rsc] "),
        "release 的产物不该有任何埋点: {rel_err:?}"
    );
    server.stop();
}

/// 从 JSON 里粗取一个字符串字段（只为测试断言，不当 JSON 解析器用）。
fn extract_string(body: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\": \"");
    let at = body.find(&pat)? + pat.len();
    let mut out = String::new();
    let mut escaped = false;
    for c in body[at..].chars() {
        if escaped {
            out.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            return Some(out);
        } else {
            out.push(c);
        }
    }
    None
}

#[test]
fn build_endpoint_reports_mapped_dsl_position_on_rust_error() {
    let server = start_server(None);
    // check 全过、但生成物编译不过的写法：把 String 直接当 i64 传（D2 推不出就报
    // compile_error!，推得出就是类型不匹配）—— 两条路都会留下 RSC-E900/E901。
    let src = "cell main;\nreceive();\njump a go with(\"文本\" -> x);\n\ncell a;\nreceive(x);\nlet y: i64 = x;\nout(y);\nexit go with();\n";
    let r = post_form(server.addr(), "/api/build", &[("source", src), ("mode", "debug")]);
    let exit = extract_number(&r.body, "exit").expect("exit");
    if exit == 0 {
        // 生成物真编译过了（D2 把 x 当字符串、`let y: i64 = x` 由 rustc 拦下才成立；
        // 万一将来这条能通过，也必须在这里显式失败，而不是静默跳过断言）。
        panic!("这条样例本该编译失败，请改断言：build exit=0，stderr 已含成功文本");
    }
    assert_eq!(exit, 2, "rustc 失败按 §4.1 退 2：{}", r.body);
    assert!(r.body.contains("RSC-E900") || r.body.contains("RSC-E901"), "mapped 里要有反映射后的码：{}", r.body);
    server.stop();
}

// ---------------------------------------------------------------- /api/save

#[test]
fn save_endpoint_writes_inside_workdir_and_rejects_every_escape() {
    let server = start_server(None);
    let workdir = server.session().workdir.clone();

    // 允许：工作目录直接下面的 .rscel
    let ok = post_form(server.addr(), "/api/save", &[("source", LEGAL), ("file", "saved.rscel")]);
    assert_eq!(ok.status, 200, "{}", ok.body);
    assert!(ok.body.contains("\"ok\": true"), "{}", ok.body);
    let written = workdir.join("saved.rscel");
    assert!(written.exists(), "保存必须真的落盘：{}", written.display());
    assert_eq!(std::fs::read_to_string(&written).unwrap(), LEGAL);

    // 拒绝：`..` 逃逸 / 绝对路径 / 错后缀 / 子目录
    for (case, file) in [
        (".. 逃逸", "../escaped.rscel"),
        ("绝对路径越界", "/tmp/outside.rscel"),
        ("错后缀", "notes.txt"),
        ("子目录", "sub/inner.rscel"),
    ] {
        let r = post_form(server.addr(), "/api/save", &[("source", LEGAL), ("file", file)]);
        assert_eq!(r.status, 400, "{case} 必须被拒（file={file}）：{}", r.body);
        assert!(r.body.contains("\"error\""), "{case} 的 400 要带理由：{}", r.body);
    }
    assert!(!Path::new("/tmp/outside.rscel").exists(), "被拒的请求绝不能已经写了文件");
    assert!(!workdir.join("escaped.rscel").exists(), "逃逸的相对路径不该在工作目录外面留下文件");

    // 缺 source 字段 → 400 而不是 panic
    let missing = post_form(server.addr(), "/api/save", &[("file", "x.rscel")]);
    assert_eq!(missing.status, 400, "{}", missing.body);

    server.stop();
    let _ = std::fs::remove_dir_all(&workdir);
}

// ---------------------------------------------------------------- 来源边界（跨站拦截）

/// 为什么这一组是**硬边界**而不是提示：任何网页都能对 `127.0.0.1` 发**无预检**的表单
/// POST（`application/x-www-form-urlencoded` 在 CORS 简单请求白名单里），而
/// `/api/save` 往磁盘写文件、`/api/build` 会 rustc 编译**并运行请求方提交的代码**
/// （`extern rust{}` 是任意 Rust，含 unsafe）—— 没有这道拦截，访问一个恶意网页
/// 就等于把开发机的文件写 + 代码执行交出去。
#[test]
fn cross_origin_requests_get_403_and_never_reach_any_endpoint() {
    let server = start_server(None);
    let addr = server.addr();
    let port = server.port();
    let workdir = server.session().workdir.clone();

    // 1. 跨站 Origin 的 /api/save → 403，且**一个字节都不写**。
    let r = post_form_with_headers(
        addr,
        "/api/save",
        &[("source", LEGAL), ("file", "from_web.rscel")],
        &[("Origin", "http://evil.example")],
    );
    assert_eq!(r.status, 403, "跨站 Origin 的 /api/save 必须 403，实际 {}：{}", r.status, r.body);
    assert!(r.body.contains("Origin"), "403 要指名是来源问题：{}", r.body);
    assert!(!workdir.join("from_web.rscel").exists(), "被拒的请求绝不能已经写了文件");

    // 2. 跨站 Origin 的 /api/build → 403，而且**根本没起 rustc**
    //    （会话计数器只有 build_json 会自增，这条是"路由之前就拒了"的证据）。
    let b = post_form_with_headers(
        addr,
        "/api/build",
        &[("source", LEGAL), ("mode", "debug")],
        &[("Origin", "http://evil.example:80")],
    );
    assert_eq!(b.status, 403, "{}", b.body);
    assert_eq!(server.session().builds.load(Ordering::Relaxed), 0, "被拒的 build 不该真的编译过");

    // 3. `Origin: null`（file:// 页面与沙箱 iframe 发的正是它）→ 同样拒。
    let n = post_form_with_headers(
        addr,
        "/api/save",
        &[("source", LEGAL), ("file", "from_frame.rscel")],
        &[("Origin", "null")],
    );
    assert_eq!(n.status, 403, "Origin: null 必须拒（iframe / file:// 的形状）：{}", n.body);
    assert!(!workdir.join("from_frame.rscel").exists());

    // 4. 非回环 Host = DNS rebinding 的形状（连到 127.0.0.1 但 Host 留着攻击者域名），
    //    GET 也一并拒：页面本身被这样取走就没有意义。
    let rebinding = send(
        addr,
        b"GET / HTTP/1.1\r\nHost: evil.example:8421\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(rebinding.status, 403, "非回环 Host 必须 403：{}", rebinding.body);
    assert!(rebinding.body.contains("Host"), "要说清是 Host 问题：{}", rebinding.body);
    let post_rebinding = post_form_with_headers(
        addr,
        "/api/check",
        &[("source", LEGAL)],
        &[("Host", "127.0.0.1.evil.example")],
    );
    assert_eq!(post_rebinding.status, 403, "`127.0.0.1.evil.example` 这类后缀陷阱必须拒：{}", post_rebinding.body);

    // 5. 回环 Origin 的两种写法都放行（同源页面发的就是 `http://127.0.0.1:<port>`，
    //    用户手打 localhost 时是 `http://localhost:<port>`）。
    for origin in [format!("http://127.0.0.1:{port}"), format!("http://localhost:{port}")] {
        let ok = post_form_with_headers(addr, "/api/check", &[("source", LEGAL)], &[("Origin", origin.as_str())]);
        assert_eq!(ok.status, 200, "回环 Origin {origin} 必须放行：{}", ok.body);
        assert!(ok.body.contains("\"errors\": 0"), "{}", ok.body);
    }

    // 6. 没有 Origin 的本机客户端（curl / 同源 GET）不能被挡住 —— 那是真实用法。
    let cli = post_form_with_headers(addr, "/api/check", &[("source", LEGAL)], &[]);
    assert_eq!(cli.status, 200, "无 Origin 的本机请求应照常服务：{}", cli.body);

    // 7. 缺 Host 的请求（HTTP/1.1 里 Host 必填）→ 拒，不落到路由。
    let no_host = send(addr, b"GET / HTTP/1.1\r\nConnection: close\r\n\r\n");
    assert_eq!(no_host.status, 403, "缺 Host 必须拒：{}", no_host.body);

    // 服务仍然活着（跨站探测不该把连接线程带崩）。
    let alive = post_form(addr, "/api/check", &[("source", LEGAL)]);
    assert_eq!(alive.status, 200, "拒了这么多之后服务必须还在：{}", alive.body);
    server.stop();
    let _ = std::fs::remove_dir_all(&workdir);
}

/// 页面与所有响应都拒绝被外站 iframe 内嵌：`X-Frame-Options: DENY`。
///
/// 为什么这一条和上面那组同属一个边界：恶意站点把编辑器**嵌进自己的 iframe** 时，
/// 框架内的页面与 API 是同源的，`Origin`/`Host` 全都合法，等于让用户在
/// 攻击者的页面里操作本机服务（点击劫持 / 诱导保存）。拒掉内嵌才闭环。
#[test]
fn every_response_refuses_framing() {
    let server = start_server(None);
    let page = get(server.addr(), "/");
    assert_eq!(page.status, 200);
    assert!(page.headers.to_ascii_lowercase().contains("x-frame-options: deny"), "{:?}", page.headers);
    let api = post_form(server.addr(), "/api/check", &[("source", LEGAL)]);
    assert!(api.headers.to_ascii_lowercase().contains("x-frame-options: deny"), "{:?}", api.headers);
    let missing = get(server.addr(), "/nope");
    assert!(missing.headers.to_ascii_lowercase().contains("x-frame-options: deny"), "{:?}", missing.headers);
    server.stop();
}

// ---------------------------------------------------------------- 传输层健壮性

#[test]
fn unknown_path_returns_json_404() {
    let server = start_server(None);
    let r = get(server.addr(), "/api/nope");
    assert_eq!(r.status, 405, "已知 /api/ 前缀但方法不对 → 405：{}", r.body);
    let r2 = get(server.addr(), "/definitely-not-here");
    assert_eq!(r2.status, 404);
    assert!(r2.body.contains("\"error\": \"not found\""), "{}", r2.body);
    assert!(r2.body.contains("\"hint\""), "404 要带可用端点提示：{}", r2.body);
    server.stop();
}

#[test]
fn malformed_requests_get_400_and_the_service_survives() {
    let server = start_server(None);
    let addr = server.addr();

    // 1. 没有 HTTP 版本的请求行
    let r = send(addr, b"GET /\r\n\r\n");
    assert_eq!(r.status, 400, "缺版本号的请求行必须 400：{}", r.body);
    // 2. 完全不是 HTTP 的垃圾字节
    let r = send(addr, b"HELLO WORLD\r\n\r\n");
    assert_eq!(r.status, 400, "{}", r.body);
    // 3. POST 不带 Content-Length（无法确定 body 边界）
    let r = send(addr, b"POST /api/check HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\nsource=abc");
    assert_eq!(r.status, 400, "POST 缺 Content-Length 必须 400：{}", r.body);
    // 4. Content-Length 超过 4 MiB 上限
    let r = send(addr, b"POST /api/check HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 99999999\r\n\r\n");
    assert_eq!(r.status, 400, "超长请求体必须 400：{}", r.body);
    assert!(r.body.contains("上限"), "要说明是上限问题：{}", r.body);
    // 5. body 不是合法 UTF-8（0xFF 不是任何 UTF-8 序列的开头）
    let body: &[u8] = b"source=\xff\xfe";
    let mut raw =
        format!("POST /api/check HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
    raw.extend_from_slice(body);
    let r = send(addr, &raw);
    assert_eq!(r.status, 400, "非法 UTF-8 请求体必须 400 而不是 panic：{}", r.body);
    // 6. percent 序列坏掉（%E4%B8 截断的多字节）
    let r = post_form_raw(addr, "/api/check", "source=%E4%B8");
    assert_eq!(r.status, 400, "{}", r.body);

    // 服务还活着：正常请求继续应答（这条才是「单连接出错不带崩服务」的证据）。
    let alive = post_form(addr, "/api/check", &[("source", LEGAL)]);
    assert_eq!(alive.status, 200, "前面那些烂请求之后服务必须还能用：{}", alive.body);
    assert!(alive.body.contains("\"errors\": 0"));
    server.stop();
}

/// 直接发未编码的表单串（专门用来测畸形 percent 序列）。
fn post_form_raw(addr: SocketAddr, path: &str, body: &str) -> Reply {
    let raw = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    send(addr, raw.as_bytes())
}

#[test]
fn source_endpoint_404s_before_any_build() {
    let server = start_server(None);
    let r = get(server.addr(), "/api/source?kind=rs");
    assert_eq!(r.status, 404, "没构建过就不该有产物：{}", r.body);
    assert!(r.body.contains("/api/build"), "404 要告诉用户先构建：{}", r.body);
    server.stop();
}

// ---------------------------------------------------------------- 端口与生命周期

/// 端口被占用时 `rscel edit` 必须退 3 且**不阻塞**（§4.1：参数/IO 错误 = 3）。
#[test]
fn occupied_port_exits_3_with_actionable_hint() {
    let busy = start_server(None);
    let port = busy.port().to_string();
    // run_edit 在绑定失败时立刻返回，不会进 accept 循环，所以这里不会挂。
    let code = rscel_cli::run(&["rscel".to_string(), "edit".to_string(), "--port".to_string(), port.clone()]);
    assert_eq!(code, 3, "端口 {port} 被占用应退 3");
    busy.stop();
}

/// 服务能被中断：stop() 后 accept 线程必须很快退出（不留后台进程）。
#[test]
fn server_stops_promptly_when_signalled() {
    let server = start_server(None);
    let addr = server.addr();
    // 先发一发确认服务确实在跑。
    assert_eq!(get(addr, "/").status, 200);
    let t0 = std::time::Instant::now();
    let mut owned = server;
    owned.wait(); // 置停止标志 + join accept 线程
    assert!(t0.elapsed() < Duration::from_secs(2), "accept 线程应在一个轮询间隔内退出，实际 {:?}", t0.elapsed());
    // 停服之后端口不再接受连接。
    assert!(TcpStream::connect(addr).is_err(), "停止后不该还能连上");
}

#[test]
fn concurrent_connections_are_served_independently() {
    // 每连接一条线程：两个连接同时半开（写完不读），第三条仍要被应答。
    let server = start_server(None);
    let addr = server.addr();
    let mut slow1 = TcpStream::connect(addr).unwrap();
    let mut slow2 = TcpStream::connect(addr).unwrap();
    slow1.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
    slow2.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
    // 不从 slow1/slow2 读：它们占着两条连接线程直到服务侧超时。
    let r = post_form(addr, "/api/check", &[("source", LEGAL)]);
    assert_eq!(r.status, 200, "占着不读的客户端不该拖死服务：{}", r.body);
    server.stop();
}
