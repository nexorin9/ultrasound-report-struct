//! 公开数据获取：脚本化下载 + 完整性校验 + 断网降级。
//!
//! **为什么要有这一层**：真实院内超声报告拿不到（涉及患者可识别明细），
//! 解析器「认不出的不猜、不误抽」这条主张就没有外部证据可对。
//! 本模块从**公开、可直连、无需凭证**的数据源取回一批非超声自由文本
//! （放射 / 影像个案报告的 FINDINGS / CONCLUSION 段）作负语料，
//! 再取公开的单位码表与部位编码表作口径核对清单。
//!
//! **三条纪律**：
//!
//! 1. **完整性校验**：下载后逐文件核对 sha256 与字节数；
//!    不匹配即以非零码退出，并写明**来源标识与文件名** —— 上游内容变更不静默接受。
//! 2. **断网降级**：网络不可用时改用仓库内置样本集，输出明确降级原因
//!    （网络错误串 + 改用的样本集版本），退出码 0 —— 断网不是失败。
//! 3. **来源可溯**：每份负语料文件头标注来源与许可，清单见 `assets/public/README.md`。
//!
//! 主路径：`fetch_public_data(&opts) → FetchReport`。
//! 离线模式只校验仓库内置样本集，不发起任何网络请求。

pub mod checklist;
pub mod integrity;
pub mod manifest;
pub mod pmc;
pub mod report;

use std::path::{Path, PathBuf};
use std::time::Duration;

use report::{FetchReport, OutcomeStatus, SourceOutcome};

pub use manifest::{
    BUILTIN_SAMPLE_SET_VERSION, MESH_ESEARCH_URL, MESH_ESUMMARY_URL, MESH_PART_TERMS, PMC_IDS,
    SecondaryFile, SourceKind, SourceSpec, UCUM_ESSENCE_URL, sources,
};

/// 获取选项。
#[derive(Clone, Debug)]
pub struct FetchOptions {
    /// 公开数据落地目录（默认 `<项目根>/assets/public`）。
    pub dest: PathBuf,
    /// 离线模式：跳过全部网络请求，只校验仓库内置样本集。
    pub offline: bool,
    /// 单次请求超时秒数。
    pub timeout_secs: u64,
}

impl Default for FetchOptions {
    fn default() -> FetchOptions {
        FetchOptions {
            dest: PathBuf::from("assets/public"),
            offline: false,
            timeout_secs: 120,
        }
    }
}

/// 单个来源的失败。
#[derive(Clone, Debug)]
pub enum SourceError {
    /// 网络不可用（超时、连接失败、非 2xx）。
    Network(String),
    /// 完整性校验不通过。
    Integrity(String),
    /// 本地文件读写失败。
    Io(String),
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceError::Network(m) => write!(f, "网络不可用：{}", m),
            SourceError::Integrity(m) => write!(f, "完整性校验不通过：{}", m),
            SourceError::Io(m) => write!(f, "本地读写失败：{}", m),
        }
    }
}

/// 整体失败（结构性错误：目录不可写、项目根定不出来）。
#[derive(Debug)]
pub enum PublicDataError {
    /// 本地 IO 失败。
    Io(std::io::Error),
    /// 项目根无法确定（写不出相对路径的校验清单）。
    ProjectRoot,
}

impl std::fmt::Display for PublicDataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PublicDataError::Io(e) => write!(f, "本地读写失败：{}", e),
            PublicDataError::ProjectRoot => write!(
                f,
                "定不出项目根（校验清单里的路径要相对项目根）：请把落地目录放在 <项目根>/assets/public 下"
            ),
        }
    }
}

/// 执行一次公开数据获取（按默认来源清单）。
pub fn fetch_public_data(opts: &FetchOptions) -> Result<FetchReport, PublicDataError> {
    fetch_public_data_with(opts, &sources())
}

