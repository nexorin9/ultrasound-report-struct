//! 公开数据来源清单。
//!
//! **来源原则**：公开、可直连、无需凭证、无患者可识别明细。
//!
//! - 负语料来自 **NCBI PMC 开放获取子集**（放射 / 影像个案报告），
//!   走 E-utilities 的 `efetch` 接口，取文章号在 [`PMC_IDS`] 里**固化**，
//!   固化是为了让语料可复现：重跑拿到的是同一批文章、同一份清单。
//! - 单位码来自 **UCUM** 官方发布仓库的 essential 子集（`ucum-essence.xml`），
//!   用来核对本项目的单位码（`mm` / `cm` / `m` / `mL` / `L` / `cm3`）确实是 UCUM 码。
//! - 部位编码来自 **NCBI MeSH** 描述符（`Kidney` → `D007668` 等），
//!   用来核对本项目的规范部位键（`kidney.left` 等）挂得到公开的医学术语编码体系上。
//!
//! **完整性校验**靠 [`SourceSpec::pinned_sha256`] 与 [`SourceSpec::pinned_bytes`]：
//! 上游内容一旦变更（换版本、改编码），校验立刻不通过并报出来 —— 不静默接受新内容。

/// 内置样本集版本：仓库内已固化的公开数据快照。
///
/// 断网降级时报告里会带上这个版本号，让运维知道「现在用的是哪一版内置数据」。
pub const BUILTIN_SAMPLE_SET_VERSION: &str = "public-negatives-2026.10";

/// 固化的 PMC 文章号（37 篇放射 / 影像个案报告）。
///
/// 每篇都满足：CC 许可、摘要含 FINDINGS / CONCLUSION 段、无泌尿系部位词。
/// 固化前逐篇用本模块的筛选逻辑核过，取回即得 37 份负语料。
pub const PMC_IDS: &[&str] = &[
    "PMC13617120",
    "PMC13622694",
    "PMC13623353",
    "PMC13624138",
    "PMC13630380",
    "PMC13630650",
    "PMC13631860",
    "PMC13633743",
    "PMC13641980",
    "PMC13644153",
    "PMC13647618",
    "PMC13652407",
    "PMC13652464",
    "PMC13653995",
    "PMC13654217",
    "PMC13654445",
    "PMC13654757",
    "PMC13655330",
    "PMC13655517",
    "PMC13657566",
    "PMC13657598",
    "PMC13662747",
    "PMC13668064",
    "PMC13668069",
    "PMC13668071",
    "PMC13674027",
    "PMC13675973",
    "PMC13682734",
    "PMC13685378",
    "PMC13687337",
    "PMC13688324",
    "PMC13688597",
    "PMC13688657",
    "PMC13700125",
    "PMC13711616",
    "PMC13715085",
    "PMC13718248",
];

/// PMC 取文接口（文章号在请求里拼，不写在清单常量里）。
pub const PMC_EFETCH_ENDPOINT: &str = "https://eutils.ncbi.nlm.nih.gov/entrez/eutils/efetch.fcgi";

/// UCUM essential 子集（官方发布仓库的 contents 接口）。
///
/// 走 contents 接口而不是 raw 地址：raw 在院内网 / 慢链路上常常拉不动，
/// contents 接口返回同一份文件（base64 编码），字节完全一致（sha256 可核对）。
pub const UCUM_ESSENCE_URL: &str =
    "https://api.github.com/repos/ucum-org/ucum/contents/ucum-essence.xml";

/// UCUM essential 子集的备用地址（raw）。
pub const UCUM_ESSENCE_RAW_URL: &str =
    "https://raw.githubusercontent.com/ucum-org/ucum/main/ucum-essence.xml";

/// UCUM 完整发布包（规范正文 + 完整码表）。
///
/// essential 子集只含最小必要单位（305 条，不含 mm / cm / mL），
/// 完整码表才有长度与容量单位 —— 两份合起来才够核对本项目的单位码。
pub const UCUM_SPEC_URL: &str = "https://api.github.com/repos/ucum-org/ucum/contents/ucum.xml";

/// UCUM 完整发布包的备用地址（raw）。
pub const UCUM_SPEC_RAW_URL: &str = "https://raw.githubusercontent.com/ucum-org/ucum/main/ucum.xml";

/// UCUM essential 子集的固定校验值（sha256 / 字节数）。
pub const UCUM_ESSENCE_SHA256: &str =
    "dfccea1b5dc284245ebae97edd1dc03c45864da4e87df55bc9851797b4fd0b61";
/// UCUM essential 子集的固定字节数。
pub const UCUM_ESSENCE_BYTES: u64 = 82776;
/// UCUM 完整发布包的固定校验值（sha256）。
pub const UCUM_SPEC_SHA256: &str =
    "26e0d3267d56cc01c01485923b18e03ba1e86d797dd6165c2697b52393fb2d86";
/// UCUM 完整发布包的固定字节数。
pub const UCUM_SPEC_BYTES: u64 = 411444;

/// NCBI MeSH 检索接口（找描述符）。
pub const MESH_ESEARCH_URL: &str = "https://eutils.ncbi.nlm.nih.gov/entrez/eutils/esearch.fcgi";

