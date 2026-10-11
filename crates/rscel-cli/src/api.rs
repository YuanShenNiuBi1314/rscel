//! `rscel edit` 的端点载荷（HTTP 传输在 [`crate::serve`]，这里只管「数据形状」）。
//!
//! ## 字段名的来源
//!
//! **全部照抄 `assets/editor.html` 文件头注释里的契约**，一个字段都不自创：
//! 前端已经写好且是唯一消费者，改名等于两边同时改一遍还得不到任何收益。
//! 返回里多带的几个字段（`reachable`、`returns`、`col`、`parent`、`synced`）
//! 是 topology 模型里已有的数据 —— 多给不破坏前端，少给会让 §16.4 要求的
//! 「标注 group / 并发上限 / 键名 / go with vs read with / 风险点」显示不出来。
//!
//! ## 行列一律是 DSL 行列
//!
//! `/api/check` 的 `line`/`col` 取自 `Diagnostic.span`（parser 的源位置），
//! **不是**生成 Rust 的行号。rustc 的报错只出现在 `/api/build` 的 `mapped` 里，
//! 且已被 §15.2 的 errmap 反映射回 DSL 位置 —— 把生成文件的行号给前端，
//! 点过去是一坨用户既看不到也不该改的代码，那正是 §15.2 要消灭的体验。
//!
//! ## 依赖方向
//!
//! 只用 `rscel_parser` / `rscel_check` / `rscel_gen` 的公开接口；
//! `serve → api` 单向（api 不认识 HTTP，才能被 `rscel graph/trace` 复用）。

use crate::errmap;
use crate::json::{field, Json};
use crate::{run_build_with, BuildMode};
use rscel_check::topology::Node;
use rscel_check::{check_program, topology, trace};
use rscel_parser::{parse_program, Diagnostic, Program, RawBlockPart, Severity, Stmt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

/// 没带文件启动时的内置样例（服务端给 `/api/initial`；页面离线打开另有自己的一份）。
pub const BUILTIN_SAMPLE: &str = r#"entry main;

// 附录 A.1：jump loop 版阶乘。`rscel edit` 没带文件时的内置样例。
cell main;
receive();
let five: i64 = 5;
jump fact go with(five -> n, 1i64 -> acc);

cell fact;
receive(n: i64, acc: i64) as (cur, sum);
let next = cur - 1;
let next_acc = sum * cur;
jump loop fact when next > 0 go with(next, next_acc);
out(next_acc);
exit go with(next_acc -> result);
"#;

/// 一次 `rscel edit` 会话：启动时确定的部分只读，构建产物用 Mutex 保护。
#[derive(Debug)]
pub struct Session {
    /// 命令行指定的 `.rscel` 文件（`/api/save` 白名单目标之一）。
    pub file: Option<PathBuf>,
    /// 初始文档正文。
    pub source: String,
    /// `/api/save` 的允许根目录（默认 = 当前工作目录）。
    pub workdir: PathBuf,
    /// 产物临时目录 `temp_dir()/rscel-edit-<pid>`：**不污染用户源文件目录**。
    pub artifact_dir: PathBuf,
    /// 本次会话的产物名前缀。目录按约定用 pid 命名，但同一进程里可能有多个
    /// Session（集成测试就是），所以文件名要再带一层唯一后缀，
    /// 否则两个会话的 `edit-1.rscel` 会互相覆盖，构建结果随机串台。
    pub build_prefix: String,
    /// 每次 build 递增，让并发构建不互相覆盖中间产物。
    pub builds: AtomicUsize,
    /// 上一次 build 的产物，供 `GET /api/source?kind=rs|map|stdout` 读文本。
    pub artifacts: Mutex<Artifacts>,
}

/// 上一次构建的可见产物。
#[derive(Debug, Default, Clone)]
pub struct Artifacts {
    pub rs: Option<String>,
    pub map: Option<String>,
    pub stdout: String,
    pub stderr: String,
    pub exit: i32,
    pub built: bool,
}

impl Session {
    /// 新建会话。`workdir = None` 用当前工作目录（真实 `rscel edit` 走这条），
    /// 测试注入临时目录，这样「允许写」与「拒绝越界」两条分支都能真跑到。
    pub fn new(file: Option<PathBuf>, workdir: Option<PathBuf>) -> std::io::Result<Session> {
        let workdir = match workdir {
            Some(w) => w,
            None => std::env::current_dir()?,
        };
        let (source, given) = match &file {
            Some(p) => (std::fs::read_to_string(p)?, Some(p.clone())),
            None => (BUILTIN_SAMPLE.to_string(), None),
        };
        let artifact_dir = std::env::temp_dir().join(format!("rscel-edit-{}", std::process::id()));
        std::fs::create_dir_all(&artifact_dir)?;
        // 会话名后缀取当前时刻的纳秒：够用且零依赖（不需要随机数 crate）。
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let build_prefix = format!("edit-{}-{}", std::process::id(), nanos);
        Ok(Session {
            file: given,
            source,
            workdir,
            artifact_dir,
            build_prefix,
            builds: AtomicUsize::new(0),
            artifacts: Mutex::new(Artifacts::default()),
        })
    }

    /// 锁产物表。**中毒的锁必须继续用**：某条连接的线程 panic 时若直接 unwrap，
    /// 之后每个 `/api/build` 都会跟着崩；而 panic 本来只该崩那一条连接。
    pub fn artifacts(&self) -> MutexGuard<'_, Artifacts> {
        self.artifacts.lock().unwrap_or_else(|e| e.into_inner())
    }
}

