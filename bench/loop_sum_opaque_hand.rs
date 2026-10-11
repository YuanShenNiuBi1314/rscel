// 场景 1b 的手写对照（自然写法）：循环在 `main` 里，起始值与 DSL 一样**不透明**。
//
// 这就是被从 `loop_sum_hand.rs` 里删掉的那种写法（`black_box(0)`/`black_box(1)`）——
// 它本身没问题，问题在于 `loop_sum` 的 DSL 传的是**字面量**，两边不同构。
// 对不透明起始值的 DSL（`loop_sum_opaque.rscel`）它才是同构对照。
//
// 2026-10-10 交错实测：这份手写 ≈ 12.4–13.7ms，而**同不透明起始值的生成物**
// ≈ 39–48ms（慢 3.3–3.9 倍）。也就是说生成物在"入参是运行期未知值"时存在
// 真实的性能缺陷，常量场景把它盖住了。归因进度见 docs/05 P0-4。
fn main() {
    let t0 = std::time::Instant::now();
    let mut a: i64 = std::hint::black_box(0);
    let mut i: i64 = std::hint::black_box(1);
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
