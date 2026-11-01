//! 工作流第 1 层：状态机与非法迁移守卫。
//!
//! **这一层解决的接缝**：超声工作站把报告投进 `inbox/`，本产品解析出测量值、
//! 跟 RIS 同侧历史对出差值 —— 然后呢？
//!
//! **产物的语义是「待确认的草稿」，不是已生效的医疗文书**。
//! 这一层就是这句话的**代码化**：报告要走到 `written_back`，
//! **必须**先经过 `awaiting_review`（超声医师确认过）。
//! 跳过确认直接写回 —— 被守卫拒绝。
//!
//! **为什么状态机要单独立一层，而不是在业务代码里 if 一把**：
//!
//! - **谁在推进状态**：状态推进是**多方**的事 —— 守护进程推进 `received → parsed`，
//!   医师回执推进 `awaiting_review → written_back / rejected`，
//!   出站写失败把状态推进 `pending_writeback`。三条路径写在一处，
//!   迟早有人写出第二条绕过确认的路径。
//! - **终态不可再推进**：`written_back` / `rejected` / `parse_failed` 是终态。
//!   已回写的报告被二次处理改成别的状态 = 库里的结论与 EMR 里的不一致。
//! - **非法迁移必须**显式报错**，不能静默忽略**：静默忽略的代价是
//!   「医师驳回了，报告却显示已回写」—— 这种不一致现场根本查不出来。
//!
//! **状态取值与状态库一一对应**（`reports.status` 落库字段）：
//! 状态字符串跨版本不变（改了等于把历史台账行作废）。
//!
//! | 状态 | 中文含义 | 是不是终态 |
//! |---|---|---|
//! | `received` | 已接收（正文读到了，还没解析） | 否 |
//! | `parsed` | 已解析（条目已落台账） | 否 |
//! | `awaiting_review` | **待医师确认**（草稿态，人工确认点） | 否 |
//! | `pending_writeback` | 回写待重试（EMR 写失败，退避重试中） | 否 |
//! | `written_back` | 已回写 EMR（终态） | **是** |
//! | `rejected` | 医师驳回（终态） | **是** |
//! | `parse_failed` | 解析失败 / 实体绑定有歧义被拒（终态） | **是** |
//!
//! **本层不做的三件事**（留给后续任务，边界写在这里免得越界）：
//!
//! - **审计留痕**（每次推进记一条）由 Task 34 落地（[`crate::workflow::audit`]）；
//! - **幂等键与事务边界**（推进与写台账同生共死）由 Task 35 落地；
//! - **落库时的状态推进**（谁在什么时候调 [`transition`]）由 Task 40（确认回执端点）
//!   与 Task 41（回写退避重试）落地。

use std::fmt;

/// **报告状态**（与状态库 `reports.status` 的取值一一对应）。
///
/// 状态是**落库字段**，取值必须稳定 —— 状态变了等于历史记录作废。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Status {
    /// `received` —— 已接收：正文读到了，还没解析出条目。
    Received,
    /// `parsed` —— 已解析：条目已落台账（**草稿**，还没人确认）。
    Parsed,
    /// `awaiting_review` —— **待医师确认**：草稿等人看，人工确认点。
    AwaitingReview,
    /// `pending_writeback` —— 回写待重试：EMR 写失败，退避重试中，**等人介入**。
    PendingWriteback,
    /// `written_back` —— 已回写 EMR（**终态**）。
    WrittenBack,
    /// `rejected` —— 医师驳回（**终态**）。
    Rejected,
    /// `parse_failed` —— 解析失败 / 实体绑定有歧义被拒（**终态**）。
    ParseFailed,
}

impl Status {
    /// **全部状态**（穷举清单）。
    pub const ALL: [Status; 7] = [
        Status::Received,
        Status::Parsed,
        Status::AwaitingReview,
        Status::PendingWriteback,
        Status::WrittenBack,
        Status::Rejected,
        Status::ParseFailed,
    ];