// ---------------------------------------------------------------- 端点

/// `GET /api/initial` → `{file, source}`
pub fn initial_json(session: &Session) -> Json {
    Json::object(vec![
        field("file", Json::opt(session.file.as_ref().map(|p| Json::text(&p.to_string_lossy())))),
        field("source", Json::text(&session.source)),
    ])
}

/// `POST /api/check` → `{diagnostics:[{code,severity,message,detail,cell,line,col}], cells, errors, warnings}`
pub fn check_json(source: &str) -> Json {
    let parsed = parse_program(source, 0);
    // parse 有错时契约表是基于残缺 AST 建的，再叠一层 E042/E044 是二阶噪声；
    // 与 `rscel check` 的既有行为一致：先只交语法错误。
    let outcome = check_program(&parsed.program);
    let diagnostics: Vec<&Diagnostic> =
        if parsed.has_errors() { parsed.diagnostics.iter().collect() } else { outcome.diagnostics.iter().collect() };
    let errors = diagnostics.iter().filter(|d| d.severity == Severity::Error).count();
    let warnings = diagnostics.iter().filter(|d| d.severity == Severity::Warning).count();
    Json::object(vec![
        field("diagnostics", Json::Arr(diagnostics.iter().copied().map(diag_json).collect())),
        field("cells", Json::Int(parsed.program.cells().count() as i64)),
        field("errors", Json::Int(errors as i64)),
        field("warnings", Json::Int(warnings as i64)),
    ])
}

fn diag_json(d: &Diagnostic) -> Json {
    Json::object(vec![
        // code 给**数字**：前端自己 `RSC-E${String(code).padStart(3,"0")}`，
        // 而 severity 单独一个字段（W 级诊断不能靠码值猜前缀）。
        field("code", Json::Int(d.code as i64)),
        field("severity", Json::text(match d.severity { Severity::Error => "error", Severity::Warning => "warning" })),
        field("message", Json::text(&d.message)),
        field("detail", Json::opt_str(&d.detail)),
        field("cell", Json::opt_str(&d.cell)),
        field("line", Json::Int(d.span.line as i64)),
        field("col", Json::Int(d.span.col as i64)),
    ])
}

/// `POST /api/graph` → `{entry, nodes, edges, groups}`
pub fn graph_json(source: &str) -> Json {
    let parsed = parse_program(source, 0);
    graph_of(&parsed.program)
}

/// 拓扑模型 → 前端字段（`rscel edit` 与将来的其它消费者共用）。
pub fn graph_of(program: &Program) -> Json {
    let topo = topology::topology_of(program);
    Json::object(vec![
        field("entry", Json::opt_str(&topo.entry)),
        field("nodes", Json::Arr(topo.nodes.iter().map(|n| node_json(program, n)).collect())),
        field("edges", Json::Arr(topo.edges.iter().map(edge_json).collect())),
        field("groups", Json::Arr(topo.groups.iter().map(group_json).collect())),
    ])
}

fn node_json(program: &Program, n: &Node) -> Json {
    Json::object(vec![
        field("name", Json::text(&n.name)),
        field("line", Json::Int(n.line as i64)),
        field("col", Json::Int(n.col as i64)),
        // 契约头里的 `cols`：解释成「入口槽位数」（前端布局真正用的是 receive 数组，
        // 这里补一个同义字段只为让 curl 出来的 JSON 自解释）。
        field("cols", Json::Int(n.receive.len() as i64)),
        field(
            "receive",
            Json::Arr(
                n.receive
                    .iter()
                    .map(|s| {
                        Json::object(vec![
                            field("key", Json::text(&s.key)),
                            field("pkg", Json::opt_str(&s.pkg)),
                            field("ty", Json::opt_str(&s.ty)),
                            field("alias", Json::text(&s.alias)),
                        ])
                    })
                    .collect(),
            ),
        ),
        field(
            "outputs",
            Json::Arr(
                n.outputs
                    .iter()
                    .map(|o| Json::object(vec![field("key", Json::text(&o.key)), field("expr", Json::text(&o.expr))]))
                    .collect(),
            ),
        ),
        field("is_entry", Json::Bool(n.is_entry)),
        field("needs_async", Json::Bool(needs_async(program, &n.name))),
        field("terminal", Json::text(&n.terminal)),
        field("reachable", Json::Bool(n.reachable)),
        field("risks", Json::strs(&n.risks)),
        field("stmts", Json::Int(n.stmts as i64)),
    ])
}

