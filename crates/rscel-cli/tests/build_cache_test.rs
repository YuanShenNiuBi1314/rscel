//! §16.2 增量缓存的端到端验收（`crates/rscel-cli/src/build_cache.rs`）。
//!
//! 这里跑的**全部是真 rustc、真 codegen、真产物运行**：缓存这种"跳过一步"的功能，
//! 静态字符串断言完全无力（它对生成物文本一个字都不改），唯一能证伪的方式是
//! 「命中 / 不命中两条路径各自把产物跑一遍，比 stdout」。
//!
//! 每个用例用自己的工作目录：`cache_root` 是"从源文件目录向上找"，共用了目录就会
//! 共用缓存根，于是 `prune` 的条目上限会让**另一个**用例的命中判定翻掉（那是假阴性）。

use rscel_cli::build_cache::CachePolicy;
use rscel_cli::{run_build_with_policy, BuildMode};
use std::path::{Path, PathBuf};

/// 附录 A.1 的阶乘（与 `crates/rscel-gen/examples/factorial_loop.rscel` 同形，
/// 只是把 `five` 的字面量留在这里方便用例改它）。运行必须打印 120（§19 M1 验收）。
const FACTORIAL: &str = "\
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
";

/// 只多一行注释的同一份 DSL：词法就把注释丢掉，所以**生成物逐字不变**。
const FACTORIAL_WITH_COMMENT: &str = "\
// 这一行注释不影响生成物，所以它也不该让缓存失效。
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
";

/// 生成物里会留下一条 rustc `unused_variables` 告警的 DSL。
/// 用它验「命中时告警文本照样端得出来」—— `scripts/bench/run.sh` 判"生成物零告警"
/// 正是 grep 这份文本，缓存绝不能把这条回归信号吞掉。
const WITH_RUSTC_WARNING: &str = "\
cell main;
receive();
extern rust { let unused_box = 1i64; }
exit go with();
";

fn work_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rscel-cli-cache-{tag}"));
    // 上一轮的条目必须清掉，否则"第一次就命中"会把 miss 路径的断言全骗过去。
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Build {
    code: i32,
    text: String,
}

impl Build {
    fn hit(&self) -> bool {
        self.text.contains("增量缓存命中")
    }
}

fn build(dir: &Path, name: &str, src: &str, mode: BuildMode, policy: CachePolicy) -> Build {
    let file = dir.join(format!("{name}.rscel"));
    std::fs::write(&file, src).unwrap();
    let out = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    let mut buf = String::new();
    let code = run_build_with_policy(&file, &out, mode, policy, &mut |line| {
        buf.push_str(line);
        buf.push('\n');
    });
    Build { code, text: buf }
}