    /// 状态字符串（**落库字段**，与 `reports.status` 一致，跨版本不变）。
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Received => "received",
            Status::Parsed => "parsed",
            Status::AwaitingReview => "awaiting_review",
            Status::PendingWriteback => "pending_writeback",
            Status::WrittenBack => "written_back",
            Status::Rejected => "rejected",
            Status::ParseFailed => "parse_failed",
        }
    }

    /// 状态 ↔ 字符串往返（未知字符串返回 `None`）。
    ///
    /// 从库里读回状态时用它 —— 读到表里没有的状态就是**数据不对**，
    /// 宁可返回 `None` 让调用方显式处理，也不硬套一个默认状态。
    pub fn lookup(raw: &str) -> Option<Status> {
        Status::ALL.iter().copied().find(|s| s.as_str() == raw)
    }

    /// 中文说明（台账 / 企微推送 / 运行摘要直接展示）。
    pub fn message(self) -> &'static str {
        match self {
            Status::Received => "已接收，待解析",
            Status::Parsed => "已解析出条目，待医师确认",
            Status::AwaitingReview => "待医师确认（草稿，未回写）",
            Status::PendingWriteback => "回写失败，待重试（需人工介入）",
            Status::WrittenBack => "已回写 EMR",
            Status::Rejected => "医师驳回",
            Status::ParseFailed => "解析失败或实体绑定有歧义，未落台账",
        }
    }

    /// 是否为**终态**（不可再推进）。
    ///
    /// 终态三种：已回写 / 已驳回 / 解析失败。
    /// 终态再推进 = 库里的结论与 EMR 里的不一致，且现场查不出来。
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Status::WrittenBack | Status::Rejected | Status::ParseFailed
        )
    }

    /// 是否为**起始态**。
    pub fn is_initial(self) -> bool {
        self == Status::Received
    }

    /// 是否**正等人确认**（唯一的人工确认点）。
    pub fn awaits_confirmation(self) -> bool {
        self == Status::AwaitingReview
    }

    /// 是否**仍在流转中**（非终态 —— 还会被推进）。
    pub fn is_open(self) -> bool {
        !self.is_terminal()
    }
}

/// **迁移表**：每个状态允许推进到的目标状态（有序、无重复）。
///
/// **这张表是唯一真源** —— [`allowed_targets`] / [`can_transition`] / [`transition`]
/// 三个入口都从这里读。各写一份判断，迟早会漂移（一处加了 `parsed → written_back`，
/// 另一处没加，跳过确认的口子就漏出去了）。
///
/// 逐条读法：
///
/// - `received → parsed`：解析出条目；
/// - `received → parse_failed`：整份解析不出来（编码判不出 / HL7 缺段 / 没有可用条目）；
/// - `parsed → awaiting_review`：条目齐了，交给医师看（**草稿态**）；
/// - `parsed → parse_failed`：解析出来了但**实体绑定有歧义**（一个检查号对多患者 / 多部位）
///   —— 宁可整份退回，也不猜（`spec.md` §4 权威数据源维度）；
/// - `awaiting_review → written_back`：医师确认 + EMR 回写成功；
/// - `awaiting_review → rejected`：医师驳回；
/// - `awaiting_review → pending_writeback`：医师确认了，但 EMR 写失败 —— 退避重试；
/// - `pending_writeback → written_back`：重试成功；
/// - `pending_writeback → pending_writeback`：**停留重试态**（重试还没成功）；
/// - 三个终态：**没有任何出边**。
///
/// **注意 `parsed → written_back` 不在表里** —— 那就是「跳过医师确认直接回写」，
/// 是本产品明确不做的事（产物语义是待确认草稿，不自动回写、不自动签名）。
pub const TRANSITION_TABLE: &[(Status, &[Status])] = &[
    (
        Status::Received,
        &[Status::Parsed, Status::ParseFailed],
    ),
    (
        Status::Parsed,
        &[Status::AwaitingReview, Status::ParseFailed],
    ),
    (
        Status::AwaitingReview,
        &[
            Status::WrittenBack,
            Status::Rejected,
            Status::PendingWriteback,
        ],
    ),
    (
        Status::PendingWriteback,
        &[Status::WrittenBack, Status::PendingWriteback],
    ),
    (Status::WrittenBack, &[]),
    (Status::Rejected, &[]),
    (Status::ParseFailed, &[]),
];

