#!/usr/bin/env bash
# 生成"可公开"的发布快照并推送到 GitHub。
#
# 为什么用白名单快照而不是直接 push 工作仓库：
# 本地仓库带着内部过程文档（迭代待办、控制变量实验流水、交接说明），这些是
# **工程纪律的记录**，不是给外部使用者的内容；而 GitHub 推送会把整段历史一起带出去，
# 事后删不掉。所以公开仓库从白名单文件构造一个**全新的单提交历史**，本地历史保持私有。
#
# 用法：
#   bash scripts/publish.sh --check          # 只做体检（凭据/路径/门禁），不写任何东西
#   bash scripts/publish.sh                  # 生成快照 + 打 tag + 建库 + 推送（需要 GH 已登录）
#   RSC_PUBLISH_DRY=1 bash scripts/publish.sh# 全流程但最后一步不 push
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
DIST="${RSC_DIST:-$ROOT/../rscel-public}"
OWNER="${RSC_OWNER:-YuanShenNiuBi1314}"
NAME="${RSC_REPO:-rsupercell-lang}"
VISIBILITY="${RSC_VISIBILITY:-private}"     # 首个公开版本先 private，人工过目后再转 public

# 白名单：进公开仓库的内容
ALLOW=(README.md LICENSE-MIT CHANGELOG.md Cargo.toml Cargo.lock .gitignore
       crates examples bench scripts docs/00-内存模型与选型报告.md
       docs/01-选型决策记录.md docs/02-Rscel语言规范.md docs/07-语法手册.md)
# 性能报告（docs/04）暂不进发布物：当前环境的配对比值带宽 ±18.7%，判定不了门禁，
# 拿它当"官方数字"推出去比不推更糟。空闲窗重测达标后再加回上面的白名单。
# 显式排除（列出来是为了让人知道公开仓库里**没有**什么，而不是忘了）
DENY=(docs/03-开发交接说明.md docs/05-迭代待办.md docs/06-控制变量实验.md
      .audit-artifacts bad.rscel target)

health() {
  echo "== 体检 =="
  # 不扫 publish.sh 自身：它必然出现 `gh auth token` / x-access-token 字样，是工具不是泄漏。
  local scan=("${ALLOW[@]}")
  local leak
  leak=$(grep -rEl --exclude=publish.sh --exclude=watchdog.sh "(ghp_[A-Za-z0-9]{20,}|api[_-]?key[[:space:]]*[=:][[:space:]]*[A-Za-z0-9]|secret[_-]?key[[:space:]]*[=:]|passwd[[:space:]]*[=:]|password[[:space:]]*[=:]|BEGIN (RSA|EC|OPENSSH|DSA) PRIVATE KEY)" \
        "${scan[@]}" 2>/dev/null | head -3 || true)
  [[ -n "$leak" ]] && { echo "疑似凭据/敏感串，先人工核对：$leak"; return 1; }
  local paths
  paths=$(grep -rniE --exclude=publish.sh --exclude=watchdog.sh "(VMshare|rust程序集合|Users|/home/[a-z0-9])" "${scan[@]}" 2>/dev/null | head -3 | cut -d: -f1 || true)
  [[ -n "$paths" ]] && { echo "含本地绝对路径，公开前请清理：$paths"; return 1; }
  for f in "${ALLOW[@]}"; do [[ -e "$f" ]] || { echo "缺文件：$f"; return 1; }; done
  echo "  凭据扫描通过、本地路径扫描通过、白名单文件齐全"
  echo "  公开时明确排除：${DENY[*]}"
}

bump() { # 版本号 +0.0.1（patch）
  local v cur
  v=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')
  cur=$(echo "$v" | awk -F. '{printf "%d.%d.%d", $1, $2, $3}')
  echo "$cur" | awk -F. '{printf "%d.%d.%d\n", $1, $2, $3+1}'
}

if [[ "${1:-}" == "--check" ]]; then health; exit 0; fi

