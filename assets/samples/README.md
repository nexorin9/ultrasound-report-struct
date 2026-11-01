# 语料资产说明（脱敏对照集）

本目录是超声报告结构化系统的**回归语料**：一批脱敏报告 + 人工标注的期望结果 + 强转路径对照表。
解析内核、强转内核、对拍引擎的每条断言都应能落到这里的某个文件上 —— 「跑通」不等于「对」，
**对照表才是判据**。

## 目录内容

| 路径 | 内容 | 规模 |
|---|---|---|
| `reports/sample-uro-01.txt` … `sample-uro-30.txt` | 脱敏泌尿系超声自由文本报告 | 30 份 |
| `hl7/sample-hl7-01.hl7` … `sample-hl7-03.hl7` | 脱敏 HL7 v2 `ORU^R01` 报文 | 3 份（含 1 份缺 OBX 段异常样例） |
| `annotations.csv` | 自由文本语料的**人工标注期望结果** | 62 行（条目 53 + 失败 9） |
| `hl7_annotations.csv` | HL7 样例的期望字段与期望条目数 | 3 行 |
| `coercion_table.csv` | **强转路径对照表**（值 + 单位 → 期望路径 / 期望数值 / 期望单位码） | 37 行 |
| `compare_baseline.csv` | **差值基准回归集**（当前测值 + RIS 历史 → 期望状态 / 期望差值 / 期望方向 / 期望命中条数 / 期望失败码） | 34 行 |
| `lexer_samples/lexer-01.txt` … `lexer-07.txt` | **词法层**样例正文（中文 / 英文数字 / 单位 / 全角 / 生僻字 / 复合量纲 / 倍率写法） | 7 份 |
| `lexer_tokens.csv` | **词法层人工标注**的 token 序列（类别 / 词面 / 原字面 / 起始字符光标 / 结束字符光标） | 42 行（10 类 token 全覆盖） |
| `locate_cases/c01-…txt` … `c09-…txt` | **位置层**样例正文（中文多字节 / 全角括号 / 空行 / 超长行 / 生僻字 / 制表符 / 末行无换行 / CRLF / 空文本） | 9 份 |
| `locate_positions.csv` | **位置层人工标注**的「字符光标 → 期望行列」（含越界与位置未知的零值用例） | 45 行 |
| `inbox/sample-uro-001/`、`inbox/sample-uro-002/` | 最小投递样例（供 `ingest --inbox` 冒烟） | 2 份 |
| `inbox-hl7/sample-hl7-01/`、`inbox-hl7/sample-hl7-03/` | HL7 v2 `ORU^R01` 投递样例（`report.hl7`；供 `ingest --inbox` 冒烟 HL7 通道） | 2 份 |

## 脱敏纪律

- 标识一律用**占位**或**类目词**：`PID-EXAMPLE`（病历号）、`脱敏`（检查日期）、`sample-uro-NN`（检查号）。
- 语料内**不出现**真实住院号、患者姓名、批号、身份证号；`assets/samples` 全目录无 8 位以上连续数字
  （自检：`grep -rE '[0-9]{8,}' assets/samples` 无命中）。
- HL7 报文的 `MSH-7` / `OBR-7` 采用 HL7 v2 允许的**降精度 TS**（`YYYYMM`），避免在脱敏语料里留下
  一串看起来像真实标识的长数字。
- 报告正文的元数据行不含数字：现版本解析内核尚未把「报告头」与「所见」分段，头部里的日期数字会被当成
  「缺单位的数值」，污染失败清单。因此语料用「检查日期：脱敏」这类**类目词**占位，测量值只出现在所见段。

## `annotations.csv` 列义

| 列 | 含义 |
|---|---|
| `study_no` | 检查号（占位；与 `reports/` 下文件名主干一致） |
| `report_file` | 相对本目录的报告文件路径 |
| `kind` | `entry`（结构化条目）/ `failure`（失败条目） |
| `part_key` | 规范部位键（`kidney.left` / `bladder` …）；部位未命中时为空 |
| `item_code` | 测项码（`kidney.ap_diameter` / `bladder.residual_urine_volume` …）；测项未命中时为空 |
| `value_raw` / `unit_raw` | 报告原值 / 原单位（保留原文写法，如 `CM`、`厘米`、`cc`） |
| `value_si` / `unit_code` | 归一值（长度 mm / 容量 mL）与单位码；**查不到合法强转路径时为空** |
| `coercion_path` | `UnitConvert` / `DirectScale` / `Composite` / `None` |
| `line` / `column` | 原文 **1 基**字符光标（未知为 0，不编造） |
| `error_code` | 该行携带的稳定错误码（`entry` 行为条目上挂的 note，`failure` 行为失败码） |
| `raw_snippet` | 失败条目的原文片段（供人眼定位） |

行序按**原文阅读顺序**（行 → 列）排列；同位置的 `entry` 行排在 `failure` 行之前。
`entry` 与 `failure` 两类行之和 = 语料的**预期产出条目数**，即 `annotations.csv` 的数据行数（62 行）。