/// 真跑产物，返回 stdout（调试模式另有 `[rsc] …` 走 stderr，这里也要能拿到）。
fn run_exe(dir: &Path, name: &str) -> (String, String) {
    let out = std::process::Command::new(dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
        .output()
        .unwrap_or_else(|e| panic!("运行 {} 失败: {e}", name));
    assert!(out.status.success(), "{name} 退出码非零: {:?}", out.status.code());
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn intermediates(dir: &Path, name: &str) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let stem = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    (
        std::fs::read(stem.with_extension("rs")).unwrap(),
        std::fs::read(stem.with_extension("map.json")).unwrap(),
        std::fs::read(&stem).unwrap(),
    )
}

/// 缓存里第一个**叶子条目**目录（用例只放一份 DSL 进去，所以就是唯一那个）。
///
/// 两级键（`docs/05` P2-③b）之后条目住在 `根/{不含版本的身份}/{版本的索引}` 两层里，
/// 所以这里按"哪一格真的放着 `identity.bin`"来找，不假定深度 —— 顺带把 `v1` 的
/// 扁平格子也认得（它同样放着 identity.bin）。
fn first_entry(dir: &Path) -> PathBuf {
    let root = dir.join(".rscel-cache");
    let mut found: Vec<PathBuf> = Vec::new();
    let tops = std::fs::read_dir(&root).unwrap_or_else(|e| panic!("{} 不存在: {e}", root.display()));
    for top in tops.flatten().map(|d| d.path()) {
        if top.join("identity.bin").is_file() {
            found.push(top);
            continue;
        }
        let Ok(sub) = std::fs::read_dir(&top) else { continue };
        for leaf in sub.flatten().map(|d| d.path()) {
            if leaf.join("identity.bin").is_file() {
                found.push(leaf);
            }
        }
    }
    assert!(!found.is_empty(), "缓存里一个条目都没有，说明 store 没跑成");
    found.sort();
    found.remove(0)
}

/// 本进程视角的 `rustc -vV` 全文（与 `build_cache::rustc_version` 同一口径：lossy UTF-8）。
fn rustc_version_text() -> String {
    let out = std::process::Command::new("rustc")
        .arg("-vV")
        .output()
        .expect("这一条要真跑 rustc -vV，rustc 不在 PATH 上就测不了");
    assert!(out.status.success(), "rustc -vV 退出码非零: {:?}", out.status.code());
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// 两级键**不许放松**的那一半：条目根不看版本，但命中仍要求 `identity.bin` 里的版本帧
/// 与本次 `rustc -vV` 逐字节相等。
///
/// 这就是"换工具链不可能吃旧产物"的可执行形式 —— 不必真装两套 rustc：把条目里**存着的**
/// 版本改一个字符（同长度，所以只动内容、不动分帧），等价于"这条目来自另一个工具链"。
/// 改完必须 miss，而**还原之后必须又命中** —— 后半句排除掉"其实是摘要/缺文件之类的
/// 顺带失效让它没命中"这种假阳性（那种情况下还原也命不中）。
#[test]
fn the_version_is_verified_byte_for_byte_even_though_the_index_ignores_it() {
    let dir = work_dir("version");
    let cold = build(&dir, "ver", FACTORIAL, BuildMode::Debug, CachePolicy::Enabled);
    assert_eq!(cold.code, 0, "{}", cold.text);

    let entry = first_entry(&dir);
    let id_path = entry.join("identity.bin");
    let original = std::fs::read(&id_path).unwrap();
    let version = rustc_version_text();
    assert!(
        original.windows(version.len()).any(|w| w == version.as_bytes()),
        "identity.bin 里找不到本次 `rustc -vV` 全文 —— 版本被从验证里拿掉了，\
         这正是 P2-③b 明令不许的那样"
    );
    // 条目根本身是个不含版本的 16 位索引：冷构建能跳过那次 52–67ms 的 spawn 全靠这一点。
    let base = entry.parent().unwrap();
    assert_eq!(
        base.file_name().unwrap().to_string_lossy().len(),
        16,
        "条目没落在 {{不含版本的根}}/{{版本的索引}} 这两层里:\n{}",
        base.display()
    );

    let hot = build(&dir, "ver", FACTORIAL, BuildMode::Debug, CachePolicy::Enabled);
    assert!(hot.hit(), "同一条命令行第二次该命中:\n{}", hot.text);

    let at = original
        .windows(version.len())
        .position(|w| w == version.as_bytes())
        .expect("上面刚断言过版本全文在 identity.bin 里");
    let mut tampered = original.clone();
    tampered[at] = if tampered[at] == b'7' { b'8' } else { b'7' };
    std::fs::write(&id_path, &tampered).unwrap();

    let after = build(&dir, "ver", FACTORIAL, BuildMode::Debug, CachePolicy::Enabled);
    assert_eq!(after.code, 0, "{}", after.text);
    assert!(
        !after.hit(),
        "条目里存的版本与本次 rustc -vV 不同，却还是端出了命中 —— 版本没被逐字节验证:\n{}",
        after.text
    );
    let (stdout_after, _) = run_exe(&dir, "ver");
    assert!(stdout_after.contains("120"), "退成重建之后产物必须照常能跑，实际 {stdout_after:?}");

    std::fs::write(&id_path, &original).unwrap();
    let restored = build(&dir, "ver", FACTORIAL, BuildMode::Debug, CachePolicy::Enabled);
    assert!(restored.hit(), "还原身份后仍不命中，说明上面那次 miss 不在版本验证上:\n{}", restored.text);
}

/// 核心等价性：命中与否，`.rs` 与 `.map.json` **逐字相同**，产物跑出来也相同。
///
/// 这是 `docs/05` P2 ③ 给这一条钉的死要求。另外顺手钉住"命中就是上一次的同一份字节"
/// （不是"看起来差不多"），以及 `--no-cache` 既不读也不写。
#[test]
fn cached_and_uncached_builds_agree_on_every_intermediate() {
    let dir = work_dir("agree");
    // 冷构建：不该有命中话术，产物必须能跑出 120。
    let cold = build(&dir, "agree", FACTORIAL, BuildMode::Debug, CachePolicy::Enabled);
    assert_eq!(cold.code, 0, "{}", cold.text);
    assert!(!cold.hit(), "第一次构建不该命中:\n{}", cold.text);
    let (rs_cold, map_cold, exe_cold) = intermediates(&dir, "agree");
    let (stdout_cold, _) = run_exe(&dir, "agree");
    assert!(stdout_cold.contains("120"), "冷构建产物该打印 120，实际 {stdout_cold:?}");

    // 命中：`.rs` / `.map.json` / 产物字节三者都与冷构建一致，且 stdout 一致。
    let hot = build(&dir, "agree", FACTORIAL, BuildMode::Debug, CachePolicy::Enabled);
    assert_eq!(hot.code, 0, "{}", hot.text);
    assert!(hot.hit(), "第二次同样的构建必须命中:\n{}", hot.text);
    let (rs_hot, map_hot, exe_hot) = intermediates(&dir, "agree");
    assert_eq!(rs_hot, rs_cold, "命中与否生成的 .rs 必须逐字相同");
    assert_eq!(map_hot, map_cold, "命中与否 .map.json 必须逐字相同");
    assert_eq!(exe_hot, exe_cold, "命中就是同一份字节，不该有任何出入");
    let (stdout_hot, stderr_hot) = run_exe(&dir, "agree");
    assert_eq!(stdout_hot, stdout_cold, "命中产物的 stdout 与冷构建不同");
    // 这条只在 Debug 档成立：`--debug` 的埋点在 stderr 上，命中产物也得带着它。
    assert!(stderr_hot.contains("[rsc]"), "Debug 档命中产物应当仍带 §15.3 埋点输出");

    // `--no-cache`：不读（所以不说命中），也不写。
    let nocc = build(&dir, "agree", FACTORIAL, BuildMode::Debug, CachePolicy::Disabled);
    assert_eq!(nocc.code, 0, "{}", nocc.text);
    assert!(!nocc.hit(), "--no-cache 不该命中:\n{}", nocc.text);
    let (rs_no, map_no, _) = intermediates(&dir, "agree");
    assert_eq!(rs_no, rs_cold, "--no-cache 的 .rs 与缓存路径也必须逐字相同");
    assert_eq!(map_no, map_cold);
    let (stdout_no, _) = run_exe(&dir, "agree");
    assert_eq!(stdout_no, stdout_cold);
}

/// `--no-cache` 连缓存根都不该创建：它必须真的"绕过"，而不是"绕过读取但仍写盘"。
#[test]
fn no_cache_does_not_create_the_cache_directory() {
    let dir = work_dir("nocache");
    let b = build(&dir, "nc", FACTORIAL, BuildMode::Debug, CachePolicy::Disabled);
    assert_eq!(b.code, 0, "{}", b.text);
    assert!(!dir.join(".rscel-cache").exists(), "--no-cache 却还是建了缓存目录");
}

/// 源码里真正影响生成物的那个字面量变了，就绝不能吃旧产物（5! → 6!）。
#[test]
fn changed_source_literal_never_serves_the_old_artifact() {
    let dir = work_dir("changed");
    let first = build(&dir, "chg", FACTORIAL, BuildMode::Debug, CachePolicy::Enabled);
    assert_eq!(first.code, 0, "{}", first.text);
    let (stdout_first, _) = run_exe(&dir, "chg");
    assert!(stdout_first.contains("120"));

    let six = FACTORIAL.replace("let five: i64 = 5;", "let five: i64 = 6;");
    let second = build(&dir, "chg", &six, BuildMode::Debug, CachePolicy::Enabled);
    assert_eq!(second.code, 0, "{}", second.text);
    assert!(!second.hit(), "源码变了还命中，就是把旧产物端给了新用户:\n{}", second.text);
    let (stdout_second, _) = run_exe(&dir, "chg");
    assert!(stdout_second.contains("720"), "改完该跑 720，实际 {stdout_second:?}");
}

/// Release 档：身份记的是**生成物字节**而不是 DSL 文本，所以只加注释的改动照样命中。
///
/// 这条同时反证了"按源文件哈希建键"的做法 —— 那会让每次编辑都失效，包括与产物无关的编辑。
/// 并且命中时 `.map.json` 仍是**本次实时产生**的：映射表里是新的 DSL 行号，
/// 反映射不会因为吃了缓存而停在旧位置上（这正是"前端一行都不缓存"换来的性质）。
#[test]
fn comment_only_edit_hits_in_release_while_the_map_is_regenerated() {
    let dir = work_dir("comment");
    let cold = build(&dir, "cmt", FACTORIAL, BuildMode::Release, CachePolicy::Enabled);
    assert_eq!(cold.code, 0, "{}", cold.text);
    let (rs_cold, map_cold, _) = intermediates(&dir, "cmt");

    let hot = build(&dir, "cmt", FACTORIAL_WITH_COMMENT, BuildMode::Release, CachePolicy::Enabled);
    assert!(hot.hit(), "只加注释的改动不改生成物，不该让缓存失效:\n{}", hot.text);
    let (rs_hot, map_hot, _) = intermediates(&dir, "cmt");
    assert_eq!(rs_hot, rs_cold, "命中路径的 .rs 必须与冷构建逐字相同");
    assert_ne!(map_hot, map_cold, "映射表该是本次实时生成的（DSL 行号整体下移一行）");
    assert!(String::from_utf8_lossy(&map_hot).contains("\"line\": 2"), "新映射表里 cell main 该在第 2 行:\n{}", String::from_utf8_lossy(&map_hot));
    let (stdout_hot, _) = run_exe(&dir, "cmt");
    assert!(stdout_hot.contains("120"));
}

/// Debug 档：同一处注释改动**必须 miss**。
///
/// §15.3 的埋点把 DSL 的 `行:列` 印成产物里的字符串常量，所以位置一变生成物就变。
/// 这不是缺陷，而是"身份 = 生成物字节"的自洽结果；钉住它是为了防止后来者为了"让它命中"
/// 把键换成源文件哈希之类更粗的口径 —— 那会同时放过真正的失效面。
#[test]
fn comment_only_edit_misses_in_debug_because_positions_are_baked_into_the_trace_lines() {
    let dir = work_dir("comment-dbg");
    let cold = build(&dir, "cmt", FACTORIAL, BuildMode::Debug, CachePolicy::Enabled);
    assert_eq!(cold.code, 0, "{}", cold.text);
    let hot = build(&dir, "cmt", FACTORIAL_WITH_COMMENT, BuildMode::Debug, CachePolicy::Enabled);
    assert!(!hot.hit(), "Debug 档埋点里带 DSL 行列，注释改动改变了生成物，本该 miss:\n{}", hot.text);
    let (stdout_hot, stderr_hot) = run_exe(&dir, "cmt");
    assert!(stdout_hot.contains("120"));
    assert!(stderr_hot.contains("spawn @ 2:1"), "埋点该跟着新行号走，实际:\n{stderr_hot}");
}

/// Debug 与 Release 是两条不同的 rustc 命令行，绝不能互相吃对方的产物。
///
/// 断言用的是**可观察行为**：Debug 档带 §15.3 埋点（stderr 有 `[rsc]`），Release 档一行都不该有
/// —— 这正是 `docs/05` 里"release 生成物逐字不变"那条不变式的运行期版本。
#[test]
fn debug_and_release_invocations_do_not_share_entries() {
    let dir = work_dir("modes");
    let dbg = build(&dir, "mode", FACTORIAL, BuildMode::Debug, CachePolicy::Enabled);
    assert_eq!(dbg.code, 0, "{}", dbg.text);
    let (stdout_dbg, stderr_dbg) = run_exe(&dir, "mode");
    assert!(stdout_dbg.contains("120"));
    assert!(stderr_dbg.contains("[rsc]"), "Debug 档产物应带埋点，实际 {stderr_dbg:?}");

    let rel = build(&dir, "mode", FACTORIAL, BuildMode::Release, CachePolicy::Enabled);
    assert_eq!(rel.code, 0, "{}", rel.text);
    assert!(!rel.hit(), "换了 rustc flag 还命中，就是拿 -O0 的产物冒充 -O3:\n{}", rel.text);
    let (stdout_rel, stderr_rel) = run_exe(&dir, "mode");
    assert!(stdout_rel.contains("120"));
    assert!(!stderr_rel.contains("[rsc]"), "Release 档产物不该有埋点输出:\n{stderr_rel}");

    // 同一条命令行再来一次才该命中（而且命中的是 Release 那一格）。
    let rel2 = build(&dir, "mode", FACTORIAL, BuildMode::Release, CachePolicy::Enabled);
    assert!(rel2.hit(), "同一条命令行第二次必须命中:\n{}", rel2.text);
    let (_, stderr_rel2) = run_exe(&dir, "mode");
    assert!(!stderr_rel2.contains("[rsc]"), "命中的产物仍须是 Release 形状");
}

/// 条目里的产物被外部截断（身份没变、只有 `exe.bin` 坏了）时必须退成**重建**，
/// 而不是把跑不起来的字节复制到用户手上。这是 `digest.txt` 存在的全部理由。
#[test]
fn truncated_cache_entry_degrades_to_a_rebuild() {
    let dir = work_dir("truncated");
    let cold = build(&dir, "trc", FACTORIAL, BuildMode::Debug, CachePolicy::Enabled);
    assert_eq!(cold.code, 0, "{}", cold.text);
    let entry = first_entry(&dir);
    let exe_path = entry.join("exe.bin");
    let original = std::fs::read(&exe_path).unwrap();
    assert!(original.len() > 3, "产物短得没法做截断实验");
    std::fs::write(&exe_path, &original[..3]).unwrap();

    let after = build(&dir, "trc", FACTORIAL, BuildMode::Debug, CachePolicy::Enabled);
    assert_eq!(after.code, 0, "{}", after.text);
    assert!(!after.hit(), "摘要不匹配却还说命中，就是代码没在验产物:\n{}", after.text);
    let (stdout_after, _) = run_exe(&dir, "trc");
    assert!(stdout_after.contains("120"), "重建后的产物必须照常能跑，实际 {stdout_after:?}");
}

/// 命中必须把 rustc 的告警**原样重放**。基准台靠这份文本判"生成物零告警"，
/// 缓存把信号吞掉就等于把 `docs/05` 里那条"放过告警=扔掉回归信号"的教训复刻一遍。
#[test]
fn rustc_warnings_are_replayed_on_a_hit() {
    let dir = work_dir("warning");
    let cold = build(&dir, "warn", WITH_RUSTC_WARNING, BuildMode::Debug, CachePolicy::Enabled);
    assert_eq!(cold.code, 0, "{}", cold.text);
    assert!(cold.text.contains("warning[unused_variables]"), "冷构建该有那条告警:\n{}", cold.text);
    // `run.sh` 判零告警用的就是这三个 grep 模式之一，这里照它验一遍。
    assert!(cold.text.contains("warning["), "grep 模式 warning[ 应当命中");

    let hot = build(&dir, "warn", WITH_RUSTC_WARNING, BuildMode::Debug, CachePolicy::Enabled);
    assert!(hot.hit(), "第二次应当命中:\n{}", hot.text);
    let cold_line = cold
        .text
        .lines()
        .find(|l| l.contains("warning[unused_variables]"))
        .unwrap()
        .to_string();
    let hot_line = hot
        .text
        .lines()
        .find(|l| l.contains("warning[unused_variables]"))
        .expect("命中路径把 rustc 告警弄丢了 —— 基准台的零告警检查会因此变成摆设");
    assert_eq!(hot_line, cold_line, "重放的告警文本与冷构建不一致");
}

/// 构建失败（rustc 非零退出）**不进缓存**：下一次同样的构建仍然要真跑，
/// 而报出来的诊断条数不许因为"缓存了失败"而变化。
#[test]
fn a_failing_build_is_never_cached() {
    let dir = work_dir("failing");
    // `out(...)` 引用一个不存在的绑定 → 类型推不出来 → 生成物里是 compile_error!，rustc 非零退出。
    // （用真 rustc 失败，而不是编一个假退出码：这条要证的就是"失败不落盘"。）
    let broken = FACTORIAL.replace("out(next_acc);", "out(no_such_value);");
    let first = build(&dir, "fail", &broken, BuildMode::Debug, CachePolicy::Enabled);
    assert_ne!(first.code, 0, "这份 DSL 本该构建失败，输出:\n{}", first.text);
    assert!(!first.text.contains("增量缓存"), "失败的构建不该碰缓存话术:\n{}", first.text);
    let second = build(&dir, "fail", &broken, BuildMode::Debug, CachePolicy::Enabled);
    assert_eq!(second.code, first.code, "两次同样失败的退出码不一致");
    assert!(!second.text.contains("增量缓存"), "第二次同样失败，且不许命中");
}
