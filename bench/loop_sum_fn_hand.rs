// 场景 1 的第二份手写对照：把循环拆进**独立 `fn`** 的写法。
//
// 为什么需要一份"拆函数"的对照（docs/05 P0-1 候选 (4)「基准公平性复核」）：`loop_sum_hand.rs`
// 把循环直接写在 `main` 里，两种形状在 LLVM 眼里可能有不同的优化结果，只给一份基线就会出现
// "比值到底反映转译质量还是反映函数拆分"分不清的问题。三边一起报数：
//   · loop_sum_hand.rs     —— 人最自然的写法（循环在 main 里）
//   · loop_sum_fn_hand.rs  —— 循环在独立 fn 里   ← 本文件
//   · 生成物               —— rscel build 产出
//
// ⚠ **本文件名字里"与生成物同构"这个词已经过期**（2026-10-11 复核，docs/05 P0-6 动作 0 之后）：
// P0-4 的热 Cell 实体展开（commit f6fb71b）之后，生成物**不再**把单一调用点的同步 Cell 发成
// 独立 `fn`，而是把 body 逐字发进调用点的块表达式。同一批已经把
// `bench/loop_sum_opaque_fn_hand.rs` 跟到新形状，这一份**留着旧形状没跟**，依据是两层实测：
//   · 本场景起始值是**字面常量**，"LLVM 常量起始值病"把所有形状压进同一个量级
//     （2026-10-10 交错实测：main 45.83ms / 独立 fn 45.10ms / 生成物 44.59ms，差 ≤1.7%）；
//   · 2026-10-11 门禁台三窗（`RSC_REPS=61`、中位数口径）这份 13.03–13.19ms、
//     生成物 13.17–13.23ms → 比值 98.97%–99.63%，形状差没有露出来。
// 所以这一格现在的准确说法是「与**函数形状的**手写比」，跟进动作登记在 `docs/05` P0-6。
//
// 这里不加 `#[inline(always)]`：那是代码生成器的策略，人手写同一段逻辑不会加，而本场景
// 常量起始值下函数边界实测不收费（上面那三个数）。这与 `loop_sum_opaque` 那份**相反**：
// 那一格展开形状与独立 `fn` 差 3 倍（docs/06 实验一的 V01/V03），所以只有它把提示照留了。
// 起始值同样写字面常量，与 DSL 的 `jump sum go with(0i64 -> acc, 1i64 -> k)` 对应。
fn cell_sum(a: i64, i: i64) -> i64 {
    let mut acc = a;
    let mut k = i;
    let limit: i64 = std::hint::black_box(1_000_000_000);
    loop {
        let next_acc = acc + k;
        let next_k = k + 1;
        if !(next_k <= limit) {
            break;
        }
        acc = next_acc;
        k = next_k;
    }
    acc
}

fn main() {
    let t0 = std::time::Instant::now();
    let checksum = cell_sum(0, 1);
    println!("checksum={checksum}");
    println!("elapsed_ns={}", t0.elapsed().as_nanos());
}
