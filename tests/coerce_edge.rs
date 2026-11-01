//! 强转内核第 8 层：**边界与异常用例集**（失败不静默改数据）。
//!
//! 这一集守的是**底线**：认不出来的写法**不许被悄悄改成另一个数**。
//!
//! 超声科现在的做法是「工作站导出表格 → 人工抄测值」——抄错了没人发现，
//! 因为抄错的数字**看着完全正常**。本产品要把每一次「认不出来」都显式记下来：
//!
//! | 边界 | 现在的兜底 | 本产品要求 |
//! |---|---|---|
//! | 缺单位 | 按 cm 猜一个 | `UNIT_MISSING`，不产出数值，原值留痕 |
//! | 未知单位 | 按 mm 猜一个 | `COERCION_NO_PATH`，不产出归一值 |
//! | 复合量纲 | 当成 cm³ 的长度 | 走 `Composite`，容量口径 mL |
//! | 超长数值 | 截断成另一个数 | `VALUE_UNPARSEABLE`，原样保留 |
//! | 空行 / 空报告 | —— | 不报错、不产出条目 |
//! | 零值 | 当成坏值丢掉 | 零是合法测量值，照常产出 |
//! | **负值** | **符号被丢掉，当成正数** | **原值带号留痕，不产出归一值** |
//! | 部位词典未命中 | 猜一个部位 | `PART_UNRESOLVED`，不猜部位 |
//! | 重复测项 | 去重丢一条 | 两条都保留，各带各的位置 |
//!
//! **负值这一条是被用例集当场撞出来的**：词法层把 `-` 切成独立标点 token，
//! 数值层若不看一眼，「-3.2cm」会被读成 **32mm** —— 台账里是个正常得不能再正常的数字，
//! 对拍也照常进行，只是比错了对象。现在数值层把符号**保留进数值**，
//! 交给强转内核判失败（原值 `-3.2` 留在条目里，不产出归一值）。
//!
//! **与其它用例集的分工**：
//! - `tests/coerce_table.rs` 守**换算口径**（同一条值该走哪条路径、数值该是多少）；
//! - `tests/corpus_annotations.rs` 守**整份脱敏语料**的逐条对拍；
//! - 本文件守**边界与异常**：坏写法不许被悄悄兜底成好数字。
//!
//! 用例集挂在 `coerce::edge` 路径下 —— 一条命令跑完整集：
//!
//! ```bash
//! cargo test coerce::edge
//! ```

mod coerce {
    mod edge {
        use ultrasound_report_struct::coerce::path::CoercionPath;
        use ultrasound_report_struct::parse::{
            parse_report, DetailCode, DocumentKind, ErrorCode, Failure, ParseOutcome,
        };

        /// 失败条目（用例入口：断言只关心第一条失败）。
        fn failure(out: &ParseOutcome) -> &Failure {
            assert!(
                !out.failures.is_empty(),
                "本用例应当产出失败条目，实际 0 条（条目 {} 条）",
                out.items.len()
            );
            &out.failures[0]
        }

        /// 失败码集合（用例入口：一个报告可能同时落好几类失败）。
        fn codes(out: &ParseOutcome) -> Vec<ErrorCode> {
            out.failures.iter().map(|f| f.code).collect()
        }

        // -----------------------------------------------------------------
        // 九类边界：每类一条用例，逐条断言「错误码 + 产出 + 位置」
        // -----------------------------------------------------------------

        #[test]
        fn 缺单位不产出数值只留一条失败() {
            let out = parse_report("左肾积水 3.2");

            // 不猜单位 → 不产出数值，也不产出条目。
            assert!(out.items.is_empty(), "缺单位不产出条目");
            assert_eq!(out.failures.len(), 1);

            let f = failure(&out);
            assert_eq!(f.code, ErrorCode::UnitMissing);
            assert_eq!(f.raw_snippet, "3.2", "片段是原值词面");
            assert_eq!(f.line, 1);
            assert_eq!(f.column, 6, "第 6 列是数值所在列");
            assert!(f.detail_code().is_none(), "缺单位没有细分归类");

            // 策略：本条不落台账、不产出数值 —— 但**不阻断整份报告**。
            assert!(f.policy().drops_item);
            assert!(f.policy().drops_value);
            assert!(!f.policy().blocks_report);
        }

        #[test]
        fn 未知单位保留原值但不产出归一值() {
            let out = parse_report("左肾积水 3.2xx");

            // 条目在（原值原单位留痕），但**没有归一值** —— 不按 mm 兜底。
            assert_eq!(out.items.len(), 1);
            let item = &out.items[0];
            assert_eq!(item.value_raw, Some(3.2), "原值保留");
            assert_eq!(item.unit_raw.as_deref(), Some("xx"), "原单位保留");
            assert_eq!(item.value_si, None, "未知单位不产出归一值");
            assert_eq!(item.unit_code, None, "未知单位不产出单位码");
            assert_eq!(item.coercion_path, CoercionPath::None);

            let f = failure(&out);
            assert_eq!(f.code, ErrorCode::CoercionNoPath);
            assert_eq!(f.line, 1);
            assert_eq!(f.column, 6);
            assert_eq!(f.raw_snippet, "3.2");

            // 策略：条目在（原值留痕）、数值不在、不参与对拍。
            assert!(!f.policy().drops_item);
            assert!(f.policy().drops_value);
            assert!(f.policy().excluded_from_comparison);
        }