fn edge_json(e: &rscel_check::topology::Edge) -> Json {
    Json::object(vec![
        field("from", Json::text(&e.from)),
        field("to", Json::text(&e.to)),
        field("kind", Json::text(e.kind.as_str())),
        field("mode", Json::opt_str(&e.mode)),
        field("keys", Json::strs(&e.keys)),
        field("group", Json::opt_str(&e.group)),
        field("limit", Json::opt_str(&e.limit)),
        field("over", Json::opt_str(&e.over)),
        field("cond", Json::opt_str(&e.cond)),
        field("returns", Json::strs(&e.returns)),
        field("stmt", Json::Int(e.stmt as i64)),
        field("line", Json::Int(e.line as i64)),
        field("col", Json::Int(e.col as i64)),
        field("in_block", Json::Bool(e.in_block)),
    ])
}

fn group_json(g: &rscel_check::topology::Group) -> Json {
    Json::object(vec![
        field("name", Json::text(&g.name)),
        field("parent", Json::text(&g.parent)),
        field("targets", Json::strs(&g.targets)),
        field("limit", Json::opt_str(&g.limit)),
        field("over", Json::opt_str(&g.over)),
        field("slots", Json::Int(g.slots as i64)),
        field("synced", Json::Bool(g.synced)),
    ])
}

/// `POST /api/trace` → `{steps:[{seq,cell,stmt,line,kind,text,packet,group,note}], notes}`
pub fn trace_json(source: &str, max_items: usize) -> Json {
    trace_of_json(&trace::trace_program(&parse_ok(source), &trace_opts(max_items)))
}

/// 前端 `maxItems` 只给了数字，其它上限用默认值（step 5000 / 深度 64）。
pub fn trace_opts(max_items: usize) -> trace::TraceOptions {
    trace::TraceOptions { max_items: max_items.max(1), ..Default::default() }
}

fn parse_ok(source: &str) -> Program {
    parse_program(source, 0).program
}

/// 把 [`trace::Trace`] 序列化成前端字段。
pub fn trace_of_json(t: &trace::Trace) -> Json {
    Json::object(vec![
        field(
            "steps",
            Json::Arr(
                t.steps
                    .iter()
                    .map(|s| {
                        Json::object(vec![
                            field("seq", Json::Int(s.seq as i64)),
                            field("cell", Json::text(&s.cell)),
                            field("stmt", Json::Int(s.stmt as i64)),
                            field("line", Json::Int(s.line as i64)),
                            field("col", Json::Int(s.col as i64)),
                            field("kind", Json::text(s.kind.as_str())),
                            field("text", Json::text(&s.text)),
                            field(
                                "packet",
                                Json::Arr(
                                    s.packet
                                        .iter()
                                        .map(|p| {
                                            Json::object(vec![
                                                field("key", Json::text(&p.key)),
                                                field("value", Json::text(&p.value)),
                                                field("died", Json::Bool(p.died)),
                                            ])
                                        })
                                        .collect(),
                                ),
                            ),
                            field("group", Json::opt_str(&s.group)),
                            field("note", Json::opt_str(&s.note)),
                        ])
                    })
                    .collect(),
            ),
        ),
        field("notes", Json::strs(&t.notes)),
    ])
}

/// `/api/build` 的稳定产物名（`docs/05` P2-③a）。
///
/// **为什么这个名字值一整条缓存**：§16.2 的键按定义锁死**整次 rustc 调用**，而 `.rs` 的
/// 路径字符串与 `-o` 的目标名都会嵌进产物（PE 调试目录里的 `<name>.pdb`），所以产物名
/// 每变一次就必然 miss。原来的 `{build_prefix}-{n}` 让编辑器点一次 build 就是一个新键，
/// 「一键 build+run」这条终极目标 2 的主路径永远吃不到缓存。
const STABLE_STEM: &str = "last";

