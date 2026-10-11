// 场景 1 的手写等价对照（自然写法：循环直接写在 main 里）。
// 用完全相同的 rustc flag 编译、与生成物**交错**计时（见 scripts/bench/run.sh），否则比值没有意义。
//
// 起始值必须写成**字面常量**，与 DSL 逐字对应：
//   entry main → `jump sum go with(0i64 -> acc, 1i64 -> k)` → cell_sum(0i64, 1i64)
// 旧版本这里写的是 `black_box(0)`/`black_box(1)`，注释的理由是"生成物里 v_a/v_b 来自
// Cell 形参，对 LLVM 是运行时值"。**那个前提是错的**：cell_sum 带 `#[inline(always)]`
// 且只有一个调用点，常量直接传进循环。2026-10-10 交错实测（同一温度窗、各取最小值）：
//   不透明起始的手写 = 12.38ms，常量起始的手写 = 45.10ms，常量起始的生成物 = 44.59ms
//   同一段循环写在 main = 45.83ms、写在独立 fn = 45.10ms（函数作用域不是因素）
// 也就是说旧对照快 3.6 倍的那部分完全是"起始值不透明度"差异，与转译质量无关，
// 于是把基准比值虚报成了 3.97（真实性能 ~99%）。详见 docs/05-迭代待办.md P0-1。
//
// `limit` 两侧都用 `black_box`：写成常量时 LLVM 能把等差数列求和直接闭式算掉
// （两边都掉到 30µs 量级），量级太小会让比值被噪声支配，测不出任何东西。
fn main() {
    let t0 = std::time::Instant::now();
    let mut a: i64 = 0;
    let mut i: i64 = 1;
    let limit: i64 = std::hint::black_box(1_000_000_000);
    loop {
        let next_acc = a + i;
        let next_i = i + 1;
        if !(next_i <= limit) {
            break;
        }
        a = next_acc;
        i = next_i;
    }
    println!("checksum={a}");
    println!("elapsed_ns={}", t0.elapsed().as_nanos());
}