> **`value_raw` 的记法**：内核当前把原值按**数值**记录（原文本词面尚未单独保留），因此本表按数值记：
> 原文 `0.0cm` 记 `0`、原文 `0.123456cm` 记 `0.123`（与台账 CSV 的数字格式一致）。待内核改为保留原值词面后，
> 对应行同步为原文写法即可，其余行不受影响。
>
> **`ITEM_DICT_MISSING`（测项未命中）** 在当前内核里是挂在条目上的 **note**，不是独立失败行；
> 待内核把测项未命中提升为失败条目后，本表补对应的 `failure` 行即可，其余行不受影响。
>
> **`sample-uro-15.txt`** 是**零条目**样例（只有叙述、无测量值），因此它在 `annotations.csv` 里没有行 ——
> 这是预期结果，不是漏登记。

> **口径说明**：`ITEM_DICT_MISSING`（测项未命中）在当前内核里是挂在条目上的 **note**，不是独立失败行；
> 待内核把测项未命中提升为失败条目后，本表补对应的 `failure` 行即可，其余行不受影响。

## `coercion_table.csv` 列义与四类路径

`value_raw` 是**报告原值词面**（不是先 parse 过的数值），`unit_raw` 是报告原单位词面。

| 路径 | 判定依据 | 例（表内） |
|---|---|---|
| `UnitConvert` | 单位族内直接换算 | `3.2` + `cm` → `32` `mm`；`1` + `L` → `1000` `mL` |
| `DirectScale` | 原值词面**自带倍率**（倍率是原文写明的，不是猜的） | `3.2×2` + `cm` → `64` `mm` |
| `Composite` | 复合量纲，需「量纲还原 + 单位换算」两步 | `3.2` + `cm3` → `3.2` `mL` |
| `None` | 查不到合法路径 → **不产出数值**，不按某单位兜底 | `2.8` + `cc` → 空 |

- `UnitConvert` 含同族同单位的行（`85` + `mm` → `85` `mm`）：族内换算因子为 1 仍走 `UnitConvert`。
- 中文单位名（`厘米` / `毫升`）与拉丁字母大小写（`CM` / `ml`）归一到同一 UCUM 码。
- `DirectScale` 行的 `value_raw` 写成 `<数值>×<倍数>`，是本表约定的**唯一**倍率写法；实现按此判定。
- 四类路径各有 ≥ 7 行，覆盖率可逐条断言。

## `compare_baseline.csv` 列义（对拍层判据 · oracle = RIS 真实历史值）

这张表把「差值 / 方向 / 三态 / 降级」四件事钉成**可逐条复核**的断言 ——
差值不是自己跟自己比，而是「报告这次的测值」对「RIS 里该患者同侧部位上次的历史值」。

| 列 | 含义 |
|---|---|
| `case_id` | 案例号（`CB-001` …，表内唯一，便于口头指认） |
| `expect_state` | 期望对拍状态：`has_history` / `first_time` / `ambiguous` / `comparison_skipped` |
| `patient_id` | 患者主索引（占位 `PID-EXAMPLE` / `PID-002`） |
| `part_key` / `item_code` | 规范部位键 / 规范测项码（当前测值所在位置） |
| `current_value` / `current_unit` | **报告原值原单位**（`3.2` / `cm`）—— 与生产路径一致，先走强转内核归一再相减 |
| `upstream_error` | 上游失败语义（`none` / `upstream_unavailable` / `auth_failed` / `bad_response` / `not_configured`）；非 `none` 即「这一轮没比成」 |
| `history` | **RIS 会返回的那种历史**，多条用 `;` 分隔，每条 `检查号\|患者\|部位\|测项码\|数值\|单位` |
| `expect_delta` / `expect_direction` | 期望差值与方向（`增大` / `减小` / `持平`）；**只有 `has_history` 有值** |
| `expect_hit_count` | 期望命中条数（`has_history` = 1，其余 = 0，歧义 = ≥ 2） |
| `expect_error_code` / `expect_detail_code` | 期望失败码 / 明细归类（`first_time` 无 —— **首次不是失败**） |
| `note` | 这一行在讲什么（口径说明；失败报告里一并打印） |

口径五则：

1. **空位一律写 `none`**（不写空串、不写 `-`）—— 少一个逗号就会串列，串列比没有表更糟。
2. `history` 的部位与测项可以写**院内别名**（`左肾` / `前后径`）—— 归一后应当对上；
   某一格写空即「RIS 侧这一项没给」（缺值 / 缺单位 / 缺部位 / 没带患者标识）。
3. **只有 `has_history` 才产差值**；`first_time`（首次）、`ambiguous`（多历史歧义）、
   `comparison_skipped`（RIS 够不着）三态**一律不产差值** —— 尤其后两者：
   歧义时任取其一可能指错对象；跳过时「跟上次一样」这句话压根没有依据。