/// 选本次构建的产物名：稳定名能覆盖就用稳定名，否则退回递增名。
///
/// 返回的第二项是**要转告用户的那一行**（只在退用时是 `Some`）。退路必须留着：
/// 递增名原本绕开的是「运行中的 exe 不能被替换」，把名字固定死之后，如果上一次产物还在跑，
/// rustc 会直接失败。所以这里先探测再决定，而不是等 rustc 撞墙。
/// 探测失败的那一次读起来就是旧行为（递增名 + 必然重编），**不会**把一个写不进去的产物
/// 说成构建成功，也不会静默换名。
fn pick_stem(session: &Session, n: usize) -> (String, Option<String>) {
    let stable = format!("{}-{}", session.build_prefix, STABLE_STEM);
    let stable_exe = session.artifact_dir.join(format!("{}{}", stable, std::env::consts::EXE_SUFFIX));
    // 先探 exe 再探 pdb：正在运行的镜像会让 exe 打不开（共享冲突），而 pdb 通常不被占用；
    // 两个都探是因为只要有一个写不进去，rustc 那一次调用就会失败。
    let blocked = exe_replacement_blocked(&stable_exe)
        .or_else(|| exe_replacement_blocked(&stable_exe.with_extension("pdb")));
    match blocked {
        None => (stable, None),
        Some(why) => {
            let fresh = format!("{}-{}", session.build_prefix, n);
            (
                fresh.clone(),
                Some(format!(
                    "RSC: 稳定产物名 {} 写不进去（{why}），本次退回 {fresh}；\
                     增量缓存的键含产物名，所以退回的那一次会真的重编 rustc。\
                     下一次构建若产物已释放就会回到稳定名。",
                    stable_exe.display()
                )),
            )
        }
    }
}

/// 「这个文件位置下次能不能被覆盖」：不存在 → 能；存在但打不开（正在运行的 exe、
/// 被别的进程占住、或同名的目录）→ 返回那句失败原因。
fn exe_replacement_blocked(path: &Path) -> Option<String> {
    if !path.exists() {
        return None;
    }
    match std::fs::OpenOptions::new().write(true).open(path) {
        Ok(_) => None,
        Err(e) => Some(format!("{}, os error {:?}", e, e.kind())),
    }
}

/// `POST /api/build` → `{exit,rsPath,mapPath,exePath,stdout,stderr,mapped}`
///
/// 走**既有** [`run_build_with`]（含 §15.2 errmap 反映射），不复制流水线；
/// `sink` 收到的文本就是返回给前端的 `stderr`。
pub fn build_json(source: &str, mode: BuildMode, session: &Session) -> Json {
    let n = session.builds.fetch_add(1, Ordering::Relaxed) + 1;
    let (stem, fallback_note) = pick_stem(session, n);
    let src_path = session.artifact_dir.join(format!("{stem}.rscel"));
    let exe_path = session.artifact_dir.join(format!("{stem}{}", std::env::consts::EXE_SUFFIX));
    let rs_path = exe_path.with_extension("rs");
    let map_path = exe_path.with_extension("map.json");

    let mut buf = String::new();
    if let Some(note) = fallback_note {
        buf.push_str(&note);
        buf.push('\n');
    }
    let exit = match std::fs::write(&src_path, source) {
        Err(e) => {
            buf.push_str(&format!("RSC: cannot write {}: {e}\n", src_path.display()));
            3
        }
        Ok(()) => run_build_with(&src_path, &exe_path, mode, &mut |line| {
            buf.push_str(line);
            buf.push('\n');
        }),
    };

    // `stdout` 要真的**运行**产物再捕获（§16.1：build 的价值在于能跑出结果）。
    let mut run_stdout = String::new();
    if exit == 0 {
        match std::process::Command::new(&exe_path).output() {
            Ok(out) => {
                run_stdout = String::from_utf8_lossy(&out.stdout).into_owned();
                let run_stderr = String::from_utf8_lossy(&out.stderr).into_owned();
                if !run_stderr.is_empty() {
                    buf.push_str("// 产物运行期 stderr\n");
                    buf.push_str(&run_stderr);
                }
                if !out.status.success() {
                    buf.push_str(&format!("\n产物退出码 {:?}", out.status.code()));
                }
            }
            Err(e) => buf.push_str(&format!("RSC: 运行产物 {} 失败: {e}", exe_path.display())),
        }
    }

    let rs_text = std::fs::read_to_string(&rs_path).ok();
    let map_text = std::fs::read_to_string(&map_path).ok();
    {
        let mut art = session.artifacts();
        art.rs = rs_text;
        art.map = map_text;
        art.stdout = run_stdout.clone();
        art.stderr = buf.clone();
        art.exit = exit;
        art.built = true;
    }

    Json::object(vec![
        field("exit", Json::Int(exit as i64)),
        field("rsPath", Json::text(&rs_path.display().to_string())),
        field("mapPath", Json::text(&map_path.display().to_string())),
        field("exePath", Json::text(&exe_path.display().to_string())),
        field("stdout", Json::text(&run_stdout)),
        field("stderr", Json::text(&buf)),
        field("mapped", Json::Arr(mapped_entries(&buf))),
    ])
}

