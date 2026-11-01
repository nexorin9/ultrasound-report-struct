//! 强转内核第 3 层：**UCUM 码 ↔ 中文单位名往返 + 未知单位拒绝**的验收用例。
//!
//! 本文件守的是**名字层的口径**：
//!
//! - **双向往返**：中文名 → 码 → 中文名 与 码 → 中文名 → 码 都回到原处；
//! - **大小写不敏感**：`cm` / `CM` / `厘米` 归一到同一 UCUM 码；
//! - **未知即拒绝**：未知码与未知中文名返回 `None`，**不回落默认单位**。
//!
//! 另有一组**跨模块不变量**：名字层（[`ucum`]）与量纲层（[`units`]）的
//! **码集合必须相等** —— 两处各写一份必然漂移。
//!
//! [`ucum`]: ultrasound_report_struct::coerce::ucum
//! [`units`]: ultrasound_report_struct::coerce::units

mod coerce {
    pub mod ucum {
        use ultrasound_report_struct::coerce::path::{decide, CoercionPath};
        use ultrasound_report_struct::coerce::ucum::{
            canonical_code, code_of_zh, entries, is_known, ucum_name_en, zh_name,
        };
        use ultrasound_report_struct::coerce::units::{canonical_unit, catalog};

        /// 读公开码表（`assets/public/ucum/unit_crosswalk.csv`）。
        ///
        /// 格式：`project_code,project_family,ucum_code,ucum_name,ucum_class,match_type,note`
        /// —— 字段内不含 ASCII 逗号，按 `,` 直切即可。
        fn crosswalk_rows() -> Vec<Vec<String>> {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("assets/public/ucum/unit_crosswalk.csv");
            let text = std::fs::read_to_string(&path).expect("读公开单位码表 unit_crosswalk.csv");
            text.lines()
                .skip(1)
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.split(',').map(|s| s.trim().to_string()).collect())
                .collect()
        }

        #[test]
        fn 中文名到码再回中文名双向一致() {
            for e in entries() {
                let code = code_of_zh(e.zh).unwrap_or_else(|| panic!("{} 应查得到码", e.zh));
                assert_eq!(code, e.code, "{} 的中文名应回到自身", e.zh);
                let name = zh_name(code).unwrap_or_else(|| panic!("{} 应查得到中文名", e.code));
                assert_eq!(name, e.zh, "{} 的码应回到自身中文名", e.code);
                // 两跳往返。
                assert_eq!(zh_name(code_of_zh(e.zh).unwrap()), Some(e.zh));
                assert_eq!(code_of_zh(zh_name(e.code).unwrap()), Some(e.code));
            }
        }

        #[test]
        fn 码到中文名再回码双向一致() {
            for e in entries() {
                // 码（任意大小写写法）→ 中文名 → 码。
                for raw in [e.code, &e.code.to_ascii_uppercase(), &e.code.to_ascii_lowercase()] {
                    let name = zh_name(raw).unwrap_or_else(|| panic!("{} 应查得到中文名", raw));
                    assert_eq!(name, e.zh);
                    assert_eq!(code_of_zh(name), Some(e.code));
                }
            }
        }

        #[test]
        fn 厘米毫米毫升归一到同一码() {
            for raw in ["cm", "CM", "Cm", "厘米"] {
                assert_eq!(canonical_code(raw), Some("cm"), "{} 应归一到 cm", raw);
                assert_eq!(zh_name(raw), Some("厘米"));
            }
            for raw in ["mm", "MM", "毫米"] {
                assert_eq!(canonical_code(raw), Some("mm"), "{} 应归一到 mm", raw);
                assert_eq!(zh_name(raw), Some("毫米"));
            }
            for raw in ["mL", "ml", "ML", "毫升"] {
                assert_eq!(canonical_code(raw), Some("mL"), "{} 应归一到 mL", raw);
                assert_eq!(zh_name(raw), Some("毫升"));
            }
        }

        #[test]
        fn 未知码与未知中文名一律拒绝() {
            for raw in ["xx", "cc", "g", "in", "mm2", "cm2", "", "  "] {
                assert_eq!(canonical_code(raw), None, "{} 不该查得到码", raw);
                assert_eq!(zh_name(raw), None, "{} 不该查得到中文名", raw);
                assert!(!is_known(raw), "{} 不该是已知单位", raw);
            }
            for raw in ["寸", "英尺", "公毫", "公克"] {
                assert_eq!(code_of_zh(raw), None, "{} 不该查得到码", raw);
            }
        }

        #[test]
        fn 未知单位不回落默认单位() {
            // 目录层、换算层、判定层三处结论一致：未知单位一律拿不到数值。
            assert!(canonical_unit("xx").is_none());
            assert!(canonical_unit("cc").is_none());
            let d = decide(3.2, None, "xx");
            assert_eq!(d.path, CoercionPath::None);
            assert!(d.value_si.is_none());
        }

        #[test]
        fn 名字层与量纲层的码集合相等() {
            let name_codes: Vec<&str> = entries().iter().map(|e| e.code).collect();
            let unit_codes: Vec<&str> = catalog().iter().map(|s| s.code).collect();
            for code in &name_codes {
                assert!(
                    unit_codes.contains(code),
                    "名字层有 {} 而量纲层没有 —— 两处目录漂移了",
                    code
                );
                assert!(
                    canonical_unit(code).is_some(),
                    "量纲层应认得 {}",
                    code
                );
            }
            for code in &unit_codes {
                assert!(
                    name_codes.contains(code),
                    "量纲层有 {} 而名字层没有 —— 两处目录漂移了",
                    code
                );
            }
        }

        #[test]
        fn 英文名与公开码表逐条对得上() {
            let rows = crosswalk_rows();
            assert!(!rows.is_empty(), "公开码表应有数据行");
            for row in &rows {
                assert_eq!(row.len(), 7, "码表列数不对：{:?}", row);
                let project_code = &row[0];
                let ucum_code = &row[2];
                let ucum_name = &row[3];
                let match_type = &row[5];
                let entry = entries()
                    .iter()
                    .find(|e| e.code == project_code)
                    .unwrap_or_else(|| panic!("码表里的 {} 应在名字层目录内", project_code));
                // 收录的单位：码与英文名都要对上。
                if !ucum_code.is_empty() {
                    assert_eq!(ucum_code, entry.code, "{} 的 UCUM 码不一致", project_code);
                    assert_eq!(
                        ucum_name_en(entry.code),
                        Some(ucum_name.as_str()),
                        "{} 的 UCUM 英文名与公开码表不一致",
                        project_code
                    );
                } else {
                    // 未收录的码：不编英文名。
                    assert!(
                        entry.ucum_name_en.is_none(),
                        "{} 在公开码表里未收录，不该有英文名",
                        project_code
                    );
                    assert_eq!(ucum_name_en(entry.code), None);
                    assert!(
                        match_type.contains("未收录"),
                        "{} 的收录方式应标未收录，实为 {}",
                        project_code,
                        match_type
                    );
                }
            }
        }
    }
}