4. **首次不是失败**：首次检查是正常业务结果，不带失败码；
   `comparison_skipped` 才带失败码（`RIS_UPSTREAM_UNAVAILABLE` / `RIS_AUTH_FAILED` /
   `RIS_BAD_RESPONSE` / `RIS_NOT_CONFIGURED`）——「没比成」与「没有历史」是两件事。
5. 表里**不出现拟真标识**：检查号一律 `S-HIST-NNN`、患者主索引一律 `PID-*`。

`tests/compare_baseline.rs` 逐条断言这张表（34 行、四类状态各有 ≥ 2 行）；
任一行对不上即产出带**行号 / 输入 / 期望 / 实际**四列的报告，运维能直接定位到漂移的那一行。

## HL7 样例约定

- 段与字段：`MSH-9`（`ORU^R01`）、`PID-3`（患者主索引）、`OBR-3`（检查号）、`OBR-7`（检查日期）、
  `OBX-5`（所见文本）。`sample-hl7-03.hl7` **缺 OBX 段**，用于「缺段 → 显式错误码」用例。
- 多条 `OBX-5` 的所见文本按**换行连接**成一段所见（`hl7_annotations.csv` 的 `expected_findings_text`
  即连接后的文本），连接后的文本再喂给同一套解析内核 —— 不另开一条抽取路径。

## 建表时发现的内核缺口（不写进标注表，留给对应任务收口）

标注表只登记**语义正确**的期望结果；下列两处内核缺口因此**没有**以「错」的期望写进表里，
而是对应语料写法被避开，另在任务清单里收口：

1. ~~**复合量纲在词法层整体识别不了**~~ —— **已收口（词法层 Task 12）**：`cm3` / `cm³` / `cm^3` / `㎤`
   现在整块切成一个单位 token（指数属于单位），`2.5cm³` 走 `Composite` 路径得 `2.5 mL`，
   不再被读成 `2.5cm`（→25mm）。复合量纲可以进自由文本语料了 ——
   `lexer_samples/lexer-03.txt` 已含 `cm³` 写法，`coercion_table.csv` 的 `Composite` 行仍是单位级判据。
2. **部位缺位时的上下文继承**：某行没有部位词时，当前行会继承上一行的部位 —— 例如
   「右侧输尿管内径 0.6cm」后面跟「前列腺大小 3.9cm」，3.9cm 会被记到**右侧输尿管**头上，
   与「宁可不比不错比」相悖。标注表不登记这种归属，故语料里未保留该写法；
   部位抽取任务应收口为「本行无部位 → 不继承」，并补该样例。

## `lexer_tokens.csv` 列义（词法层判据）

| 列 | 含义 |
|---|---|
| `sample_id` | 样例编号（与 `lexer_samples/<sample_id>.txt` 一一对应） |
| `token_index` | 该样例内 token 的序号（从 1 起，连续） |
| `kind` | token 类别（`Number` / `Unit` / `Punct` / `Multiply` / `Modifier` / `Part` / `Item` / `Section` / `Term` / `Word`） |
| `text` | **归一后**的词面（全角数字转半角、方块兼容单位与上标指数转 ASCII、单位转小写） |
| `raw` | **原字面**（失败清单回显原文用；全角标点原样保留） |
| `start` / `end` | 1 基**字符**光标（UTF-8 多字节按字符计，不按字节）；`end` 为闭区间末位 |

`tests/lexer_tokens.rs` 逐条断言这两份资产的一致性；`tests/corpus_annotations.rs` 则管抽取层
（30 份语料 × `annotations.csv`）。两层判据分工明确：**词法切错字符光标，抽取层就会把
失败条目指到错误的行列**。

## `locate_positions.csv` 列义（位置层判据）

| 列 | 含义 |
|---|---|
| `case_id` | 样例编号（与 `locate_cases/<case_id>.txt` 一一对应） |
| `text_file` | 相对本目录的样例正文路径 |
| `char_index` | 1 基**字符**光标（UTF-8 多字节按字符计，不按字节）；`0` = 位置未知 |
| `expect_line` / `expect_column` | 期望的 1 基行列；位置不可确定时**留 0**（不编造） |
| `note` | 该行在人工核对时看的锚点（哪一个字符 / 为什么是这个行列） |

口径三则（与 `src/parse/locate.rs` 的实现同源，改一处必须改另一处）：

1. **换行**：`\n` 起新行；CRLF 里的 `\r` 算**上一行的普通字符**（落在该行最后一列）。
2. **制表符**：算**一列**（按字符计，不按显示宽度）—— 列号是**可复现的定位锚点**，
   不是排版坐标。
3. **位置不可确定**（光标 0 / 越过文本末尾）→ 行列都是 **0**，**不编造**。

`tests/locate_positions.rs` 逐条断言这份矩阵，并额外断言两件事：
**四类失败分支**（单位缺失 / 部位未命中 / 测项未命中 / 数值不可解析）都产出带位置的失败条目；
**失败清单里的每一条都能反查回原文**（行列 → 字符光标 → 原文片段逐字对上）——
清单可被独立复核，不必相信解析器。

