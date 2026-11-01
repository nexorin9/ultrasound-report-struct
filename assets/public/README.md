# 公开数据资产（负语料 + 口径核对清单）

本目录是**随仓库发布的公开数据快照**。它的作用是给本项目的两条主张提供可复核的外部证据：

1. **不误抽**：非超声科别的自由文本不会被抽成超声测项 → `negatives/`
2. **口径不是自编的**：本项目自报的单位码与部位键挂得到公开编码体系 → `ucum/`、`mesh/`

## 目录内容

| 路径 | 内容 | 条数 | 来源 | 许可 |
|---|---|---|---|---|
| `negatives/neg-001.txt` … `neg-037.txt` | 放射 / 影像个案报告的 FINDINGS / CONCLUSION 段（负语料） | 37 份 | NCBI PubMed Central 开放获取子集（PMC OA Subset） | 逐篇 CC 许可（by / by-nc / by-nc-nd 4.0），见每份文件头与 `negatives/INDEX.csv` |
| `negatives/INDEX.csv` | 每份负语料的来源、期刊、DOI、许可与许可链接 | 37 行 | 同上 | 同上 |
| `ucum/unit_codes.csv` | UCUM 单位码全表（essential 策展子集 + 完整发布包码表合并） | 555 行 | Regenstrief / UCUM 官方发布仓库 | UCUM 发布仓库 LICENSE.md（开放许可） |
| `ucum/unit_crosswalk.csv` | 本项目单位码逐条对照 UCUM 码（精确 / 完整码表 / 未收录） | 6 行 | 同上 | 同上 |
| `mesh/body_part_codes.csv` | 本项目规范部位键逐条对照 MeSH 描述符编码（MeSH UI） | 6 行 | NCBI / NLM Medical Subject Headings | NLM MeSH 公开数据 |

| `CHECKSUMS.sha256` | 上述产物的 sha256 清单（`sha256sum` 兼容格式，路径相对项目根） | — | 本项目生成 | — |

## 怎么取回 / 怎么校验

```bash
# 联网取回（脚本化下载 + 完整性校验）
scripts/fetch_public_data.sh

# 断网 / 院内隔离网：只校验仓库内置样本集，不发起任何网络请求，退出码 0
cargo run -- fetch-public-data --offline

# 复核内置样本集（系统自带工具即可，不必依赖本程序）
sha256sum -c assets/public/CHECKSUMS.sha256
```

三条纪律：

1. **完整性校验** —— 下载后逐文件核对 sha256 与字节数；不匹配即以非零码退出，并写明**来源标识与文件名**（上游内容变更不静默接受）。
2. **断网降级** —— 网络不可用时改用本目录的内置样本集，输出明确降级原因（网络错误串 + 改用的样本集版本），退出码 0。断网不是失败。
3. **来源可溯** —— 每份负语料文件头标注来源与许可；`INDEX.csv` 给出逐篇的许可链接。

## 负语料的取材口径

- **只要 CC 许可的文章**，逐篇标注许可链接（`INDEX.csv` 的 `license` / `license_url` 两列）。
- **只要摘要里同时有「所见」段与「结论」段的文章** —— 段形态对齐影像报告口径。
- **只取期刊公开的摘要正文**，不含任何患者可识别明细。
- **不含泌尿系部位词**（kidney / renal / bladder / ureter / nephro / pyelo …）：本项目的部位词典限定泌尿系，负语料要验证的是「非目标科别的自由文本不被误抽」；泌尿系词汇的边界由院内负样例与对拍基准单独覆盖。
- 文章号在 `src/publicdata/manifest.rs` 的 `PMC_IDS` 里**固化**：重跑取回的是同一批文章、同一份清单。

## 口径核对清单的用法

- `ucum/unit_crosswalk.csv` 逐条给出本项目单位码（`mm` / `cm` / `m` / `mL` / `L` / `cm3`）与 UCUM 码的关系：
  - `L` 标「精确」——它就在 UCUM 官方策展子集（essential）里；
  - `mm` / `cm` / `m` / `mL` 标「完整码表」——essential 子集只含 305 条最小必要单位（不含长度与容量单位），这四个码取自 UCUM 完整发布包的码表；
  - `cm3` 标「未收录」——**本项目用简化码表示复合量纲**，UCUM 两份发布包都没有这个字面码，换算按**院内约定**（1 cm³ = 1 mL），不是法定计量标准。
- `mesh/body_part_codes.csv` 把规范部位键（`kidney.left` / `kidney.right` / `kidney.bilateral` / `ureter.left` / `ureter.right` / `bladder`）挂到 MeSH 描述符编码上。

> **取舍说明**：部位编码原本打算取影像侧（TCIA NBIA / DICOM PS3.6）的那套码。
> 实测本机到 `services.cancerimagingarchive.net` 不可达（连接超时），
> 而 `dicom.nema.org` 的 PS3.6 docbook 只含数据元素注册表、不含代码表，
> 因此改用同样公开、无需凭证、且稳定可达的 **NCBI MeSH** 描述符编码做部位编码核对。
> 清单只做**核对**，不替院内口径做决定：换算因子与侧别规则仍是院内约定的经验口径。