        #[test]
        fn 复合量纲走合法路径不落失败() {
            // 「cm³」是**体积**：按长度读会差 1000 倍（2.5cm³ ≠ 25mm）。
            let out = parse_report("膀胱 2.5cm³");

            assert!(out.failures.is_empty(), "复合量纲有合法路径，不该落失败");
            assert_eq!(out.items.len(), 1);
            let item = &out.items[0];
            assert_eq!(item.value_raw, Some(2.5));
            assert_eq!(item.unit_raw.as_deref(), Some("cm³"));
            assert_eq!(item.value_si, Some(2.5), "1 cm³ = 1 mL");
            assert_eq!(item.unit_code.as_deref(), Some("mL"), "容量口径");
            assert_eq!(item.coercion_path, CoercionPath::Composite);
            assert_eq!(item.part_key.as_deref(), Some("bladder"));
        }

        #[test]
        fn 超长数值原样保留不截断() {
            // 16 位数字串多半是扫描件 / 复制粘贴粘连出来的。
            let long = "1".repeat(16);
            let out = parse_report(&format!("左肾积水 {}cm", long));

            assert!(out.items.is_empty(), "超长数值不产出条目");
            assert_eq!(out.failures.len(), 1);

            let f = failure(&out);
            assert_eq!(f.code, ErrorCode::ValueUnparseable);
            assert_eq!(
                f.detail_code(),
                Some(DetailCode::ValueTooLong),
                "细分归类要能分出「太长」"
            );
            // **不截断**：片段是完整的 16 位原文，一个字都不少。
            assert_eq!(f.raw_snippet, format!("{}cm", long));
            assert_eq!(f.raw_snippet.len(), 18);
        }

        #[test]
        fn 空行不吞行号也不报错() {
            // 两行空行夹在中间：值仍在第 4 行 —— 空行不吞行号，
            // 运维照着「第 4 行第 4 列」能准确翻到原文。
            let out = parse_report("超声所见\n\n\n左肾 3.2cm");

            assert!(out.failures.is_empty(), "空行不是失败");
            assert_eq!(out.items.len(), 1);
            let item = &out.items[0];
            assert_eq!(item.value_si, Some(32.0));
            assert_eq!(item.line, 4, "空行不吞行号：值在第 4 行");
            assert_eq!(item.column, 4);
        }

        #[test]
        fn 零值是合法测量值() {
            // 残余尿量 0 mL 是**真实结果**，不是坏值 —— 丢掉它等于漏抄一条结果。
            let out = parse_report("左肾 0cm");

            assert!(out.failures.is_empty(), "零值不是失败");
            assert_eq!(out.items.len(), 1);
            let item = &out.items[0];
            assert_eq!(item.value_raw, Some(0.0));
            assert_eq!(item.value_si, Some(0.0));
            assert_eq!(item.unit_code.as_deref(), Some("mm"));
            assert_eq!(item.coercion_path, CoercionPath::UnitConvert);
        }

        #[test]
        fn 负值不静默当成正数() {
            let out = parse_report("左肾 -3.2cm");

            // 条目在，**原值带号**留痕；没有归一值。
            assert_eq!(out.items.len(), 1);
            let item = &out.items[0];
            assert_eq!(item.value_raw, Some(-3.2), "负号不许丢");
            assert_eq!(item.unit_raw.as_deref(), Some("cm"));
            assert_eq!(item.value_si, None, "负值不是测量值，不产出归一值");
            assert_eq!(item.unit_code, None);
            assert_eq!(item.coercion_path, CoercionPath::None);

            let f = failure(&out);
            assert_eq!(f.code, ErrorCode::CoercionNoPath);
            assert_eq!(f.raw_snippet, "-3.2", "片段含负号");
            assert_eq!(f.line, 1);
            assert_eq!(f.column, 4, "指回负号那一列");
            assert!(f.policy().drops_value, "没有归一值");
            assert!(!f.policy().drops_item, "原值仍在条目里留痕");
        }

        #[test]
        fn 区间连字符不误判成负号() {
            // 「3.2-4.1cm」是区间：4.1 不是 -4.1。
            let out = parse_report("左肾 3.2-4.1cm");

            assert_eq!(out.items.len(), 1);
            let item = &out.items[0];
            assert_eq!(item.value_raw, Some(4.1), "区间上限是正数");
            assert_eq!(item.value_si, Some(41.0));
            assert_eq!(item.coercion_path, CoercionPath::UnitConvert);
            assert!(
                !codes(&out).contains(&ErrorCode::CoercionNoPath),
                "区间不是负值，不该报强转无路径"
            );
        }

