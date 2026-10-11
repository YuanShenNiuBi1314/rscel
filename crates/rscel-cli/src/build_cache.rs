//! §16.2 增量缓存 —— 把 `rscel build` 里唯一贵的那一段（rustc）做成内容寻址缓存。
//!
//! # 为什么缓存的是 rustc 产物，不是 §16.2 字面写的「每 Cell 文本哈希」
//!
//! 2026-10-11 实测（入库台 `bash scripts/bench/buildtime.sh`，release 档编译器、同一台机器、
//! N=7 与 N=9 两窗交错、判定值取中位数，两窗同号；数字全表在 `docs/05` P2 ③）：
//! `rscel build` 一次 373–935ms，其中**单独把同一份 `.rs` 交给同 flag 的 rustc**就要
//! 366–918ms；整个前端（parse + check + gen + `.rs`/`.map.json` 落盘 + 映射表自校验 + 反映射）
//! 只占 **1.4%–2.9%**（中位 7.0–14.7ms），而进程启动本身就要 24–25ms（这个台的每次 spawn
//! 都带 MSYS 的 fork 开销；换 node 台量是 9–10ms，前端占比 0.3%–3.4% —— **绝对值不跨台混比**，
//! 两台一致的是"前端 <3.5%"这一句）。min 口径下「build 减 rustc」还可以是**负数**
//! （窗 2 的 `read_share`：cold min 921.4 − rustc max 929.8 = −8.4ms）→ 前端总量低于台子分辨率。
//! 所以按 §16.2 字面做 `<file>.ast.json` / `<cell>.cfc.json` / `<cell>.gen.rs` 三层缓存，
//! 上限只能省下这不到 3 个点，还要背上「Cell 变化级联失效其反向调用边」这套失效逻辑；
//! 而它跟 P0-4 的实体展开直接冲突（展开的意义就是消掉 Cell 边界，按 Cell 拆片段会把边界请回来）。
//! → 本模块只缓存 rustc 的产物：**省下的是构建耗时的 97%–99%（实测加速 4.5x–11.5x）**；
//!   前端一行都不缓存，于是诊断永远实时产生，根本不存在"缓存把旧诊断端给用户"这条失效面。
//!
//! # 键为什么必须锁死整次调用
//!
//! 实测（同一份 `pipeline_and.rs`、同一组 flag、两次独立 rustc 调用）：
//! - 同一 `-o` 名：产物差 **21 字节** = 链接时间戳（偏移 241 / 153221 / 153249 / 153277）
//!   + 16 字节 PDB GUID（153593 起）→ 机器码与文件大小逐字相同；
//! - 换 `-o` 名：产物差 **762 字节** → PE 的调试目录里嵌了 `name_A.pdb` 这种**输出文件名**，
//!   而 `.rs` 的**路径字符串**也原样嵌在产物里（`strings` 可见）。
//!
//! 结论：`.rs` 路径与输出路径都是产物内容的一部分，所以二者都进键（漏掉任何一个，
//! 就是把"另一条命令行的产物"端给用户）。这条约束曾经让 `rscel edit` 那条路径**拿不到命中**
//! （`api::build_json` 每次把产物名递增，为的是绕开"运行中的 exe 不能被替换"，
//! 于是点一次 build 就是一个新键）—— 已按 `docs/05` P2-③a 修掉：编辑器改用稳定名
//! `<prefix>-last`，只有在那个位置真写不进去时才退回递增名（退回的那一次按定义就是 miss，
//! 并且会把原因打到 stderr）。**键的设计没动**，动的是调用方给什么名字。
//!
//! # 两级键：版本参与验证，但不参与索引（`docs/05` P2-③b）
//!
//! 版本全文（`rustc -vV`）以前是**串行**查的，而那次 spawn 自己就要 52–67ms
//! （实测构成见 `docs/05` P2 ③ 的代价那一段）—— 它排在 lookup 之前，所以带缓存的冷构建
//! 比 `--no-cache` 白贵一整次进程启动。修法不是省掉查询，而是**只在有东西可验时才串行付**：
//!
//! - 条目根目录名 = 「**不含版本**的身份」的索引；
//! - 叶子目录名 = 版本全文的索引，`identity.bin` 里**仍然带版本全文**，
//!   命中判定依旧要求整段身份 blob **逐字节相等**（所以换工具链仍然不可能吃旧产物 ——
//!   这一条是被测过的，见 `identity_bin_carries_the_version_and_rejects_a_swapped_one`）；
//! - 条目根不存在 ⇒ 这次调用从没编过，**连版本都不用查**：直接冷构建，并把 `rustc -vV`
//!   非阻塞地发出去与编译**重叠**跑，编译完再收来写条目。
//!
//! 不做的两件事，理由是硬的：不许把版本从验证里拿掉（上面那条测试钉着）；也不许改用
//! `rustc` 可执行文件的 mtime/size 当版本代理（rustup 的 shim 换工具链时那两个都不动）。
//!
//! # 为什么"验证"比哈希更重要
//!
//! 目录名只是**索引**（FNV-1a 64，非密码学）。命中判定要求把 `identity.bin` 读回来与本次
//! 算出的身份 blob **逐字节相等**，并且 `exe.bin` 的**摘要**（`digest.txt`）与
//! `rustc.stderr` 都读得出来、都对得上 ——
//! 于是哈希碰撞的后果是多一次 `memcmp`，而不是拿到错误的二进制。任何不匹配、任何 IO 失败
//! 一律当 miss（宁可多编译一次，绝不把不确定的产物说成命中）。
//!
//! # 命中时与冷构建的唯一行为差
//!
//! 不刷新 `<out>.pdb`（它由**同一条命令行**的上一次冷构建留在同一路径上；真丢了就用
//! `--no-cache` 编一次）。rustc 的 stderr 原文（NDJSON）存在条目里，命中时走**同一个**
//! `errmap::render_diagnostics` 重新渲染 → 告警信号不会因为命中而消失
//! （`scripts/bench/run.sh` 正是靠这条信号判"生成物零 rustc 告警"）。