/// 把 sink 文本里的 RSC-E900 / RSC-E901 行结构化成前端的 `mapped` 列表。
///
/// **为什么解析文本而不是重跑一遍 rustc**：`sink` 是 `run_build_with` 唯一的可注入
/// 出口，想拿 rustc 的 NDJSON 就得复制整条流水线（两份会漂移的 rustc 调用 + srcmap
/// 读法）。这里只把「已反映射的成品文本」切三段，切坏的最多是 `rust_code` 为 null，
/// 不会丢诊断。
fn mapped_entries(stderr: &str) -> Vec<Json> {
    let mut out = Vec::new();
    for line in stderr.lines() {
        let (code, rest) = match line.split_once(": ") {
            Some(pair) => pair,
            None => continue,
        };
        if code != errmap::CODE_MAPPED && code != errmap::CODE_IN_EXTERN {
            continue;
        }
        out.push(Json::object(vec![
            field("code", Json::text(code)),
            field("detail", Json::text(rest)),
            field("rust_code", Json::opt_str(&rust_code_of(rest))),
        ]));
    }
    out
}

/// 从 `error[E0308]: ...` 里取 rustc 自己的码；没有则 `None`。
fn rust_code_of(text: &str) -> Option<String> {
    let start = text.find('[')?;
    let end = text[start..].find(']')? + start;
    let code = &text[start + 1..end];
    let ok = code.len() > 1
        && code.starts_with('E')
        && code[1..].chars().all(|c| c.is_ascii_digit());
    ok.then(|| code.to_string())
}

/// `POST /api/save` → `{ok,message,file}`；`Err(理由)` 由上层出 400。
pub fn save_json(source: &str, file: &str, session: &Session) -> Result<Json, String> {
    let target = save_target(file, session)?;
    std::fs::write(&target, source).map_err(|e| format!("写入 {} 失败: {e}", target.display()))?;
    Ok(Json::object(vec![
        field("ok", Json::Bool(true)),
        field("message", Json::text(&format!("已保存到 {}", target.display()))),
        field("file", Json::text(&target.display().to_string())),
    ]))
}

/// 路径白名单：**只允许**写「命令行指定的那个文件」或「工作目录直接下面的 `.rscel`」。
///
/// 这是真安全边界不是提示：浏览器里任何页面都能对 `127.0.0.1` 发 POST（本地服务
/// 本来就靠「不监听外网 + 校验来源」活着），没有这道限制就等于开了一个任意路径写文件
/// 的接口。逐条拒绝并把理由回给前端：
/// 1. 空文件名 → 仅当启动时带了文件才允许（存回它）；
/// 2. 含 NUL、含 `..` 段 → 直接拒。**不做**「先解析再看是否在目录内」的弱验证：
///    `..` 与 symlink 的组合在规范化之前是可绕的，先拒再算才闭环；
/// 3. 后缀必须 `.rscel`（大小写不敏感）；
/// 4. 绝对路径只允许等于启动时指定的那个文件；
/// 5. 相对路径拼上工作目录后，父目录必须**已存在**且 canonicalize 后**正好等于**
///    canonicalize 后的工作目录（等于就必然在其内，所以不再另做前缀判断；
///    canonicalize 解掉 symlink，这一步才真的把逃逸堵死）。
pub fn save_target(file: &str, session: &Session) -> Result<PathBuf, String> {
    if file.is_empty() {
        return match &session.file {
            Some(p) => Ok(p.clone()),
            None => Err("没有指定文件名，且本次会话不是从某个 .rscel 文件启动的".to_string()),
        };
    }
    if file.contains('\0') {
        return Err("路径含 NUL 字符".to_string());
    }
    let given = Path::new(file);
    if given.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("路径里不允许 `..`".to_string());
    }
    if !given.extension().map(|e| e.to_string_lossy().eq_ignore_ascii_case("rscel")).unwrap_or(false) {
        return Err(format!("只允许写 `.rscel` 文件：`{file}` 后缀不符"));
    }

    if given.is_absolute() {
        let allowed = session
            .file
            .as_deref()
            .ok_or_else(|| format!("拒绝绝对路径 `{file}`：本次会话没有指定可写文件"))?;
        if canonical(allowed)? == canonical(given)? {
            return Ok(given.to_path_buf());
        }
        return Err(format!("绝对路径只允许启动时指定的文件：{}", allowed.display()));
    }

    let joined = session.workdir.join(given);
    let parent = joined.parent().ok_or_else(|| format!("`{file}` 没有父目录"))?;
    let root = canonical(&session.workdir)?;
    let real_parent = canonical(parent)
        .map_err(|_| format!("目录不存在或不可解析：`{}`（只能保存到已存在目录下）", parent.display()))?;
    if real_parent != root {
        let root_text = root.display().to_string();
        return Err(format!("只允许写工作目录 {root_text} **直接**下面的 .rscel（实际解析到 {}）：拒绝", real_parent.display()));
    }
    Ok(joined)
}