/// 某个状态允许推进到的目标状态（空切片 = 终态）。
pub fn allowed_targets(from: Status) -> &'static [Status] {
    TRANSITION_TABLE
        .iter()
        .find(|(status, _)| *status == from)
        .map(|(_, targets)| *targets)
        .unwrap_or(&[])
}

/// 这次推进**允不允许**（不产生错误，只给判断）。
pub fn can_transition(from: Status, to: Status) -> bool {
    allowed_targets(from).contains(&to)
}

/// **状态推进守卫**（本层的核心入口）。
///
/// 合法迁移返回目标状态；非法迁移返回**显式错误** —— 不静默忽略、不兜底放行。
///
/// 两种非法：
///
/// - [`TransitionErrorKind::TerminalState`] —— 终态再推进（状态已经落地了）；
/// - [`TransitionErrorKind::IllegalTransition`] —— 表里没有这条边
///   （典型：`parsed → written_back`，**跳过医师确认**）。
pub fn transition(from: Status, to: Status) -> Result<Status, TransitionError> {
    // 终态先判：终态的报错要说的是「别动终态」，
    // 而不是笼统一句「非法迁移」—— 运维看到的是两回事。
    if from.is_terminal() {
        return Err(TransitionError {
            from,
            to,
            kind: TransitionErrorKind::TerminalState,
        });
    }
    if can_transition(from, to) {
        Ok(to)
    } else {
        Err(TransitionError {
            from,
            to,
            kind: TransitionErrorKind::IllegalTransition,
        })
    }
}

/// **只判不推**（调用方只想知道这次推进合不合法，不想拿目标状态）。
pub fn guard(from: Status, to: Status) -> Result<(), TransitionError> {
    transition(from, to).map(|_| ())
}

/// **非法迁移的原因分类**（稳定码，进日志 / 审计）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransitionErrorKind {
    /// 表里没有这条边（典型：`parsed → written_back`，跳过医师确认）。
    IllegalTransition,
    /// 终态再推进（`written_back` / `rejected` / `parse_failed`）。
    TerminalState,
}

impl TransitionErrorKind {
    /// 稳定错误码（**跨版本不变**）。
    pub fn as_str(self) -> &'static str {
        match self {
            TransitionErrorKind::IllegalTransition => "ILLEGAL_TRANSITION",
            TransitionErrorKind::TerminalState => "TERMINAL_STATE",
        }
    }

    /// 中文说明（运维看得懂就行）。
    pub fn message(self) -> &'static str {
        match self {
            TransitionErrorKind::IllegalTransition => "非法状态迁移（迁移表里没有这条边）",
            TransitionErrorKind::TerminalState => "终态不可再推进",
        }
    }
}

/// **非法迁移错误**。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransitionError {
    /// 迁移前的状态。
    pub from: Status,
    /// 想推进到的状态。
    pub to: Status,
    /// 拒绝原因分类。
    pub kind: TransitionErrorKind,
}

impl TransitionError {
    /// 稳定错误码。
    pub fn code(&self) -> &'static str {
        self.kind.as_str()
    }
}

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            TransitionErrorKind::TerminalState => write!(
                f,
                "终态不可再推进：{} → {}（{} 已是终态）",
                self.from.as_str(),
                self.to.as_str(),
                self.from.as_str()
            ),
            TransitionErrorKind::IllegalTransition => {
                let allowed = allowed_targets(self.from);
                if allowed.is_empty() {
                    write!(
                        f,
                        "非法状态迁移：{} → {}（{} 无允许的目标状态）",
                        self.from.as_str(),
                        self.to.as_str(),
                        self.from.as_str()
                    )
                } else {
                    let names: Vec<&str> = allowed.iter().map(|s| s.as_str()).collect();
                    write!(
                        f,
                        "非法状态迁移：{} → {}（{} 只允许推进到：{}）",
                        self.from.as_str(),
                        self.to.as_str(),
                        self.from.as_str(),
                        names.join("、")
                    )
                }
            }
        }
    }
}