use std::path::{Path, PathBuf};
use std::time::Duration;

/// 缓存目录名（§16.2）。已在 `.gitignore` 里，产物不是仓库内容。
const CACHE_DIR_NAME: &str = ".rscel-cache";
/// 条目格式版本：身份 blob 的字段构成一改就升这个号，让旧条目整体 miss。
/// `v2` = 两级键（条目根不含版本，见模块头那一节）；`v1` 的扁平条目由 `prune` 收掉。
const FORMAT_VERSION: &str = "v2";
/// 最多保留多少个**叶子条目**（每条约 = 一个 exe，实测 185KB 量级）。
/// 上限存在的理由：`rscel build` 会被脚本反复调到不同的 `-o` 上，无限追加就是无限占盘。
const MAX_ENTRIES: usize = 16;
/// 崩溃遗留的临时条目目录（`<hex>.tmp-*`）超过这个年龄才可以安全回收。
const TEMP_ENTRY_AGE: Duration = Duration::from_secs(3600);

/// 是否允许读写增量缓存（CLI 的 `--no-cache` 把它置为 `Disabled`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CachePolicy {
    Enabled,
    Disabled,
}

/// 一次 rustc 调用里**与工具链版本无关**的那半截身份 —— 它决定条目根目录的名字。
///
/// 之所以要把这一半单独拿出来：根目录不存在就等于"这次调用从没编过"，此时查版本
/// 纯属白等（那 52–67ms 的构成见模块头「两级键」）。全是共享引用，所以 `Copy`。
#[derive(Clone, Copy)]
pub struct RustcCall<'a> {
    /// 交给 rustc 的生成文件路径（原样，不 canonicalize —— 理由见模块头）。
    pub rs_path: &'a Path,
    /// `-o` 的目标路径（原样）。
    pub output: &'a Path,
    /// 除输入/输出之外的完整 rustc 参数向量（含 `--error-format=json`：
    /// 它决定了条目里存的 stderr 长什么样）。
    pub args: &'a [String],
    /// 本次生成的 `.rs` 字节（内嵌运行时与 `--debug` 埋点都在里面，所以代码生成一变就 miss）。
    pub generated_rs: &'a [u8],
}