fn canonical(p: &Path) -> Result<PathBuf, String> {
    std::fs::canonicalize(p).map_err(|e| format!("规范化路径 {} 失败: {e}", p.display()))
}

/// `GET /api/source?kind=rs|map|stdout` → 文本；没有产物时 `None`（上层出 404）。
pub fn artifact_text(session: &Session, kind: &str) -> Option<String> {
    let art = session.artifacts();
    if !art.built {
        return None;
    }
    match kind {
        "rs" => art.rs.clone(),
        "map" => art.map.clone(),
        "stdout" => Some(art.stdout.clone()),
        // 多给一个 stderr：curl 排障时最常问的就是「上次构建到底报了什么」。
        "stderr" => Some(art.stderr.clone()),
        other => Some(format!("RSC: 未知的 kind `{other}`；可用值 rs|map|stdout|stderr")),
    }
}

// ---------------------------------------------------------------- §16.4 报告

/// §16.4 的调用链路报告：默认文本缩进树，`--mermaid` 出 `graph TD`。
///
/// 直接复用 `rscel_check::topology`（**不再走一遍 gen**）；只在这里包一层
/// 「警告级风险点」—— §16.4 要求标注 force read with 于 POD 之类的风险，
/// 而那批判定是 check 的 W060/W002/W001 诊断，topology 有意不重复实现。
pub fn report(program: &Program, mermaid: bool) -> String {
    let topo = topology::topology_of(program);
    if mermaid {
        return topology::to_mermaid(&topo);
    }
    let mut s = topology::to_text_report(&topo);
    let warnings: Vec<String> = check_program(program)
        .diagnostics
        .into_iter()
        .filter(|d| d.severity == Severity::Warning)
        .map(|d| format!("RSC-W{:03}: {}；位置: {}", d.code, d.message, d.cell.unwrap_or_else(|| "<顶层>".to_string())))
        .collect();
    // 图上的形状风险（fire-and-forget 这类不是诊断的问题）也在同一节里给出来。
    let mut risks: Vec<String> = Vec::new();
    for n in &topo.nodes {
        for r in &n.risks {
            risks.push(format!("{}: {r}", n.name));
        }
    }
    s.push_str("\n风险点:\n");
    if warnings.is_empty() && risks.is_empty() {
        s.push_str("  无\n");
    }
    for w in &warnings {
        s.push_str(&format!("  {w}\n"));
    }
    for r in &risks {
        s.push_str(&format!("  ⚠ {r}\n"));
    }
    s
}

/// Cell 是否会生成 `async fn`：判据 = **透传 Rust 文本里出现 `.await`**
/// （只有真用了 await 的 Cell 才付 future 状态机的代价）。
///
/// 为什么不复用 gen 的判定：`rscel-gen` 的 codegen 正在被并发修改，而且为了一个
/// 展示字段去依赖生成器内部函数会把 cli → gen 的耦合面扩大。
pub fn needs_async(program: &Program, cell: &str) -> bool {
    program.cell(cell).map(|c| stmts_have_await(&c.body)).unwrap_or(false)
}

fn stmts_have_await(body: &[Stmt]) -> bool {
    body.iter().any(stmt_has_await)
}

fn stmt_has_await(s: &Stmt) -> bool {
    match s {
        Stmt::RawRust(r) => r.text.contains(".await"),
        Stmt::ExternRustBlock { body, .. } => body.text.contains(".await"),
        Stmt::RustBlock { block } => block.parts.iter().any(|p| match p {
            RawBlockPart::Text(t) => t.text.contains(".await"),
            RawBlockPart::Stmt(inner) => stmt_has_await(inner),
        }),
        _ => false,
    }
}

/// 给 `rscel trace` / `/api/trace` 的 help 文本与 serve 路由用的默认上限。
pub const DEFAULT_MAX_ITEMS: usize = trace::DEFAULT_MAX_ITEMS;

#[cfg(test)]
mod tests {
    use super::*;

    const LEGAL: &str = "cell main;\nreceive();\njump a go with(1 -> x);\n\ncell a;\nreceive(x: i64);\nexit go with(x -> y);\n";