/// 执行一次公开数据获取（来源清单可替换 —— 测试用本地 HTTP 替身）。
pub fn fetch_public_data_with(
    opts: &FetchOptions,
    sources: &[SourceSpec],
) -> Result<FetchReport, PublicDataError> {
    let dest = &opts.dest;
    let root = project_root(dest).ok_or(PublicDataError::ProjectRoot)?;
    let mut report = FetchReport::new(BUILTIN_SAMPLE_SET_VERSION);

    if opts.offline {
        report.offline = true;
        report.push(verify_builtin(dest, &root)?);
        let entries = collect_artifacts(dest, &root)?;
        report.checksums = entries.len();
        report.degradation_note = format!(
            "离线降级：使用仓库内置样本集（版本 {}）；原因：按 --offline 显式请求，本次未发起网络请求",
            BUILTIN_SAMPLE_SET_VERSION
        );
        return Ok(report);
    }

    let mut degraded_notes: Vec<String> = Vec::new();
    for spec in sources {
        match fetch_source(spec, dest, opts) {
            Ok(outcome) => report.push(outcome),
            Err(SourceError::Network(detail)) => {
                let note = format!(
                    "来源 {} 网络不可用，已改用仓库内置样本集（版本 {}）；网络错误：{}",
                    spec.id, BUILTIN_SAMPLE_SET_VERSION, detail
                );
                degraded_notes.push(note.clone());
                report.push(SourceOutcome {
                    id: spec.id.to_string(),
                    kind_label: spec.kind.label().to_string(),
                    status: OutcomeStatus::Degraded,
                    files: 0,
                    detail: note,
                });
            }
            Err(SourceError::Integrity(detail)) => {
                report.push(SourceOutcome {
                    id: spec.id.to_string(),
                    kind_label: spec.kind.label().to_string(),
                    status: OutcomeStatus::Failed,
                    files: 0,
                    detail,
                });
            }
            Err(SourceError::Io(detail)) => {
                report.push(SourceOutcome {
                    id: spec.id.to_string(),
                    kind_label: spec.kind.label().to_string(),
                    status: OutcomeStatus::Failed,
                    files: 0,
                    detail,
                });
            }
        }
    }

    // 重算校验清单（只覆盖随仓库发布的产物；原始下载件不入清单）。
    let entries = collect_artifacts(dest, &root)?;
    let list_path = dest.join("CHECKSUMS.sha256");
    integrity::write_checksums(&list_path, &entries).map_err(PublicDataError::Io)?;
    report.checksums = entries.len();

    if !degraded_notes.is_empty() {
        report.degradation_note = format!("离线降级：{}", degraded_notes.join("；"));
    }
    Ok(report)
}

/// 取一个来源。
fn fetch_source(
    spec: &SourceSpec,
    dest: &Path,
    opts: &FetchOptions,
) -> Result<SourceOutcome, SourceError> {
    match spec.kind {
        SourceKind::PmcNegatives => pmc::fetch_corpus(spec, dest, PMC_IDS, opts.timeout_secs),
        SourceKind::UnitCodeList => checklist::fetch_unit_codes(spec, dest, opts.timeout_secs),
        SourceKind::BodyPartCodeList => {
            checklist::fetch_body_part_codes(spec, dest, opts.timeout_secs)
        }
    }
}

/// 离线模式：校验仓库内置样本集。
fn verify_builtin(dest: &Path, root: &Path) -> Result<SourceOutcome, PublicDataError> {
    let list_path = dest.join("CHECKSUMS.sha256");
    let id = "builtin-sample-set".to_string();
    if !list_path.exists() {
        // 首次运行：没有校验清单就现场生成一份，随后即可逐条核对。
        let entries = collect_artifacts(dest, root)?;
        if entries.is_empty() {
            return Ok(SourceOutcome {
                id,
                kind_label: "内置样本集".to_string(),
                status: OutcomeStatus::Degraded,
                files: 0,
                detail: format!(
                    "内置样本集为空（{} 下没有产物），离线降级无可用样本",
                    dest.display()
                ),
            });
        }
        integrity::write_checksums(&list_path, &entries).map_err(PublicDataError::Io)?;
        return Ok(SourceOutcome {
            id,
            kind_label: "内置样本集".to_string(),
            status: OutcomeStatus::Degraded,
            files: entries.len(),
            detail: format!(
                "首次生成校验清单：{}（{} 个文件）",
                list_path.display(),
                entries.len()
            ),
        });
    }

    let entries = integrity::read_checksums(&list_path, root).map_err(PublicDataError::Io)?;
    let problems = integrity::verify_entries(root, &entries);
    if problems.is_empty() {
        Ok(SourceOutcome {
            id,
            kind_label: "内置样本集".to_string(),
            status: OutcomeStatus::Degraded,
            files: entries.len(),
            detail: format!(
                "内置样本集校验通过（sha256 + 字节数逐条核对，{} 个文件）；版本 {}",
                entries.len(),
                BUILTIN_SAMPLE_SET_VERSION
            ),
        })
    } else {
        Ok(SourceOutcome {
            id,
            kind_label: "内置样本集".to_string(),
            status: OutcomeStatus::Failed,
            files: entries.len(),
            detail: format!("内置样本集校验不通过：{}", problems.join("；")),
        })
    }
}