/// 一次 rustc 调用的**完整身份**：命中要求整次调用逐字段相同。
pub struct RustcIdentity<'a> {
    /// 不含版本的那半截（条目根由它索引）。
    pub call: RustcCall<'a>,
    /// `rustc -vV` 的完整输出：工具链版本、host triple、LLVM 版本都在里面。
    /// 它进叶子目录名，也进 `identity.bin`，命中时**逐字节验证**。
    pub rustc_version: &'a str,
}

/// 把「不含版本的身份」编成自描述的二进制 blob。
///
/// 字段用「十进制长度 + `\n` + 原始字节」分帧，而不是用换行分隔：路径与 `rustc -vV`
/// 输出都可能含换行/制表符，分隔符撞上内容的后果就是两次不同的调用编出同一个 blob。
/// 参数向量额外记**个数**，否则 `["-C", "opt-level=3"]` 与 `["-C opt-level=3"]` 会撞键。
fn call_blob(call: &RustcCall<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(FORMAT_VERSION.as_bytes());
    out.push(0);
    let mut put = |field: &[u8]| {
        out.extend_from_slice(field.len().to_string().as_bytes());
        out.push(b'\n');
        out.extend_from_slice(field);
    };
    put(call.rs_path.to_string_lossy().as_bytes());
    put(call.output.to_string_lossy().as_bytes());
    put(call.args.len().to_string().as_bytes());
    for a in call.args {
        put(a.as_bytes());
    }
    put(call.generated_rs);
    out
}

/// 命中要逐字节比对的那份 blob = 不含版本的那半截 **+ 版本帧**。
///
/// 版本放在**最后**一帧，于是 `identity.bin` 以 `call_blob` 开头、末尾多一段版本全文：
/// 这个形状让"版本确实验证过"这件事在条目文件里直接可读（集成测试就是读它来钉的）。
fn identity_blob(id: &RustcIdentity<'_>) -> Vec<u8> {
    let mut out = call_blob(&id.call);
    let version = id.rustc_version.as_bytes();
    out.extend_from_slice(version.len().to_string().as_bytes());
    out.push(b'\n');
    out.extend_from_slice(version);
    out
}

/// 产物自己的摘要（`<字节数> <fnv1a 十六进制>`）。
///
/// 防的是"身份没变但产物文件坏了"这一类：条目文件被外部截断、写坏、只删掉一半时，
/// 命中判定必须退成 miss 重新编译，而不是把一个跑不起来的 exe 复制到用户手上。
/// 同样不是密码学摘要 —— 这里要的是完整性自检，不是抗篡改。
fn artifact_digest(exe: &[u8]) -> String {
    format!("{} {}", exe.len(), index_hash(exe))
}