    fn session_in(dir: PathBuf) -> Session {
        Session {
            file: None,
            source: String::new(),
            workdir: dir.clone(),
            artifact_dir: dir,
            build_prefix: "t".to_string(),
            builds: AtomicUsize::new(0),
            artifacts: Mutex::new(Artifacts::default()),
        }
    }

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rscel-api-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn stable_artifact_name_is_used_while_the_previous_one_is_replacable() {
        let dir = tempdir("stem-stable");
        // 上一批测试可能留着一个真产物在同一个临时目录里，先清掉才能判"首次构建"。
        let _ = std::fs::remove_file(dir.join("t-last.exe"));
        let _ = std::fs::remove_dir_all(dir.join("t-last.exe"));
        let session = session_in(dir.clone());
        let (stem, note) = pick_stem(&session, 3);
        assert_eq!(stem, "t-last", "能覆盖就必须用稳定名（缓存键含产物名）");
        assert!(note.is_none(), "没退路就不该有说明：{note:?}");
    }

    #[test]
    fn an_unwritable_stable_exe_falls_back_to_the_incrementing_name_and_says_why() {
        let dir = tempdir("stem-locked");
        let blocker = dir.join("t-last.exe");
        let _ = std::fs::remove_file(&blocker);
        // 用一个同名**目录**当"这个位置写不进去"的证据：以写方式打开目录在两个平台上都失败，
        // 而它不需要真起一个进程把 exe 占住 —— 后者会因调度先后而随机红。
        std::fs::create_dir_all(&blocker).unwrap();
        let session = session_in(dir.clone());
        let (stem, note) = pick_stem(&session, 7);
        assert_eq!(stem, "t-7", "写不进去时必须退回递增名，而不是让 rustc 撞墙");
        let note = note.expect("退回必须说清楚，不能静默换名");
        assert!(note.contains("写不进去") && note.contains("重编 rustc"), "{note}");
        std::fs::remove_dir(&blocker).unwrap();
    }

    #[test]
    fn check_json_reports_dsl_line_not_generated_rust_line() {
        // `jump ghost` 的目标不存在（E042 类），诊断必须落在源文件第 3 行。
        let bad = "cell main;\nreceive();\njump ghost go with(1 -> x);\n";
        let text = check_json(bad).to_pretty();
        assert!(text.contains("\"errors\": 1"), "{text}");
        assert!(text.contains("\"line\": 3"), "必须给 DSL 行号：{text}");
        assert!(text.contains("\"col\": 1"), "{text}");
        assert!(text.contains("ghost"), "{text}");
    }

    #[test]
    fn check_json_legal_program_has_zero_errors_and_counts_cells() {
        let text = check_json(LEGAL).to_pretty();
        assert!(text.contains("\"errors\": 0"), "{text}");
        assert!(text.contains("\"cells\": 2"), "{text}");
        assert!(text.contains("\"diagnostics\": []"), "{text}");
    }

    #[test]
    fn graph_json_field_names_match_editor_contract() {
        let text = graph_json(LEGAL).to_pretty();
        for key in [
            "\"entry\"", "\"nodes\"", "\"edges\"", "\"groups\"", "\"name\"", "\"line\"", "\"cols\"",
            "\"receive\"", "\"outputs\"", "\"is_entry\"", "\"needs_async\"", "\"terminal\"", "\"kind\"",
            "\"mode\"", "\"keys\"", "\"group\"", "\"limit\"", "\"cond\"", "\"over\"", "\"stmt\"",
        ] {
            assert!(text.contains(key), "缺字段 {key}:\n{text}");
        }
        assert!(text.contains("\"to\": \"a\""), "{text}");
        assert!(text.contains("\"kind\": \"plain\""), "{text}");
    }

    #[test]
    fn trace_json_exposes_the_documented_step_shape() {
        let text = trace_json(LEGAL, 4).to_pretty();
        for key in ["\"steps\"", "\"seq\"", "\"cell\"", "\"packet\"", "\"died\"", "\"notes\"", "\"kind\"", "\"value\""] {
            assert!(text.contains(key), "缺字段 {key}:\n{text}");
        }
        assert!(text.contains("\"kind\": \"exit\""), "{text}");
        assert!(text.contains("\"seq\": 1"), "{text}");
    }

    #[test]
    fn trace_of_unparsable_source_says_so_instead_of_empty_success() {
        let t = trace::trace_program(&parse_program("cell main;\nreceive(", 0).program, &trace_opts(4));
        let json = trace_of_json(&t).to_pretty();
        assert!(json.contains("\"steps\": []"), "{json}");
        // 空 steps + 空 notes 就是「静默降级」，这条断言把它钉死。
        assert!(!t.notes.is_empty(), "parse 失败必须有说明");
    }

