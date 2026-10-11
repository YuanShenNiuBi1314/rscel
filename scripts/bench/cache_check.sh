#!/usr/bin/env bash
# §16.2 增量缓存的**等价性**扫描（docs/05 P2 ③ 那笔「9/9 逐字相同」就是这台出的）。
#
# 为什么必须有这台：缓存唯一能证伪的方式是「命中与否，产物是否真的同一份」。
# 只测一个小样例不够 —— 展开、内嵌运行时、`--debug` 埋点、extern rust 这些形状各自
# 都可能让"同一份生成物"这个前提在某一格里不成立，所以要过一遍**全部 9 份 DSL**。
#
# 每份 DSL 在同一条命令行上跑三次：
#   ① --no-cache        冷构建，既不读也不写（= 改动前的行为）
#   ② Enabled 首次      条目为空 → 必须 miss 并写入
#   ③ Enabled 再次      必须命中
# 判定：三份 `.rs` 与 `.map.json` 逐字相同；③ 的 exe 与 ② 的逐字相同；② 不说命中、③ 说命中。
# **不**要求 ①/② 的 exe 逐字相同：rustc 每次链接都会写新的时间戳与 PDB GUID
# （实测同一 `-o` 名差 21 字节、换 `-o` 名差 762 字节，机器码与大小相同）。
#
# 用法：RSC_BIN=target/reldata2/release/rscel.exe bash scripts/bench/cache_check.sh
set -uo pipefail
cd "$(dirname "$0")/../.."

RSC="${RSC_BIN:-target/release/rscel.exe}"
MODE="${RSC_CC_MODE:-release}"
OUT="target/scratch/cache-check"
DSLS=(
  bench/loop_sum.rscel
  bench/loop_sum_opaque.rscel
  bench/loop_sum_two_sites.rscel
  bench/pipeline_and.rscel
  bench/read_share.rscel
  examples/multi_branch_die.rscel
  examples/extern_rust_mix.rscel
  crates/rscel-gen/examples/factorial_loop.rscel
  crates/rscel-gen/examples/jump_and_worker.rscel
)

[[ -x "$RSC" || -f "$RSC" ]] || { echo "找不到编译器：$RSC（先 cargo build --release -p rscel-cli，或用 RSC_BIN 指一份确认是 HEAD 的）"; exit 1; }
rm -rf "$OUT"

fails=0
for dsl in "${DSLS[@]}"; do
  tag="$(basename "$dsl" .rscel)"
  d="$OUT/$tag"
  mkdir -p "$d"
  cp "$dsl" "$d/$tag.rscel"
  # 缓存根按「就近向上找」的规则落在这一级：九份 DSL 各自独立，才不会让 MAX_ENTRIES 的
  # 淘汰在扫描中途把别人的条目挤掉（9 份 × 每份 1 条 > 上限 16 的话，读到的就是假阴性）。
  mkdir -p "$d/.rscel-cache"
  exe="$d/$tag"

  log1="$d/log1.txt"; log2="$d/log2.txt"; log3="$d/log3.txt"
  "$RSC" build "$d/$tag.rscel" -o "$exe" "--$MODE" --no-cache 2>"$log1" 1>/dev/null || { echo "FAIL $tag 第①次构建失败"; cat "$log1"; fails=$((fails+1)); continue; }
  cp "$exe.rs" "$d/rs1"; cp "$exe.map.json" "$d/map1"; cp "$exe" "$d/exe1"

  "$RSC" build "$d/$tag.rscel" -o "$exe" "--$MODE" 2>"$log2" 1>/dev/null || { echo "FAIL $tag 第②次构建失败"; cat "$log2"; fails=$((fails+1)); continue; }
  cp "$exe.rs" "$d/rs2"; cp "$exe.map.json" "$d/map2"; cp "$exe" "$d/exe2"

  "$RSC" build "$d/$tag.rscel" -o "$exe" "--$MODE" 2>"$log3" 1>/dev/null || { echo "FAIL $tag 第③次构建失败"; cat "$log3"; fails=$((fails+1)); continue; }
  cp "$exe.rs" "$d/rs3"; cp "$exe.map.json" "$d/map3"; cp "$exe" "$d/exe3"

  reasons=()
  cmp -s "$d/rs1" "$d/rs2"   || reasons+=("①↔② .rs 有差")
  cmp -s "$d/rs2" "$d/rs3"   || reasons+=("②↔③ .rs 有差")
  cmp -s "$d/map1" "$d/map2" || reasons+=("①↔② map 有差")
  cmp -s "$d/map2" "$d/map3" || reasons+=("②↔③ map 有差")
  cmp -s "$d/exe2" "$d/exe3" || reasons+=("③ 的 exe 与② 不逐字相同（命中就该是同一份字节）")
  grep -q "增量缓存命中" "$log2" && reasons+=("② 首次不该命中")
  grep -q "增量缓存命中" "$log3" || reasons+=("③ 该命中却没命中")
  # 条目数按**叶子**算：两级键（docs/05 P2-③b）之后条目住在
  # `.rscel-cache/{不含版本的身份}/{版本的索引}` 两层里，数第一层会把"根格子"当条目。
  n=$(find "$d/.rscel-cache" -mindepth 2 -maxdepth 2 -type d -name '????????????????' 2>/dev/null | wc -l | tr -d ' ')
  [[ "$n" == "1" ]] || reasons+=("叶子条目数=$n，应为 1")

  if (( ${#reasons[@]} == 0 )); then
    printf 'OK   %-20s .rs=%sB 逐字相同  map 逐字相同  命中: ②否 ③是  条目=1  exe=%sB 与②逐字相同\n' \
      "$tag" "$(wc -c <"$d/rs1" | tr -d ' ')" "$(wc -c <"$d/exe3" | tr -d ' ')"
  else
    fails=$((fails+1))
    printf 'FAIL %-20s %s\n' "$tag" "$(IFS='; '; echo "${reasons[*]}")"
  fi
  # 只留日志与判定所需，产物字节不留在树里。
  rm -f "$d/exe1" "$d/exe2" "$d/exe3" "$d"/*.pdb
done

echo
if (( fails == 0 )); then
  echo "== ${#DSLS[@]}/${#DSLS[@]} 等价性成立（缓存命中与否，前端产物逐字相同）=="
else
  echo "== 有 $fails 份 DSL 不满足等价性，看上面 FAIL 行 =="
fi
exit $(( fails > 0 ? 1 : 0 ))
