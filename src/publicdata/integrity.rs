//! 完整性校验：sha256 + 字节数 + 校验清单读写。
//!
//! **校验清单格式**用 `sha256sum` 兼容格式（`<hex>  <相对路径>`），
//! 这样运维可以直接用系统自带工具复核，不必依赖本程序：
//!
//! ```bash
//! sha256sum -c assets/public/CHECKSUMS.sha256
//! ```
//!
//! 清单里的路径相对**项目根**，所以上面这条命令要在项目根目录执行。

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// 一条校验记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChecksumEntry {
    /// 文件 sha256（十六进制小写）。
    pub sha256: String,
    /// 文件字节数。
    pub bytes: u64,
    /// 相对项目根的路径（清单里就写这个）。
    pub rel_path: String,
}

/// 算 sha256 十六进制。
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

/// 写校验清单（`sha256sum` 兼容格式，按路径排序）。
pub fn write_checksums(path: &Path, entries: &[ChecksumEntry]) -> std::io::Result<()> {
    let mut sorted: Vec<&ChecksumEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    let mut text = String::new();
    for entry in sorted {
        text.push_str(&entry.sha256);
        text.push_str("  ");
        text.push_str(&entry.rel_path);
        text.push('\n');
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, text)
}

/// 读校验清单。
///
/// 清单里的路径相对**项目根**，字节数按项目根解析文件后回填（清单格式本身是
/// `sha256sum` 兼容的，只带哈希；字节数从文件读出来，缺文件则记 0 并在核对时报缺）。
pub fn read_checksums(path: &Path, root: &Path) -> std::io::Result<Vec<ChecksumEntry>> {
    let text = fs::read_to_string(path)?;
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, "  ");
        let sha = parts.next().unwrap_or("").trim().to_string();
        let rel = parts.next().unwrap_or("").trim().to_string();
        if sha.is_empty() || rel.is_empty() {
            continue;
        }
        let bytes = fs::metadata(root.join(&rel)).map(|m| m.len()).unwrap_or(0);
        entries.push(ChecksumEntry { sha256: sha, bytes, rel_path: rel });
    }
    Ok(entries)
}

/// 校验清单里的每个文件是否与记录一致。
///
/// 返回不匹配项的说明（缺文件、字节数不同、sha256 不同），全部一致时返回空表。
pub fn verify_entries(root: &Path, entries: &[ChecksumEntry]) -> Vec<String> {
    let mut problems = Vec::new();
    for entry in entries {
        let path = root.join(&entry.rel_path);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(err) => {
                problems.push(format!("{}：文件缺失或读不到（{}）", entry.rel_path, err));
                continue;
            }
        };
        if bytes.len() as u64 != entry.bytes {
            problems.push(format!(
                "{}：字节数不符（清单 {}，实际 {}）",
                entry.rel_path,
                entry.bytes,
                bytes.len()
            ));
            continue;
        }
        let actual = sha256_hex(&bytes);
        if actual != entry.sha256 {
            problems.push(format!(
                "{}：sha256 不符（清单 {}，实际 {}）",
                entry.rel_path, entry.sha256, actual
            ));
        }
    }
    problems
}

/// 核对下载内容的 sha256 与字节数是否与清单里的固定值一致。
///
/// 不一致返回说明，写明**来源标识与文件名** —— 这是「上游内容变了」的唯一出口。
pub fn check_pinned(
    source_id: &str,
    file_name: &str,
    bytes: &[u8],
    pinned_sha256: Option<&str>,
    pinned_bytes: Option<u64>,
) -> Result<(), String> {
    if let Some(expected_bytes) = pinned_bytes {
        if bytes.len() as u64 != expected_bytes {
            return Err(format!(
                "来源 {} 的文件 {} 字节数不符：期望 {}，实际 {}",
                source_id,
                file_name,
                expected_bytes,
                bytes.len()
            ));
        }
    }
    if let Some(expected_sha) = pinned_sha256 {
        let actual = sha256_hex(bytes);
        if actual != expected_sha {
            return Err(format!(
                "来源 {} 的文件 {} sha256 不符：期望 {}，实际 {}（上游内容已变更，需人工复核后重新固定）",
                source_id, file_name, expected_sha, actual
            ));
        }
    }
    Ok(())
}

/// 递归收集目录下的文件（相对项目根），用于生成校验清单。
pub fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_与已知值一致() {
        // 空串的 sha256 是固定已知值。
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn 固定校验值同时核对字节数与哈希() {
        let body = b"abc";
        assert!(check_pinned("s", "f.txt", body, Some(sha256_hex(body).as_str()), Some(3)).is_ok());
        let err = check_pinned("s", "f.txt", body, Some("dead"), Some(3)).unwrap_err();
        assert!(err.contains("sha256 不符"), "{}", err);
        assert!(err.contains("f.txt"), "{}", err);
        let err = check_pinned("s", "f.txt", body, None, Some(9)).unwrap_err();
        assert!(err.contains("字节数不符"), "{}", err);
    }

    #[test]
    fn 校验清单写出后可读回并逐条核对() {
        let dir = std::env::temp_dir().join("urs-integrity-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("negatives")).unwrap();
        fs::write(dir.join("negatives/a.txt"), b"alpha").unwrap();
        fs::write(dir.join("negatives/b.txt"), b"beta").unwrap();

        let entries = vec![
            ChecksumEntry {
                sha256: sha256_hex(b"alpha"),
                bytes: 5,
                rel_path: "negatives/a.txt".to_string(),
            },
            ChecksumEntry {
                sha256: sha256_hex(b"beta"),
                bytes: 4,
                rel_path: "negatives/b.txt".to_string(),
            },
        ];
        let list = dir.join("CHECKSUMS.sha256");
        write_checksums(&list, &entries).unwrap();

        // sha256sum 兼容格式：<hex>  <path>
        let text = fs::read_to_string(&list).unwrap();
        assert!(text.lines().all(|l| l.contains("  ")), "{}", text);
        assert_eq!(text.lines().count(), 2);

        let read = read_checksums(&list, &dir).unwrap();
        assert_eq!(read.len(), 2);
        assert!(verify_entries(&dir, &read).is_empty());
    }

    #[test]
    fn 文件被改动后校验给出问题清单() {
        let dir = std::env::temp_dir().join("urs-integrity-test-2");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.txt"), b"alpha").unwrap();
        let entries = vec![ChecksumEntry {
            sha256: sha256_hex(b"alpha"),
            bytes: 5,
            rel_path: "a.txt".to_string(),
        }];
        assert!(verify_entries(&dir, &entries).is_empty());
        fs::write(dir.join("a.txt"), b"tampered").unwrap();
        let problems = verify_entries(&dir, &entries);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("a.txt"), "{:?}", problems);
    }
}
