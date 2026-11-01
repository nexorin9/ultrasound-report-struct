#!/usr/bin/env bash
# 脚本化取回公开数据：负语料（放射个案报告 FINDINGS / CONCLUSION 段）+ 口径核对清单（单位码 / 部位编码）。
#
# 三条纪律（与 `ultrasound-report-struct fetch-public-data` 一致）：
#   1. 完整性校验：下载后逐文件核对 sha256 与字节数，不匹配即非零退出并写明来源与文件名；
#   2. 断网降级：网络不可用时改用仓库内置样本集，输出降级原因（网络错误串 + 样本集版本），退出码 0；
#   3. 来源可溯：每份负语料文件头标注来源与许可。
#
# 用法：
#   scripts/fetch_public_data.sh                # 联网取回
#   scripts/fetch_public_data.sh --offline      # 不触网，只校验仓库内置样本集
#   scripts/fetch_public_data.sh --dest assets/public --timeout 120
#
# 退出码：0 = 取回成功或已降级；1 = 完整性校验不通过；2 = 用法错误。

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

cd "${PROJECT_DIR}"

# 已构建的可执行文件优先；没有就退回 cargo run。
BIN="${URS_BIN:-${PROJECT_DIR}/target/release/ultrasound-report-struct}"
if [[ ! -x "${BIN}" ]]; then
  BIN="${PROJECT_DIR}/target/debug/ultrasound-report-struct"
fi
if [[ ! -x "${BIN}" ]]; then
  echo "准备可执行文件：cargo build" >&2
  cargo build --quiet
  BIN="${PROJECT_DIR}/target/debug/ultrasound-report-struct"
fi

echo "公开数据获取：${BIN}"
"${BIN}" fetch-public-data "$@"
