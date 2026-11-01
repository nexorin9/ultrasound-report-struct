//! 获取报告：逐来源状态 + 降级原因 + 渲染。
//!
//! 运维看这个报告要能立刻答出三个问题：
//! 1. **哪些数据拿回来了**（来源 / 文件数）；
//! 2. **哪些没拿回来、为什么**（降级原因里带网络错误串与改用的样本集版本）；
//! 3. **有没有校验不过的**（完整性失败要逐条点名来源与文件）。

use std::fmt::Write as _;

/// 单个来源的处理状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutcomeStatus {
    /// 成功取回并写入产物。
    Fetched,
    /// 降级：改用仓库内置样本集（网络不可用等情况）。
    Degraded,
    /// 失败：完整性校验不通过，产物未采用。
    Failed,
}

impl OutcomeStatus {
    /// 状态标签。
    pub fn label(self) -> &'static str {
        match self {
            OutcomeStatus::Fetched => "已取回",
            OutcomeStatus::Degraded => "已降级",
            OutcomeStatus::Failed => "校验失败",
        }
    }
}

/// 单个来源的处理结果。
#[derive(Clone, Debug)]
pub struct SourceOutcome {
    /// 来源标识。
    pub id: String,
    /// 来源种类标签。
    pub kind_label: String,
    /// 处理状态。
    pub status: OutcomeStatus,
    /// 产物文件数。
    pub files: usize,
    /// 说明（成功时写产物位置；降级时写网络错误串；失败时写校验不符详情）。
    pub detail: String,
}

/// 整体获取报告。
#[derive(Clone, Debug)]
pub struct FetchReport {
    /// 是否离线模式（跳过全部网络请求）。
    pub offline: bool,
    /// 内置样本集版本。
    pub builtin_version: String,
    /// 逐来源结果。
    pub outcomes: Vec<SourceOutcome>,
    /// 校验清单覆盖的文件数。
    pub checksums: usize,
    /// 降级说明（离线模式或来源降级时给出）。
    pub degradation_note: String,
}

impl FetchReport {
    /// 新建报告。
    pub fn new(builtin_version: &str) -> FetchReport {
        FetchReport {
            offline: false,
            builtin_version: builtin_version.to_string(),
            outcomes: Vec::new(),
            checksums: 0,
            degradation_note: String::new(),
        }
    }

    /// 记一个来源结果。
    pub fn push(&mut self, outcome: SourceOutcome) {
        self.outcomes.push(outcome);
    }

    /// 是否有校验失败（决定退出码）。
    pub fn has_failure(&self) -> bool {
        self.outcomes.iter().any(|o| o.status == OutcomeStatus::Failed)
    }

    /// 是否有来源走了降级路径。
    pub fn has_degradation(&self) -> bool {
        self.outcomes.iter().any(|o| o.status == OutcomeStatus::Degraded)
    }

    /// 渲染成多行文本（终端直接打印）。
    pub fn render(&self) -> String {
        let mut out = String::new();
        let mode = if self.offline { "离线模式" } else { "联网模式" };
        let _ = writeln!(out, "公开数据获取（{}）", mode);
        let _ = writeln!(
            out,
            "内置样本集版本：{}",
            self.builtin_version
        );
        for o in &self.outcomes {
            let _ = writeln!(
                out,
                "[{}] {} · {} · 产物 {} 个",
                o.status.label(),
                o.id,
                o.kind_label,
                o.files
            );
            if !o.detail.is_empty() {
                let _ = writeln!(out, "    {}", o.detail);
            }
        }
        if !self.degradation_note.is_empty() {
            let _ = writeln!(out, "离线降级：{}", self.degradation_note);
        }
        let _ = writeln!(out, "校验清单覆盖文件：{} 个", self.checksums);
        out
    }
}