    #[test]
    fn rust_code_extraction_and_mapped_structuring() {
        assert_eq!(rust_code_of("error[E0308]: mismatched types").as_deref(), Some("E0308"));
        assert_eq!(rust_code_of("error: aborting due to 1 previous error"), None);
        assert_eq!(rust_code_of("note: [not a code]"), None);
        let line = format!(
            "{}: rustc 编译错误（映射自 DSL 位置 cell fact @ 3:1）: error[E0308]: mismatched types；生成位置: f.rs:9:1",
            errmap::CODE_MAPPED
        );
        let entries = mapped_entries(&format!("{line}\n无关行\n"));
        assert_eq!(entries.len(), 1);
        let text = Json::Arr(entries).to_pretty();
        assert!(text.contains("\"E0308\""), "{text}");
        assert!(text.contains("mismatched types"), "{text}");
    }

    #[test]
    fn save_target_allows_cwd_rscel_and_rejects_every_escape() {
        let dir = tempdir("save");
        let session = session_in(dir.clone());
        let ok = save_target("hello.rscel", &session).expect("工作目录下的 .rscel 必须允许");
        assert_eq!(ok, dir.join("hello.rscel"));

        for bad in [
            "../evil.rscel",
            "sub/../evil.rscel",
            "..\\evil.rscel",
            "notes.txt",
            "a.rscel.bak",
            "no-suffix",
            "x\0.rscel",
            "UPPER.RSCeL", // 大小写不敏感 → 允许，所以不放这里
        ] {
            let r = save_target(bad, &session);
            if bad == "UPPER.RSCeL" {
                assert!(r.is_ok(), "{bad} 应当被允许（后缀大小写不敏感）");
            } else {
                assert!(r.is_err(), "必须拒绝 {bad}");
            }
        }
        // 绝对路径越界
        assert!(save_target(dir.join("..").join("outside.rscel").to_str().unwrap(), &session).is_err());
        // 空文件名且没有 --file
        assert!(save_target("", &session).is_err());
        // 子目录（工作目录**直接**下面之外一律拒绝）
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        assert!(save_target("sub/x.rscel", &session).is_err(), "子目录不在白名单里");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_target_allows_only_the_file_named_on_the_command_line() {
        let dir = tempdir("save2");
        let named = dir.join("named.rscel");
        std::fs::write(&named, "x").unwrap();
        let mut session = session_in(dir.join("workdir"));
        std::fs::create_dir_all(&session.workdir).unwrap();
        session.file = Some(named.clone());
        assert_eq!(save_target("", &session).unwrap(), named);
        assert_eq!(save_target(named.to_str().unwrap(), &session).unwrap(), named);
        assert!(save_target(dir.join("other.rscel").to_str().unwrap(), &session).is_err(), "别的绝对路径必须拒");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn needs_async_only_when_await_appears_in_passthrough() {
        let plain = parse_program(LEGAL, 0).program;
        assert!(!needs_async(&plain, "main"));
        assert!(!needs_async(&plain, "nope"));
        let held = parse_program("cell main;\nreceive();\nlet x = f().await;\ndie;\n", 0).program;
        assert!(needs_async(&held, "main"));
        // 块内的 await 同样算（§3.5.2 的嵌套透传）。
        let nested = parse_program("cell main;\nreceive();\nif a { let x = f().await; }\ndie;\n", 0).program;
        assert!(needs_async(&nested, "main"));
    }

    #[test]
    fn report_text_and_mermaid_are_both_produced_from_topology_only() {
        let p = parse_program(LEGAL, 0).program;
        let text = report(&p, false);
        assert!(text.contains("入口: main"), "{text}");
        assert!(text.contains("风险点:"), "{text}");
        let m = report(&p, true);
        assert!(m.starts_with("graph TD"), "{m}");
        assert!(m.contains("-->"), "{m}");
    }

    #[test]
    fn initial_json_gives_file_and_source() {
        let dir = tempdir("init");
        let mut session = session_in(dir.clone());
        session.source = "cell main;\nreceive();\ndie;\n".to_string();
        let text = initial_json(&session).to_pretty();
        assert!(text.contains("\"file\": null"), "{text}");
        assert!(text.contains("cell main"), "{text}");
        session.file = Some(dir.join("x.rscel"));
        assert!(initial_json(&session).to_pretty().contains("x.rscel"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn artifact_text_requires_a_completed_build() {
        let dir = tempdir("art");
        let session = session_in(dir.clone());
        assert_eq!(artifact_text(&session, "rs"), None, "没构建过就没有产物");
        {
            let mut art = session.artifacts();
            art.built = true;
            art.rs = Some("fn main() {}".to_string());
            art.stdout = "120\n".to_string();
        }
        assert_eq!(artifact_text(&session, "rs").as_deref(), Some("fn main() {}"));
        assert_eq!(artifact_text(&session, "stdout").as_deref(), Some("120\n"));
        assert!(artifact_text(&session, "bogus").unwrap().contains("未知的 kind"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