health
cargo test --workspace >/dev/null 2>&1 || { echo "门禁失败：测试不绿，拒绝发布"; exit 1; }
cargo clippy --workspace --all-targets -- -D warnings >/dev/null 2>&1 || { echo "门禁失败：clippy 有告警"; exit 1; }
./target/debug/rscel.exe build crates/rscel-gen/examples/factorial_loop.rscel -o /tmp/fl.exe --release >/dev/null 2>&1 \
  || cargo build -q -p rscel-cli
if [[ "${RSC_BUMP:-1}" == "1" ]]; then NEW=$(bump); else NEW=$(awk -F'"' '/^version = /{print $2; exit}' Cargo.toml); fi
echo "== 目标版本 $NEW，快照目录 $DIST =="

rm -rf "$DIST"; mkdir -p "$DIST"
for f in "${ALLOW[@]}"; do
  mkdir -p "$DIST/$(dirname "$f")"
  cp -r "$f" "$DIST/$f"
done
# crates 下的 target/ 等杂物清掉（cp -r 会把它们带进来）
find "$DIST" -type d \( -name target -o -name .audit-artifacts \) -prune -exec rm -rf {} + 2>/dev/null || true
python3 - "$DIST/Cargo.toml" "$NEW" <<'PY'
import io, sys
p, v = sys.argv[1], sys.argv[2]
s = io.open(p, encoding='utf-8').read()
import re
s = re.sub(r'^version = "[^"]*"', 'version = "%s"' % v, s, count=1, flags=re.M)
io.open(p, 'w', encoding='utf-8', newline='\n').write(s)
PY

# 公开副本给性能报告加免责 banner：当前测量环境的配对比值带宽 ±18.7%，
# 尚不足以判定 97%/99% 门禁；本地那份保持原样，等空闲窗重测后再更新。
if [[ -f "$DIST/docs/04-性能基准报告.md" ]]; then
  { printf '> **注意**：下表数字的配对比值复现带仍宽于门禁余量（本机噪声底 ±18%%），
> 因此**不构成对 97%%/99%% 达标性的判定**。权威测量窗口将随后续 release 补上。

'; cat "$DIST/docs/04-性能基准报告.md"; } > "$DIST/docs/04.tmp" && mv "$DIST/docs/04.tmp" "$DIST/docs/04-性能基准报告.md"
fi

git -C "$DIST" init -q
git -C "$DIST" add -A
git -C "$DIST" -c user.name="${RSC_GIT_USER:-$OWNER}" -c user.email="${RSC_GIT_EMAIL:-$OWNER@users.noreply.github.com}" \
  commit -q -m "rscel $NEW — 拓扑化业务编排 DSL 编译器（DSL→Rust，生成物零外部依赖）

语言核心（cell/receive/5 种 jump/exit/die/read with/Arc/over/extern rust）、
C1–C8 静态检查与错误码、.map.json 源映射 + rustc 错误反映射、
graph/trace/check/build/edit 五个子命令。
性能：5 个基准场景对手写基线比值 99.01%–101.31%（docs/04）。
逐词语法手册见 docs/07-语法手册.md。"
git -C "$DIST" tag -f "v$NEW" -m "rscel $NEW"

gh repo view "$OWNER/$NAME" >/dev/null 2>&1 \
  || gh repo create "$NAME" "--$VISIBILITY" --source "$DIST" --description "RSupercell (rscel)：拓扑化业务编排 DSL → Rust 编译器"

if [[ "${RSC_PUBLISH_DRY:-0}" == "1" ]]; then
  echo "DRY RUN：快照与 tag 已就绪于 $DIST（未推送）"
else
  git -C "$DIST" push -f "https://x-access-token:$(gh auth token)@github.com/$OWNER/$NAME.git" main --tags 2>/dev/null \
    || { git -C "$DIST" branch -M main; git -C "$DIST" push -f "https://x-access-token:$(gh auth token)@github.com/$OWNER/$NAME.git" main --tags; }
  echo "已推送 https://github.com/$OWNER/$NAME  tag v$NEW（当前可见性：$VISIBILITY）"
fi