/// 收集随仓库发布的产物（不含原始下载件与校验清单本身）。
fn collect_artifacts(dest: &Path, root: &Path) -> Result<Vec<integrity::ChecksumEntry>, PublicDataError> {
    let mut files = Vec::new();
    integrity::collect_files(dest, &mut files).map_err(PublicDataError::Io)?;
    let mut entries = Vec::new();
    for path in files {
        let rel = match path.strip_prefix(root) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        // 原始下载件是缓存（可重新取回），不进校验清单；校验清单自身也不进。
        if rel.starts_with("assets/public/raw/") || rel.ends_with("CHECKSUMS.sha256") {
            continue;
        }
        let bytes = std::fs::read(&path).map_err(PublicDataError::Io)?;
        entries.push(integrity::ChecksumEntry {
            sha256: integrity::sha256_hex(&bytes),
            bytes: bytes.len() as u64,
            rel_path: rel,
        });
    }
    Ok(entries)
}

/// 定项目根：落地目录形如 `<项目根>/assets/public`。
pub fn project_root(dest: &Path) -> Option<PathBuf> {
    let public = dest.file_name()?;
    if public != "public" {
        return None;
    }
    let assets = dest.parent()?;
    if assets.file_name()? != "assets" {
        return None;
    }
    assets.parent().map(|p| p.to_path_buf())
}

/// 发起一次 HTTP GET，返回原始字节。
///
/// 错误串里带上 URL 与原因 —— 降级报告要靠它说清「为什么没取到」。
pub fn http_get(url: &str, timeout_secs: u64) -> Result<Vec<u8>, SourceError> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(timeout_secs))
        .user_agent("ultrasound-report-struct/0.1 (public-data-fetch)")
        .build();
    match agent.get(url).call() {
        Ok(response) => {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut response.into_reader(), &mut buf)
                .map_err(|e| SourceError::Network(format!("{}：读取响应失败（{}）", url, e)))?;
            Ok(buf)
        }
        Err(ureq::Error::Status(code, _response)) => Err(SourceError::Network(format!(
            "{}：HTTP {}",
            url,
            code
        ))),
        Err(ureq::Error::Transport(err)) => {
            Err(SourceError::Network(format!("{}：{}", url, err)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 项目根按assets_public推导() {
        let root = project_root(Path::new("/tmp/proj/assets/public")).unwrap();
        assert_eq!(root, PathBuf::from("/tmp/proj"));
        assert!(project_root(Path::new("/tmp/proj/data")).is_none());
        assert!(project_root(Path::new("/tmp/proj/assets/samples")).is_none());
    }

    #[test]
    fn 校验清单不含原始下载件与清单自身() {
        let dir = std::env::temp_dir().join("urs-publicdata-artifacts");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("assets/public/negatives")).unwrap();
        std::fs::create_dir_all(dir.join("assets/public/raw")).unwrap();
        std::fs::write(dir.join("assets/public/negatives/a.txt"), b"a").unwrap();
        std::fs::write(dir.join("assets/public/raw/big.xml"), b"b").unwrap();
        std::fs::write(dir.join("assets/public/CHECKSUMS.sha256"), b"").unwrap();
        let root = dir.clone();
        let entries = collect_artifacts(&dir.join("assets/public"), &root).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].rel_path.ends_with("negatives/a.txt"), "{:?}", entries);
    }
}