        #[test]
        fn 部位词典未命中不猜部位() {
            // 「胆囊」不在泌尿系口径内 —— 猜一个部位就是拿错对象做对拍。
            let out = parse_report("胆囊 3.2cm");

            assert_eq!(out.items.len(), 1);
            let item = &out.items[0];
            assert_eq!(item.part_key, None, "未命中不得产出部位键");
            assert_eq!(item.item_code, None, "部位未归一时合成键不成立");
            assert_eq!(item.value_si, Some(32.0), "值本身可用，仍留痕");

            let f = failure(&out);
            assert_eq!(f.code, ErrorCode::PartUnresolved);
            assert_eq!(f.raw_snippet, "胆囊", "指回部位词本身");
            assert_eq!(f.line, 1);
            assert_eq!(f.column, 1);
            assert!(f.policy().excluded_from_comparison, "不参与对拍");
            assert!(!f.policy().drops_item, "条目仍在台账里留痕");
        }

        #[test]
        fn 重复测项两条都保留() {
            // 同一部位同一测项出现两次：去重丢一条就等于漏抄一次测量。
            let out = parse_report("左肾前后径 2.1cm\n左肾前后径 3.0cm");

            assert!(out.failures.is_empty());
            assert_eq!(out.items.len(), 2, "两条都要在");
            assert_eq!(out.items[0].item_code.as_deref(), Some("kidney.ap_diameter"));
            assert_eq!(out.items[1].item_code.as_deref(), Some("kidney.ap_diameter"));
            assert_eq!(out.items[0].value_si, Some(21.0));
            assert_eq!(out.items[1].value_si, Some(30.0));
            assert_eq!(out.items[0].line, 1);
            assert_eq!(out.items[1].line, 2, "两条各带各的位置");
        }

        // -----------------------------------------------------------------
        // 部分失败 / 空报告 / 超长字段
        // -----------------------------------------------------------------

        #[test]
        fn 部分失败时好条目照常产出() {
            // 一份报告里有一条好值、一条坏值：坏值进失败清单，好值照常进台账。
            let out = parse_report("左肾积水 3.2cm\n右肾积水 3.2.5cm");

            assert_eq!(out.items.len(), 1, "好值照常产出");
            let item = &out.items[0];
            assert_eq!(item.part_key.as_deref(), Some("kidney.left"));
            assert_eq!(item.value_raw, Some(3.2));
            assert_eq!(item.value_si, Some(32.0));
            assert_eq!(item.unit_code.as_deref(), Some("mm"));
            assert_eq!(item.coercion_path, CoercionPath::UnitConvert);
            assert_eq!(item.line, 1);

            assert_eq!(out.failures.len(), 1, "坏值只坏自己那一条");
            let f = failure(&out);
            assert_eq!(f.code, ErrorCode::ValueUnparseable);
            assert_eq!(f.detail_code(), Some(DetailCode::ValueMultipleDots));
            assert_eq!(f.raw_snippet, "3.2.5cm");
            assert_eq!(f.line, 2, "坏值指回自己那一行");
            assert_eq!(f.column, 6);
        }

        #[test]
        fn 空报告不报错不产出条目() {
            for text in ["", "   ", "\n\n\t "] {
                let out = parse_report(text);
                assert!(out.items.is_empty(), "[{text:?}] 不该产出条目");
                assert!(out.failures.is_empty(), "[{text:?}] 不该产出失败条目");
                assert_eq!(out.doc_kind, DocumentKind::Empty);
            }
        }

        #[test]
        fn 超长字段不产出第二个台账行() {
            // 超长值不落条目 → 台账里不会有「被截断成另一个数」的那一行。
            let long = "9".repeat(40);
            let out = parse_report(&format!("左肾积水 {}cm\n左肾积水 3.2cm", long));

            assert_eq!(out.items.len(), 1, "只有好值那一行进台账");
            assert_eq!(out.items[0].value_si, Some(32.0));
            assert_eq!(out.failures.len(), 1);
            assert_eq!(out.failures[0].code, ErrorCode::ValueUnparseable);
            assert_eq!(out.failures[0].detail_code(), Some(DetailCode::ValueTooLong));
            assert_eq!(out.failures[0].raw_snippet, format!("{}cm", long));
            // 好值那一行不受坏值牵连。
            assert_eq!(out.items[0].line, 2);
        }

        #[test]
        fn 九类边界各自落在登记过的错误码上() {
            // 边界不许长出自由文本分支 —— 每一条都要落回错误码表里的码。
            let cases: Vec<(&str, ErrorCode)> = vec![
                ("左肾积水 3.2", ErrorCode::UnitMissing),
                ("左肾积水 3.2xx", ErrorCode::CoercionNoPath),
                ("左肾积水 3.2.5cm", ErrorCode::ValueUnparseable),
                ("胆囊 3.2cm", ErrorCode::PartUnresolved),
                ("膀胱前后径 2.1cm", ErrorCode::ItemDictMissing),
            ];
            for (text, code) in cases {
                let out = parse_report(text);
                assert!(
                    codes(&out).contains(&code),
                    "[{text}] 应当落 {code:?}，实际 {:?}",
                    codes(&out)
                );
                for f in &out.failures {
                    assert!(
                        ErrorCode::ALL.contains(&f.code),
                        "[{text}] 落了个表里没有的码 {:?}",
                        f.code
                    );
                }
            }
        }
    }
}