impl std::error::Error for TransitionError {}

/// **医师回执决定**（确认 / 驳回）。
///
/// 这是产品里**唯一**的人工确认动作的取值（回执端点的 body 映射到它）。
/// 拆成独立类型，是为了让「谁能推进到哪个状态」这件事在类型上说得清：
/// 确认 → 回写成功进 `written_back`；驳回 → 进 `rejected`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewDecision {
    /// 医师确认（`confirm`）—— 确认后由出站写回 EMR，成功进 `written_back`，
    /// 失败先落 `pending_writeback`（退避重试）。
    Confirm,
    /// 医师驳回（`reject`）—— 进终态 `rejected`，不回写。
    Reject,
}

impl ReviewDecision {
    /// 全部取值。
    pub const ALL: [ReviewDecision; 2] = [ReviewDecision::Confirm, ReviewDecision::Reject];

    /// 取值字符串（回执端点的 `decision` 字段取值）。
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewDecision::Confirm => "confirm",
            ReviewDecision::Reject => "reject",
        }
    }

    /// 字符串 → 取值（未知取值返回 `None` —— 不猜默认动作）。
    pub fn lookup(raw: &str) -> Option<ReviewDecision> {
        ReviewDecision::ALL
            .iter()
            .copied()
            .find(|d| d.as_str() == raw)
    }

    /// 这个决定对应的目标状态。
    pub fn target_status(self) -> Status {
        match self {
            ReviewDecision::Confirm => Status::WrittenBack,
            ReviewDecision::Reject => Status::Rejected,
        }
    }

    /// 中文说明。
    pub fn message(self) -> &'static str {
        match self {
            ReviewDecision::Confirm => "医师确认",
            ReviewDecision::Reject => "医师驳回",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 合法迁移链逐条可走() {
        // 主链：接收 → 解析 → 待确认 → 已回写。
        assert_eq!(
            transition(Status::Received, Status::Parsed).unwrap(),
            Status::Parsed
        );
        assert_eq!(
            transition(Status::Parsed, Status::AwaitingReview).unwrap(),
            Status::AwaitingReview
        );
        assert_eq!(
            transition(Status::AwaitingReview, Status::WrittenBack).unwrap(),
            Status::WrittenBack
        );
        // 医师驳回也是合法去向。
        assert_eq!(
            transition(Status::AwaitingReview, Status::Rejected).unwrap(),
            Status::Rejected
        );
        // 整份解析不出来。
        assert_eq!(
            transition(Status::Received, Status::ParseFailed).unwrap(),
            Status::ParseFailed
        );
        // 解析出来了但实体绑定有歧义 → 整份退回失败态。
        assert_eq!(
            transition(Status::Parsed, Status::ParseFailed).unwrap(),
            Status::ParseFailed
        );
        // 回写失败 → 重试态；重试成功 → 已回写。
        assert_eq!(
            transition(Status::AwaitingReview, Status::PendingWriteback).unwrap(),
            Status::PendingWriteback
        );
        assert_eq!(
            transition(Status::PendingWriteback, Status::WrittenBack).unwrap(),
            Status::WrittenBack
        );
        // 重试还没成功 → **停留重试态**（自环允许）。
        assert_eq!(
            transition(Status::PendingWriteback, Status::PendingWriteback).unwrap(),
            Status::PendingWriteback
        );
    }

    #[test]
    fn 跳过医师确认被拒() {
        // `parsed → written_back` = 没等医师点头就回写 EMR —— 明确不做的事。
        let err = transition(Status::Parsed, Status::WrittenBack)
            .expect_err("跳过确认必须被拒");
        assert_eq!(err.kind, TransitionErrorKind::IllegalTransition);
        assert_eq!(err.code(), "ILLEGAL_TRANSITION");
        assert_eq!(err.from, Status::Parsed);
        assert_eq!(err.to, Status::WrittenBack);
        // 报错要**说清合法的路**是什么，运维才知道该走哪条。
        let text = err.to_string();
        assert!(text.contains("parsed"), "报错应带上原状态：{}", text);
        assert!(text.contains("awaiting_review"), "报错应指路：{}", text);

        // 收到就直接回写，同样不行。
        assert!(transition(Status::Received, Status::WrittenBack).is_err());
        // 没等确认就驳回（驳回也是人工动作）—— 不行。
        assert!(transition(Status::Parsed, Status::Rejected).is_err());
        // 待确认态不能自己退回去重解析。
        assert!(transition(Status::AwaitingReview, Status::Parsed).is_err());
        // 待确认态不能跳回接收态。
        assert!(transition(Status::AwaitingReview, Status::Received).is_err());
        // 重试态不能退回待确认（重试的是回写，不是确认）。
        assert!(transition(Status::PendingWriteback, Status::AwaitingReview).is_err());
    }

    #[test]
    fn 终态再推进被拒() {
        for terminal in [Status::WrittenBack, Status::Rejected, Status::ParseFailed] {
            assert!(terminal.is_terminal(), "{} 应是终态", terminal.as_str());
            for target in Status::ALL {
                let result = transition(terminal, target);
                assert!(
                    result.is_err(),
                    "终态再推进必须被拒：{} → {}",
                    terminal.as_str(),
                    target.as_str()
                );
                let err = result.unwrap_err();
                assert_eq!(err.kind, TransitionErrorKind::TerminalState);
                assert_eq!(err.code(), "TERMINAL_STATE");
                assert!(err.to_string().contains("终态"));
            }
            // 终态**没有任何出边**。
            assert!(allowed_targets(terminal).is_empty());
            assert!(!terminal.is_open());
        }
    }

    #[test]
    fn 迁移表穷举自洽() {
        // 49 种组合逐对判定：`transition` 放行 == `can_transition` == 迁移表里有这条边。
        // 这条不变量保证「三个入口永远说同一句话」。
        for from in Status::ALL {
            for to in Status::ALL {
                let allowed = can_transition(from, to);
                let result = transition(from, to);
                assert_eq!(
                    allowed,
                    result.is_ok(),
                    "{} → {} 的判定不一致",
                    from.as_str(),
                    to.as_str()
                );
                if let Ok(next) = result {
                    assert_eq!(next, to);
                }
                // 表里的每条边都真的能走。
                if allowed_targets(from).contains(&to) {
                    assert!(guard(from, to).is_ok());
                }
            }
        }
        // 表里没有重复边、每个状态都在表里出现且只有一次。
        let mut seen = Vec::new();
        for (from, targets) in TRANSITION_TABLE {
            assert!(!seen.contains(from), "{} 在迁移表里出现两次", from.as_str());
            seen.push(*from);
            let mut local = Vec::new();
            for target in *targets {
                assert!(
                    !local.contains(target),
                    "{} 的出边里 {} 重复",
                    from.as_str(),
                    target.as_str()
                );
                local.push(*target);
            }
        }
        assert_eq!(seen.len(), Status::ALL.len());
        for status in Status::ALL {
            assert!(seen.contains(&status), "{} 不在迁移表里", status.as_str());
        }
    }

    #[test]
    fn 已回写只能经人工确认进入() {
        // **不自动回写**的技术保证：
        // 进 `written_back` 的来路只有两条 —— 医师确认过（awaiting_review）
        // 或重试成功（pending_writeback，而 pending_writeback 本身也来自 awaiting_review）。
        // 换句话说：**没有任何一条路能绕过医师确认**。
        for (from, targets) in TRANSITION_TABLE {
            if targets.contains(&Status::WrittenBack) {
                assert!(
                    matches!(from, Status::AwaitingReview | Status::PendingWriteback),
                    "{} 不该直接进 written_back",
                    from.as_str()
                );
            }
        }
        // 反向再确认一次：确认前的状态一律**不是**已回写。
        assert!(transition(Status::Received, Status::WrittenBack).is_err());
        assert!(transition(Status::Parsed, Status::WrittenBack).is_err());
        // pending_writeback 的来路也只能是 awaiting_review（不能凭空冒出来）；
        // 它自己的自环是「重试还没成功，停留在重试态」，不是新的来路。
        for (from, targets) in TRANSITION_TABLE {
            if targets.contains(&Status::PendingWriteback) && *from != Status::PendingWriteback {
                assert_eq!(
                    *from,
                    Status::AwaitingReview,
                    "pending_writeback 只能从待确认态进来"
                );
            }
        }
        // 自环存在且只有这一处（其余状态都不许原地停留）。
        assert!(can_transition(
            Status::PendingWriteback,
            Status::PendingWriteback
        ));
        for status in Status::ALL {
            if status != Status::PendingWriteback {
                assert!(
                    !can_transition(status, status),
                    "{} 不该允许原地停留",
                    status.as_str()
                );
            }
        }
    }

    #[test]
    fn 状态字符串往返且与状态库取值一致() {
        for status in Status::ALL {
            assert_eq!(Status::lookup(status.as_str()), Some(status));
            assert!(!status.message().is_empty());
            // 落库字段是小写蛇形。
            assert!(
                status
                    .as_str()
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_'),
                "{} 不是小写蛇形",
                status.as_str()
            );
        }
        assert_eq!(Status::lookup("no_such_status"), None);
        // 状态库 `reports.status` 现有取值（入库路径写的三个 + 调度器统计的两个）
        // 必须在状态机的取值集合里 —— 库里出现表里没有的状态 = 数据不对。
        for raw in [
            "received",
            "parsed",
            "awaiting_review",
            "pending_writeback",
            "parse_failed",
        ] {
            assert!(
                Status::lookup(raw).is_some(),
                "状态库在用的 {} 不在状态机取值集合里",
                raw
            );
        }
    }

    #[test]
    fn 状态含义分类自洽() {
        assert!(Status::Received.is_initial());
        assert!(!Status::Parsed.is_initial());
        assert!(Status::AwaitingReview.awaits_confirmation());
        assert!(!Status::Parsed.awaits_confirmation());
        assert!(Status::Received.is_open());
        assert!(Status::PendingWriteback.is_open());
        // 终态不等人确认 —— 已经落地了。
        for terminal in [Status::WrittenBack, Status::Rejected, Status::ParseFailed] {
            assert!(!terminal.awaits_confirmation());
        }
    }

    #[test]
    fn 回执决定映射到目标状态() {
        assert_eq!(ReviewDecision::lookup("confirm"), Some(ReviewDecision::Confirm));
        assert_eq!(ReviewDecision::lookup("reject"), Some(ReviewDecision::Reject));
        // 未知取值不猜默认动作。
        assert_eq!(ReviewDecision::lookup("maybe"), None);
        assert_eq!(ReviewDecision::lookup(""), None);
        assert_eq!(
            ReviewDecision::Confirm.target_status(),
            Status::WrittenBack
        );
        assert_eq!(ReviewDecision::Reject.target_status(), Status::Rejected);
        for decision in ReviewDecision::ALL {
            assert_eq!(ReviewDecision::lookup(decision.as_str()), Some(decision));
            assert!(!decision.message().is_empty());
            // 两个目标状态都必须是**合法可达**的（从待确认态）。
            assert!(
                transition(Status::AwaitingReview, decision.target_status()).is_ok(),
                "{} 的目标状态 {} 不可达",
                decision.as_str(),
                decision.target_status().as_str()
            );
        }
    }

    #[test]
    fn 错误信息带足排查信息() {
        let illegal = transition(Status::Parsed, Status::WrittenBack).unwrap_err();
        let text = illegal.to_string();
        assert!(text.contains("parsed"), "{}", text);
        assert!(text.contains("written_back"), "{}", text);
        assert!(text.contains("awaiting_review"), "{}", text);

        let terminal = transition(Status::WrittenBack, Status::Parsed).unwrap_err();
        let text = terminal.to_string();
        assert!(text.contains("终态"), "{}", text);
        assert!(text.contains("written_back"), "{}", text);
    }
}