/// FNV-1a 64 位，只当**索引**用（真验证是身份 blob 的逐字节比对），所以不追求抗碰撞，
/// 只求零依赖且跨进程稳定 —— `std` 的 `Hasher` 做不到后者：`RandomState` 每个进程换种子，
/// 缓存键会每次都不一样（那才是真的白做）。
fn index_hash(blob: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in blob {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// 从 DSL 文件所在目录向上找缓存根（§16.2 的"向上找"）。
///
/// 顺序：已存在的最近一个 `.rscel-cache/` → 最近的含 `.git` 的祖先（= 项目根，与
/// `.gitignore` 里那条对齐）→ 源文件自己的目录（临时目录里的单次构建）。
fn cache_root(source: &Path) -> Option<PathBuf> {
    let start = source.parent().unwrap_or(Path::new(".")).to_path_buf();
    let mut dir: &Path = &start;
    let mut first_seen: Option<PathBuf> = None;
    loop {
        let candidate = dir.join(CACHE_DIR_NAME);
        if candidate.is_dir() {
            return Some(candidate);
        }
        if first_seen.is_none() {
            first_seen = Some(dir.to_path_buf());
        }
        if dir.join(".git").exists() {
            return Some(candidate);
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => break,
        }
    }
    Some(first_seen?.join(CACHE_DIR_NAME))
}

/// `rustc -vV` 全文（**串行**）。拿不到就返回 `None`，调用方据此**跳过缓存**并照常调 rustc ——
/// rustc 真的不存在时，该给用户看的是原有的 `RSC: rustc invocation failed`，不是缓存话术。
///
/// 只在"条目根已经存在、需要拿版本去验身份"那一侧用这条；冷构建侧请用
/// [`spawn_rustc_version`] + [`collect_rustc_version`]，那一次 spawn 与编译重叠。
pub fn rustc_version() -> Option<String> {
    let out = std::process::Command::new("rustc").arg("-vV").output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// 非阻塞地把 `rustc -vV` 发出去，与真正的编译**重叠**跑（模块头「两级键」那一节）。
///
/// stderr 挂 `null`：版本查询没有可读的诊断，让它漏进用户终端等于凭空多一行噪声。
/// spawn 失败（rustc 不存在）返回 `None`，调用方按"这次不写条目"处理，
/// 而编译那一步自然会报出真正的缺失 —— 缓存不提供第二条报错路径。
pub fn spawn_rustc_version() -> Option<std::process::Child> {
    let mut cmd = std::process::Command::new("rustc");
    cmd.arg("-vV");
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    cmd.spawn().ok()
}

/// 收掉与编译重叠的那次版本查询（**消费**那个 `Child`：`wait_with_output` 要拿走句柄）。
///
/// **必须被调用**（哪怕本次编译失败）：`Child` 就地丢弃会把子进程留成没人 reap 的孤儿。
/// 非零退出 / 读不到 / 非法 UTF-8 一律 `None`（= 本次不写条目，构建结果不受影响）。
pub fn collect_rustc_version(child: std::process::Child) -> Option<String> {
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// 本次构建可用的缓存；根目录建不出来就是 `None`（构建照常，只是没有缓存）。
pub struct Cache {
    pub root: PathBuf,
}

impl Cache {
    pub fn open(source: &Path) -> Option<Cache> {
        let root = cache_root(source)?;
        if std::fs::create_dir_all(&root).is_err() {
            return None;
        }
        Some(Cache { root })
    }

    /// 条目**根**目录（名字只由不含版本的身份决定）。
    ///
    /// 公开这个是因为调用方要拿它做那个"要不要查版本"的判定：它不存在 ⇒ 定义上就是 miss。
    pub fn base_dir(&self, call: &RustcCall<'_>) -> PathBuf {
        self.root.join(index_hash(&call_blob(call)))
    }

    /// 叶子条目目录 = 根 / 版本全文的索引。换工具链是**另开一格**，不是改写同一格。
    fn entry_dir(&self, id: &RustcIdentity<'_>) -> PathBuf {
        self.base_dir(&id.call).join(index_hash(id.rustc_version.as_bytes()))
    }

    /// 查缓存；返回 `(exe 字节, rustc stderr 原文)`，任何不对劲都是 `None`（= miss）。
    pub fn lookup(&self, id: &RustcIdentity<'_>) -> Option<(Vec<u8>, String)> {
        let blob = identity_blob(id);
        let entry = self.entry_dir(id);
        // 逐字节验证身份：宁可 miss，也不把"另一次调用"的产物当命中端出去。
        if std::fs::read(entry.join("identity.bin")).ok()? != blob {
            return None;
        }
        let exe = std::fs::read(entry.join("exe.bin")).ok()?;
        // 产物自己的摘要也要核：身份对了不代表 `exe.bin` 还是当初写进去的那份
        // （外部截断、磁盘写坏、有人手抖删了半个文件都可能只坏掉产物）。
        let digest = std::fs::read_to_string(entry.join("digest.txt")).ok()?;
        if digest.trim() != artifact_digest(&exe) {
            return None;
        }
        let stderr = std::fs::read_to_string(entry.join("rustc.stderr")).ok()?;
        Some((exe, stderr))
    }

    /// 把「刚编好的产物 + 刚抓到的 rustc stderr」存成条目。
    ///
    /// 失败只影响下次能不能命中，**不影响本次构建结果**，所以调用方拿到 `Err(原因)`
    /// 该做的是继续按成功收尾、并把这句话端给用户（缓存写不进去不是用户的错，但不能不说）。
    pub fn store(&self, id: &RustcIdentity<'_>, stderr_json: &str) -> Result<(), String> {
        let blob = identity_blob(id);
        let base = self.base_dir(&id.call);
        let leaf_name = index_hash(id.rustc_version.as_bytes());
        let entry = base.join(&leaf_name);
        if entry.is_dir() {
            return Ok(());
        }
        std::fs::create_dir_all(&base).map_err(|e| format!("create {}: {e}", base.display()))?;
        let exe = std::fs::read(id.call.output).map_err(|e| format!("读产物 {}: {e}", id.call.output.display()))?;
        // 先写独占的临时目录再整体改名：并发的两个构建不会各写一半、合成一个
        // "看起来完整"的条目；崩溃留下的最多是一个没人引用的临时目录。
        let tmp = base.join(format!(
            "{}.tmp-{}-{}",
            leaf_name,
            std::process::id(),
            std::time::UNIX_EPOCH.elapsed().map_err(|e| e.to_string())?.as_nanos()
        ));
        std::fs::create_dir_all(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        let written = (|| -> Result<(), String> {
            std::fs::write(tmp.join("identity.bin"), &blob).map_err(|e| format!("写 identity.bin: {e}"))?;
            std::fs::write(tmp.join("exe.bin"), &exe).map_err(|e| format!("写 exe.bin: {e}"))?;
            std::fs::write(tmp.join("digest.txt"), format!("{}\n", artifact_digest(&exe)))
                .map_err(|e| format!("写 digest.txt: {e}"))?;
            std::fs::write(tmp.join("rustc.stderr"), stderr_json.as_bytes())
                .map_err(|e| format!("写 rustc.stderr: {e}"))?;
            Ok(())
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e);
        }
        if entry.exists() {
            // 同名条目已经就位（并发批次刚写同一份产物）：同键同内容，留谁的都一样。
            let _ = std::fs::remove_dir_all(&tmp);
            return Ok(());
        }
        std::fs::rename(&tmp, &entry).map_err(|e| format!("改名落位: {e}"))?;
        prune(&self.root)
    }
}

/// 目录名是不是一个索引格（16 位十六进制）。
fn is_index_name(name: &str) -> bool {
    name.len() == 16 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 目录 mtime 距今多久；任何一步拿不到就当**最旧**（`Duration::ZERO` 在"新→旧"排序里排最后，
/// 也就是优先被删 —— 读不到 mtime 的格子本来就不是正常写进去的条目）。
fn age_of(path: &Path) -> Duration {
    let Ok(meta) = std::fs::metadata(path) else { return Duration::ZERO };
    let Ok(modified) = meta.modified() else { return Duration::ZERO };
    modified.elapsed().unwrap_or(Duration::ZERO)
}

/// 回收：只保留最近 `MAX_ENTRIES` 个**叶子条目**，顺带清掉过期（一小时前）的临时目录、
/// `v1` 留下的扁平条目，以及被掏空的那层根目录。
fn prune(root: &Path) -> Result<(), String> {
    let mut entries: Vec<(Duration, PathBuf)> = Vec::new();
    let mut stale_tmp: Vec<PathBuf> = Vec::new();
    let mut empty_bases: Vec<PathBuf> = Vec::new();
    let read = match std::fs::read_dir(root) {
        Ok(r) => r,
        Err(e) => return Err(format!("读缓存目录: {e}")),
    };
    for path in read.flatten().map(|d| d.path()) {
        if !path.is_dir() {
            continue;
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if name.contains(".tmp-") {
            // 只碰明显过期的临时目录：正在写的那一个才几微秒大，删它就是破坏并发批次。
            if age_of(&path) > TEMP_ENTRY_AGE {
                stale_tmp.push(path);
            }
            continue;
        }
        if !is_index_name(&name) {
            continue;
        }
        // v1 的扁平条目（这一层就直接放着 `identity.bin`）：它的目录名含版本，
        // 两级键永远不会再读它 → 收掉，别让 185KB 的废格子长期占盘。
        if path.join("identity.bin").is_file() {
            let _ = std::fs::remove_dir_all(&path);
            continue;
        }
        let mut leaves = 0usize;
        let Ok(sub) = std::fs::read_dir(&path) else { continue };
        for leaf in sub.flatten().map(|d| d.path()) {
            if !leaf.is_dir() {
                continue;
            }
            let leaf_name = leaf.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if leaf_name.contains(".tmp-") {
                if age_of(&leaf) > TEMP_ENTRY_AGE {
                    stale_tmp.push(leaf);
                }
                continue;
            }
            if is_index_name(&leaf_name) {
                entries.push((age_of(&leaf), leaf));
                leaves += 1;
            }
        }
        if leaves == 0 {
            empty_bases.push(path);
        }
    }
    for p in stale_tmp {
        let _ = std::fs::remove_dir_all(p);
    }
    if entries.len() > MAX_ENTRIES {
        // mtime 新→旧排序，砍掉超出上限的；删失败就留着下次再删（缓存不是正确性依赖）。
        entries.sort_by_key(|(age, _)| std::cmp::Reverse(*age));
        for (_, path) in &entries[MAX_ENTRIES..] {
            let _ = std::fs::remove_dir_all(path);
        }
    }
    for base in empty_bases {
        // 只删真空目录：`remove_dir` 非空必失败，所以正被并发批次使用的那一格动不了。
        let _ = std::fs::remove_dir(&base);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity<'a>(rs: &'a str, out: &'a str, args: &'a [String], version: &'a str, gen: &'a [u8]) -> RustcIdentity<'a> {
        RustcIdentity {
            call: RustcCall {
                rs_path: Path::new(rs),
                output: Path::new(out),
                args,
                generated_rs: gen,
            },
            rustc_version: version,
        }
    }

    /// 长度前缀分帧的意义：内容里有换行的字段不能把表框撑坏。
    #[test]
    fn newline_inside_a_path_is_not_a_field_separator() {
        let args = vec!["--edition=2021".to_string()];
        let a = identity("we\nird.rs", "out.exe", &args, "rustc 1.0", b"fn main(){}");
        let b = identity("we", "ird.rs\nout.exe", &args, "rustc 1.0", b"fn main(){}");
        assert_ne!(identity_blob(&a), identity_blob(&b), "换行被当成了字段边界");
    }

    /// 参数向量的**个数**也要进 blob：少记一层就会把两种不同的命令行合成一个键。
    #[test]
    fn argument_vector_length_is_part_of_the_identity() {
        let gen = b"fn main(){}";
        let two_args = vec!["-C".to_string(), "opt-level=3".to_string()];
        let one_arg = vec!["-C opt-level=3".to_string()];
        let two = identity("a.rs", "o.exe", &two_args, "v", gen);
        let one = identity("a.rs", "o.exe", &one_arg, "v", gen);
        assert_ne!(identity_blob(&two), identity_blob(&one), "参数切分方式没进键");
    }

    /// 任何一项身份不同都必须产生不同的 blob —— 这些正是"缓存不能跨的维度"。
    #[test]
    fn every_identity_field_changes_the_blob() {
        let args = vec!["--edition=2021".to_string()];
        let base = identity("a.rs", "o.exe", &args, "rustc 1.75", b"body");
        let variants = [
            identity("b.rs", "o.exe", &args, "rustc 1.75", b"body"),
            identity("a.rs", "p.exe", &args, "rustc 1.75", b"body"),
            identity("a.rs", "o.exe", &args, "rustc 1.80", b"body"),
            identity("a.rs", "o.exe", &args, "rustc 1.75", b"bbody"),
        ];
        for v in variants {
            assert_ne!(identity_blob(&base), identity_blob(&v));
        }
    }

    /// 索引只是目录名：同一段字节永远算出同一个十六进制串（跨进程稳定才谈得上缓存）。
    #[test]
    fn index_hash_is_stable_and_hex_fixed_width() {
        let h1 = index_hash(b"abc");
        let h2 = index_hash(b"abc");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 16);
        assert!(h1.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(h1, index_hash(b"abd"));
    }

    /// 两级键的形状，两句必须**同时**成立：条目根不看版本（否则"根不存在就是 miss"
    /// 这个判定要求先花 52–67ms 查版本），而验证用的 blob 仍然带版本全文
    /// （否则换工具链还能吃旧产物 —— 那是 P2-③b 明确不许放松的一条）。
    #[test]
    fn the_index_ignores_the_version_while_the_verified_blob_does_not() {
        let args = vec!["--edition=2021".to_string()];
        let cache = Cache { root: PathBuf::from(".") };
        let v75 = identity("a.rs", "o.exe", &args, "rustc 1.75.0\nhost: x86_64", b"body");
        let v80 = identity("a.rs", "o.exe", &args, "rustc 1.80.0\nhost: x86_64", b"body");
        assert_eq!(
            cache.base_dir(&v75.call),
            cache.base_dir(&v80.call),
            "条目根因工具链版本换了格子，miss 判定就还得先查一次版本"
        );
        assert_ne!(identity_blob(&v75), identity_blob(&v80), "验证用的 blob 必须含版本");
        // 叶子格子按版本分：同一次调用换版本是**另开一格**，不是改写同一格。
        assert_ne!(cache.entry_dir(&v75), cache.entry_dir(&v80));
        let other_call = identity("b.rs", "o.exe", &args, "rustc 1.75.0\nhost: x86_64", b"body");
        assert_ne!(cache.base_dir(&v75.call), cache.base_dir(&other_call.call), "不含版本的那半截身份必须进索引");
    }

    /// `identity.bin` 的最后一帧就是版本全文（带自己的长度前缀），所以"版本确实被
    /// 逐字节验证"这件事在条目文件里是**可读的**，集成测试据此去篡改它并期望退成 miss。
    #[test]
    fn the_verified_blob_ends_with_the_full_version_text() {
        let args = vec!["--edition=2021".to_string()];
        let version = "rustc 1.75.0 (abc123 2023-12-31)\nbinary: rustc\nhost: x86_64-pc-windows-msvc\n";
        let id = identity("a.rs", "o.exe", &args, version, b"body");
        let blob = identity_blob(&id);
        let base = call_blob(&id.call);
        let tail = version.as_bytes();
        let frame = format!("{}\n", tail.len()).into_bytes();
        assert_eq!(&blob[..base.len()], base.as_slice(), "前半截就是不含版本的那份身份");
        assert!(blob.ends_with(tail), "版本全文必须是 blob 的最后一帧");
        assert!(blob[..blob.len() - tail.len()].ends_with(&frame), "版本帧少了长度前缀就会被别的字段内容撞开");
    }

    /// 摘要防的那一类：产物被截断后必须换摘要，否则命中判定形同虚设。
    #[test]
    fn artifact_digest_changes_when_the_artifact_is_truncated() {
        let full = vec![7u8; 64];
        assert_ne!(artifact_digest(&full), artifact_digest(&full[..32]), "截断必须换摘要");
        let mut flipped = full.clone();
        flipped[0] = 8;
        assert_ne!(artifact_digest(&full), artifact_digest(&flipped), "同长度的内容变化也要出不同摘要");
    }

    /// §16.2 的"从文件所在目录向上找"：最近的 `.rscel-cache/` 优先，其次项目根（`.git`），
    /// 都没有才落到源文件自己的目录。
    #[test]
    fn cache_root_walks_up_to_the_project_marker() {
        let pid = std::process::id();
        let base = std::env::temp_dir().join(format!("rscel-cache-root-{pid}"));
        let _ = std::fs::remove_dir_all(&base);
        let nested = base.join("bench").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(base.join(".git")).unwrap();
        let source = nested.join("x.rscel");
        std::fs::write(&source, "cell main;\n").unwrap();
        assert_eq!(
            cache_root(&source).unwrap(),
            base.join(CACHE_DIR_NAME),
            "有项目标记时该落在项目根，而不是源文件旁边"
        );

        // 已经存在的 `.rscel-cache/` 比 `.git` 更近时，用更近的那个（§16.2 的"向上找"= 就近）。
        std::fs::create_dir_all(nested.join(CACHE_DIR_NAME)).unwrap();
        assert_eq!(cache_root(&source).unwrap(), nested.join(CACHE_DIR_NAME));

        // 祖先链上没有任何项目标记时（临时目录里的单次构建），退回源文件自己的目录。
        // 前提：这台机器的 Temp 祖先链确实没有别的 `.git` —— 有则本断言会红，那是环境变了，
        // 不是代码错，看到红先看这里的注释而不是改生成器。
        let free = std::env::temp_dir().join(format!("rscel-cache-free-{pid}"));
        let _ = std::fs::remove_dir_all(&free);
        let orphan = free.join("only");
        std::fs::create_dir_all(&orphan).unwrap();
        let orphan_source = orphan.join("y.rscel");
        std::fs::write(&orphan_source, "cell main;\n").unwrap();
        assert_eq!(cache_root(&orphan_source).unwrap(), orphan.join(CACHE_DIR_NAME));

        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&free);
    }

    /// `prune` 的三条边界，都是两级键带来的新形状：
    /// ① 正在写的新鲜临时目录不能碰；② 封顶按**叶子条目**数算（一个根格子可以住多个版本）；
    /// ③ `v1` 遗留的扁平条目和新掏空的根格子要清掉，否则废格子永远占盘。
    #[test]
    fn prune_keeps_fresh_temp_dirs_caps_leaves_and_clears_legacy_entries() {
        let root = std::env::temp_dir().join(format!("rscel-cache-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(root.join("ffff000000000000.tmp-1-2")).unwrap();
        // v1 的扁平条目：这一层直接放着 identity.bin，两级键永远不会读它。
        let legacy = root.join("eeee000000000000");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("identity.bin"), b"old").unwrap();
        // 叶子被删光之后剩下的空根格子。
        std::fs::create_dir_all(root.join("dddd000000000000")).unwrap();
        for i in 0..(MAX_ENTRIES + 3) {
            std::fs::create_dir_all(
                root.join(format!("{:016x}", i)).join(format!("f{:015x}", i)),
            )
            .unwrap();
        }
        prune(&root).unwrap();

        assert!(!legacy.exists(), "v1 的扁平条目没被清掉，会长期白占一整份 exe");
        assert!(root.join("ffff000000000000.tmp-1-2").exists(), "新鲜的临时目录被误删了");
        assert!(!root.join("dddd000000000000").exists(), "空的根格子没被回收");
        let leaves: usize = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|d| d.path())
            .filter(|p| p.is_dir() && !p.file_name().unwrap().to_string_lossy().contains(".tmp-"))
            .map(|p| std::fs::read_dir(&p).map(|sub| sub.flatten().filter(|d| d.path().is_dir()).count()).unwrap_or(0))
            .sum();
        assert!(leaves <= MAX_ENTRIES, "叶子条目数 {leaves} 没被压到上限 {MAX_ENTRIES}");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
