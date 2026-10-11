// 场景 1b 的第二份手写对照：**与生成物同构** —— 2026-10-11 跟着 P0-4 的实体展开重写。
//
// 为什么这一份必须重写（docs/05 P0-6 动作 0，docs/06 实验六）：本文件是 `loop_sum_opaque`
// 的**门禁分母**，而它写于 commit f6fb71b **之前**：循环还待在
// `#[inline(always)] fn cell_sum(...)` 这个独立函数里，文件头还声称"生成器就是这么发的"。
// 展开之后生成物**已经没有** `fn cell_sum` 了（单调用点的同步 Cell 被逐字发进调用点的
// 块表达式），于是"P0-6：典型运行慢约 1%"是拿**块表达式里的循环**去比**独立 `fn` 里的
// 循环**得到的 —— 两侧本来不同形，这个残差连归因的资格都没有。与 P0-5 第 4 项对编排
// 基线做过的事情同一类（当时是分母的 supervisor 交接不同构）。
//
// 本文件逐处照抄 `rscel build bench/loop_sum_opaque.rscel --release` 的生成物结构
// （对照 `target/ab/gen.rs`，与当前生成物逐字一致）：
//   · entry Cell 的 `#[inline(always)] fn cell_main() -> Option<RetMain>` 外壳，
//     被调 Cell 的 body 发进它里面的**块表达式**，被调方的尾 `exit` 变成块尾值
//     `Some(RetSum {})`，外面再套 `let _ = ... ; return Some(RetMain {})`；
//   · S2 类型化调用约定的**别名拷贝**（生成物 `v_a`/`v_b` ↔ 这里 `acc`/`k`）；
//   · 起始值的 `black_box` 落在 **`t0` 之前**（生成物就是这个顺序：先取值再开表）；
//   · 循环的 `let` 是 **hoisted 形状**（`let mut next_acc;` 在循环外声明、循环内赋值）；
//   · `limit` 也在计时区内取，`checksum=` / `elapsed_ns=` 两行逐字相同。
//
// 内联提示**照留**（这与 `bench/pipeline_and_fn_hand.rs` 的做法相反，理由是实测的）：
// `docs/06` 实验一的 V02→V03 那一格给这个形状量过 +37%（20.56ms → 12.91ms），
// 也就是这里去掉提示会让分母虚慢好几倍、门禁形同虚设；而编排场景实测提示不收费
// （每元素工作由互斥锁主导），那份才去掉了。
//
// 与生成物的**有意**差异只剩两处，都是已实测免费的：
// ① 没有整段内嵌运行时的死代码 —— `docs/06` 实验五的 `V08`（= 手写展开形状 + 同一段
//    死代码）四窗读数 +0.16% / 0.00% / −0.16% / 0.00% → 打平；
// ② `main` 里没有 `let __rsc_argv = std::env::args().skip(1).collect();` 那行采集
//    （DSL 侧 `receive()` 不取实参，人手写不会白收集）。**这一处是本文件唯一还没被
//    单因素隔离过的差异**，所以若分母修完仍剩残差，它和内嵌运行时一起是下一批的靶子
//    （P0-6 动作 1 的 V10 定义本来就不含这两处）。
//
// 历史读数（**旧分母**，min 口径，单位 ms）：2026-10-10 这份 ≈ 13.5–13.9，当时生成物
// ≈ 39–48（差 3.3×，那是 P0-4 修之前的生成物形状）。旧分母在中位数口径下 6.46–6.55、
// 与当时生成物 6.54–6.68 差约 1%（docs/05 P0-6 那七窗）。修正后的读数在 docs/06 实验六。
#[derive(Default)]
struct RetMain {}

#[derive(Default)]
struct RetSum {}

#[inline(always)]
fn cell_main() -> Option<RetMain> {
    { let _ = {
        let a: i64 = std::hint::black_box(0i64);
        let i: i64 = std::hint::black_box(1i64);
        let mut acc: i64 = a;
        let mut k: i64 = i;
        let t0 = std::time::Instant::now();
        let limit: i64 = std::hint::black_box(1000000000i64);
        let mut next_acc;
        let mut next_k;
        loop {
            next_acc = acc + k;
            next_k = k + 1;
            if !(next_k <= limit) {
                break;
            }
            acc = next_acc;
            k = next_k;
        }
        println!("checksum={}", acc);
        println!("elapsed_ns={}", t0.elapsed().as_nanos());
        Some(RetSum {})
    }; return Some(RetMain {}); }
}

fn main() {
    let _ = cell_main();
}