/// NCBI MeSH 摘要接口（取描述符编码与首选词）。
pub const MESH_ESUMMARY_URL: &str = "https://eutils.ncbi.nlm.nih.gov/entrez/eutils/esummary.fcgi";

/// 本项目部位词典挂靠的 MeSH 描述符检索词。
///
/// 只取三个：肾 / 输尿管 / 膀胱 —— 与本项目的泌尿系部位键一一对应。
pub const MESH_PART_TERMS: &[&str] = &["Kidney", "Ureter", "Urinary Bladder"];

/// 来源种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// PMC 开放获取个案报告 → 负语料。
    PmcNegatives,
    /// UCUM 单位码表 → 单位码核对清单。
    UnitCodeList,
    /// 公开术语编码（MeSH 描述符）→ 部位编码核对清单。
    BodyPartCodeList,
}

impl SourceKind {
    /// 来源种类标签（报告里显示用）。
    pub fn label(self) -> &'static str {
        match self {
            SourceKind::PmcNegatives => "负语料（放射个案报告）",
            SourceKind::UnitCodeList => "单位码表",
            SourceKind::BodyPartCodeList => "部位编码表",
        }
    }
}

/// 一个来源需要的**第二份原始文件**（部分来源要两份才完整）。
#[derive(Clone, Debug)]
pub struct SecondaryFile {
    /// 入口地址。
    pub url: String,
    /// 备用地址（主地址不通时用）。
    pub raw_url: String,
    /// 文件名（报错时点名用）。
    pub file_name: String,
    /// 固定 sha256。
    pub sha256: String,
    /// 固定字节数。
    pub bytes: u64,
}

/// 一个公开数据来源。
#[derive(Clone, Debug)]
pub struct SourceSpec {
    /// 来源标识（稳定，进日志与报告）。
    pub id: String,
    /// 来源种类。
    pub kind: SourceKind,
    /// 入口地址。
    pub url: String,
    /// 许可说明。
    pub license: String,
    /// 归属说明（谁发布的、数据从哪来）。
    pub attribution: String,
    /// 用途说明（这份数据在本项目里干什么用）。
    pub purpose: String,
    /// 固定校验值（原始文件 sha256 十六进制）；`None` 表示该来源不固定原始字节。
    pub pinned_sha256: Option<String>,
    /// 固定字节数；`None` 表示不固定。
    pub pinned_bytes: Option<u64>,
    /// 第二份原始文件；`None` 表示该来源只要一份。
    pub secondary: Option<SecondaryFile>,
}

/// 来源清单（按处理顺序排列）。
///
/// 做成函数而不是常量：测试要能把入口地址换成本地 HTTP 替身
/// （替身只在测试数据层，不是最终对接边界）。
pub fn sources() -> Vec<SourceSpec> {
    vec![
        SourceSpec {
            id: "pmc-radiology-case-reports".to_string(),
            kind: SourceKind::PmcNegatives,
            url: PMC_EFETCH_ENDPOINT.to_string(),
            license: "逐篇 CC 许可（by / by-nc / by-nc-nd 4.0），见 neg-*.txt 头部与 INDEX.csv"
                .to_string(),
            attribution: "NCBI PubMed Central 开放获取子集（PMC OA Subset）".to_string(),
            purpose: "负语料：非超声的自由文本报告片段，验证解析器不误抽成测项".to_string(),
            pinned_sha256: None,
            pinned_bytes: None,
            secondary: None,
        },
        SourceSpec {
            id: "ucum-essential-units".to_string(),
            kind: SourceKind::UnitCodeList,
            url: UCUM_ESSENCE_URL.to_string(),
            license: "UCUM 发布仓库 LICENSE.md（开放许可）".to_string(),
            attribution: "Regenstrief / UCUM 官方发布仓库".to_string(),
            purpose: "单位码核对清单：核对本项目单位码确实是 UCUM 码".to_string(),
            pinned_sha256: Some(UCUM_ESSENCE_SHA256.to_string()),
            pinned_bytes: Some(UCUM_ESSENCE_BYTES),
            secondary: Some(SecondaryFile {
                url: UCUM_SPEC_URL.to_string(),
                raw_url: UCUM_SPEC_RAW_URL.to_string(),
                file_name: "ucum.xml".to_string(),
                sha256: UCUM_SPEC_SHA256.to_string(),
                bytes: UCUM_SPEC_BYTES,
            }),
        },
        SourceSpec {
            id: "nlm-mesh-body-parts".to_string(),
            kind: SourceKind::BodyPartCodeList,
            url: MESH_ESEARCH_URL.to_string(),
            license: "NLM MeSH 公开数据（美国国立医学图书馆公共领域词表）".to_string(),
            attribution: "NCBI / NLM Medical Subject Headings（MeSH）".to_string(),
            purpose: "部位编码核对清单：把本项目规范部位键挂到公开的医学术语编码（MeSH UI）上"
                .to_string(),
            pinned_sha256: None,
            pinned_bytes: None,
            secondary: None,
        },
    ]
}
