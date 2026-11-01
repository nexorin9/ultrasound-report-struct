//! 解析失败分类与**稳定错误码表**（全项目失败口径的唯一真源）。
//!
//! **为什么要有这张表**：超声科现在的做法是「工作站导出表格 → 人工抄测值」，
//! 抄漏了没人知道、抄错了没人发现。本产品把每一次「认不出来」都记成一条
//! **带稳定错误码 + 中文文案 + 原文位置**的失败条目 —— 运维按错误码聚合，
//! 一眼分清「**编码问题**」（文件读不出来）与「**词典缺口**」（词表缺词）。
//!
//! **三条纪律**：
//!
//! 1. **码要稳定** —— 错误码进台账 / 失败清单 / 运行摘要 / 审计，跨版本不变；
//!    文案可以改，码不改（改了等于把历史失败记录作废）。
//! 2. **策略集中** —— 「这个码怎么处置」只在 [`FailurePolicy`] 一处定义：
//!    阻断整份报告 / 本条不落台账 / 本条不产出数值 / 本条不参与对拍。
//!    处置逻辑散在各处就会漂移（同一个码在两个地方一个放行一个阻断）。
//! 3. **不留自由文本分支** —— 抽取、强转、入站三处的失败一律收敛到本表；
//!    明细归类（同一码下「具体是哪一种坏」）走 [`DetailCode`] 登记，
//!    不在调用点临时拼字符串 —— 拼出来的字符串没法聚合，也没法回归。
//!
//! **机制溯源（改写，不复制源代码）**：源产品把语法错误统一收敛成带字符光标的
//! 错误对象，光标由字节偏移换算而来；本项目把同一收敛思路落到**业务口径**上：
//! 失败一律带稳定错误码 + 中文文案 + 原文位置。

use crate::parse::locate::locate;

/// 未登记的码（错误码 / 明细归类）—— 从 CSV 或库里读回一个表里没有的码。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnknownCode;

impl std::fmt::Display for UnknownCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "未登记的码")
    }
}

impl std::error::Error for UnknownCode {}

impl std::str::FromStr for ErrorCode {
    type Err = UnknownCode;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        ErrorCode::lookup(raw).ok_or(UnknownCode)
    }
}

impl std::str::FromStr for DetailCode {
    type Err = UnknownCode;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        DetailCode::lookup(raw).ok_or(UnknownCode)
    }
}

/// 稳定错误码。
///
/// 码进台账 / 失败清单 / 运行摘要 / 审计；**跨版本不变**。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ErrorCode {
    /// 部位词典未命中 —— 不猜部位，宁可不比不错比。
    PartUnresolved,
    /// 数值缺单位 —— 不猜单位，不产出数值。
    UnitMissing,
    /// 数值不可解析（空值 / 纯符号 / 多个小数点 / 超长数字串）。
    ValueUnparseable,
    /// 测项词典未命中 —— 不静默丢弃测项文本。
    ItemDictMissing,
    /// 量纲强转查不到合法路径 —— 不静默按某单位兜底。
    CoercionNoPath,
    /// HL7 消息结构不完整 —— 缺 `MSH` / `PID` / `OBR` / `OBX` 段，或必需字段为空。
    ///
    /// **阻断整份报告**：结构不全就没有可信的权威字段与所见文本，
    /// 与「某一条数值认不出」（只坏一条）是两回事。
    Hl7SegmentMissing,
    /// 报告正文混编 / 破损 —— UTF-8 与 GBK 解不出统一口径，不猜转码。
    ///
    /// **阻断整份报告**：猜错转码会让测量值与单位一起乱码，
    /// 而乱码在台账里看起来仍然像一个数字 —— 比缺这一行更危险。
    EncodingMixed,
    /// 报告正文编码无法判定 —— 两种编码都解不出（多半是二进制 / 非文本）。
    ///
    /// **阻断整份报告**：理由同 [`ErrorCode::EncodingMixed`]。
    EncodingUnknown,
    /// RIS 历史取数失败 —— **上游不可达**（连接失败 / 超时 / 5xx）。
    ///
    /// **不阻断整份报告**：报告本身没问题，只是取不到历史值 ——
    /// 差值缺失是可观测的失败（落 `comparison_skipped`），**绝不当作「无差异」**。
    RisUpstreamUnavailable,
    /// RIS 鉴权失败 —— 401 / 403。重试无意义，要人去配令牌。
    RisAuthFailed,
    /// RIS 响应不合契约 —— 非 JSON / 字段缺失 / 其他 4xx。
    ///
    /// **不阻断整份报告**：同 [`ErrorCode::RisUpstreamUnavailable`]，
    /// 报告侧的测值照样落台账，只是这轮对不上历史。
    RisBadResponse,
    /// RIS 出站读**未配置** —— 端点或凭据缺失，还没发请求就失败了。
    ///
    /// 与 [`ErrorCode::RisUpstreamUnavailable`] 分开：一个是「配好了但连不上」，
    /// 一个是「根本没配」—— 运维处置动作不一样（后者去改配置 / 环境变量）。
    RisNotConfigured,
    /// 对拍歧义 —— 同一规范键命中**多条**历史，差值无法唯一确定。
    ///
    /// **绝不任取其一**：任取一条可能指错对象（把另一份报告的值当参照），
    /// 差值算得出来、看着正常，结论却可能指错 ——
    /// 这是差值类台账最危险的失败方式。交人工处置（指定参照哪一份历史）。
    ///
    /// **不阻断整份报告**：测值本身是对的（仍落台账、仍带数值），
    /// 对不上的是「参照哪一条历史」—— 报告侧与词典都不用改。
    ComparisonAmbiguous,
    /// EMR 回写失败 —— **上游不可达**（连接失败 / 超时 / 5xx）。
    ///
    /// **不阻断整份报告**：报告侧已经解析完了（台账行与差值都在库里），
    /// 写不出去的是**回写**这一步 —— 台账行**保留**，转 `pending_writeback` 等重试。
    EmrUpstreamUnavailable,
    /// EMR 鉴权失败 —— 401 / 403。重试无意义，要人去换令牌。
    EmrAuthFailed,
    /// EMR 响应不合契约 —— 其他 4xx（请求被拒）/ 请求本身不合法。
    ///
    /// **不阻断整份报告**：同 [`ErrorCode::EmrUpstreamUnavailable`] ——
    /// 结论算出来了，只是这一笔没写进 EMR。
    EmrBadResponse,
    /// EMR 出站写**未配置** —— 端点或凭据缺失，还没发请求就失败了。
    ///
    /// 与 [`ErrorCode::EmrUpstreamUnavailable`] 分开：一个是「配好了但连不上」，
    /// 一个是「根本没配」—— 运维处置动作不一样（后者去改配置 / 环境变量）。
    EmrNotConfigured,
    /// 企微推送失败 —— **上游不可达**（连接失败 / 超时 / 5xx / 触发频率限制）。
    ///
    /// **不阻断整份报告**：结论算出来了、也可能已经写进 EMR，
    /// 没出去的是**通知**这一步 —— 台账行**保留**，退避重试。
    WecomUpstreamUnavailable,
    /// 企微推送鉴权失败 —— webhook 地址或 key 无效（`errcode` 93000 / 93004）。
    ///
    /// **重试无意义**（地址错了再发一百遍还是错），要人去换群机器人地址。
    WecomAuthFailed,
    /// 企微响应不合契约 —— 其他 4xx / 响应体不是 JSON / `errcode` 非 0。
    ///
    /// **HTTP 200 不等于推送成功** —— 企微把业务错误放在响应体的 `errcode` 里，
    /// 只看状态码会把「被拒收」当成「已送达」。这个码就是记这件事。
    WecomBadResponse,
    /// 企微推送**未配置** —— webhook 地址缺失，还没发请求就失败了。
    ///
    /// 与 [`ErrorCode::WecomUpstreamUnavailable`] 分开：一个是「配好了但发不出去」，
    /// 一个是「根本没配」—— 处置动作不一样（后者去配环境变量）。
    WecomNotConfigured,
    /// 确认回执端点**鉴权失败** —— 缺 `Authorization` 头，或 Bearer 令牌不对。
    ///
    /// **不阻断整份报告**：报告侧已经解析完了（台账行与差值都在库里），
    /// 没走通的是「医师确认」这一步 —— 报告仍是待确认的草稿。
    ReviewUnauthorized,
    /// 确认回执**请求本身不合法** —— 请求体不是 JSON / 决定值不是 `confirm|reject` /
    /// 缺确认人 / 方法不对。
    ///
    /// **不阻断整份报告**：同 [`ErrorCode::ReviewUnauthorized`]，
    /// 报告仍是待确认的草稿，等人把回执重发一次。
    ReviewBadRequest,
    /// 确认回执**找不到对象** —— 路径不匹配这个端点，或检查号在库里没有。
    ///
    /// **不阻断整份报告**：同 [`ErrorCode::ReviewUnauthorized`]。
    ReviewNotFound,
    /// 确认回执**撞上不可推进的状态** —— 报告已回写 / 已驳回 / 解析失败 / 还没解析出来。
    ///
    /// **不阻断整份报告**：这是**状态**问题，不是报告问题 ——
    /// 报告的结论还在库里，只是这一份已经被确认过（或还没到能确认的时候）。
    ReviewIllegalState,
    /// 确认回执端点**读不到状态库** —— 打不开 / 写不进 / 被锁住。
    ///
    /// **不阻断整份报告**：报告侧的数据一点没少（库还在），
    /// 差的是「这一条回执没能落进状态库」—— 医师重发一次即可。
    ReviewStateStoreUnavailable,
}

impl ErrorCode {
    /// **全部错误码**（穷举清单）。
    ///
    /// 运维聚合、码表自检、测试覆盖都从这里取 —— 有人加了码忘了登记策略，
    /// `码表穷举一致` 这条用例会先红。
    pub const ALL: [ErrorCode; 26] = [
        ErrorCode::PartUnresolved,
        ErrorCode::UnitMissing,
        ErrorCode::ValueUnparseable,
        ErrorCode::ItemDictMissing,
        ErrorCode::CoercionNoPath,
        ErrorCode::Hl7SegmentMissing,
        ErrorCode::EncodingMixed,
        ErrorCode::EncodingUnknown,
        ErrorCode::RisUpstreamUnavailable,
        ErrorCode::RisAuthFailed,
        ErrorCode::RisBadResponse,
        ErrorCode::RisNotConfigured,
        ErrorCode::ComparisonAmbiguous,
        ErrorCode::EmrUpstreamUnavailable,
        ErrorCode::EmrAuthFailed,
        ErrorCode::EmrBadResponse,
        ErrorCode::EmrNotConfigured,
        ErrorCode::WecomUpstreamUnavailable,
        ErrorCode::WecomAuthFailed,
        ErrorCode::WecomBadResponse,
        ErrorCode::WecomNotConfigured,
        ErrorCode::ReviewUnauthorized,
        ErrorCode::ReviewBadRequest,
        ErrorCode::ReviewNotFound,
        ErrorCode::ReviewIllegalState,
        ErrorCode::ReviewStateStoreUnavailable,
    ];

    /// 错误码字符串（稳定，进台账 / 失败清单 / 运维聚合）。
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::PartUnresolved => "PART_UNRESOLVED",
            ErrorCode::UnitMissing => "UNIT_MISSING",
            ErrorCode::ValueUnparseable => "VALUE_UNPARSEABLE",
            ErrorCode::ItemDictMissing => "ITEM_DICT_MISSING",
            ErrorCode::CoercionNoPath => "COERCION_NO_PATH",
            ErrorCode::Hl7SegmentMissing => "HL7_SEGMENT_MISSING",
            ErrorCode::EncodingMixed => "ENCODING_MIXED",
            ErrorCode::EncodingUnknown => "ENCODING_UNKNOWN",
            ErrorCode::RisUpstreamUnavailable => "RIS_UPSTREAM_UNAVAILABLE",
            ErrorCode::RisAuthFailed => "RIS_AUTH_FAILED",
            ErrorCode::RisBadResponse => "RIS_BAD_RESPONSE",
            ErrorCode::RisNotConfigured => "RIS_NOT_CONFIGURED",
            ErrorCode::ComparisonAmbiguous => "COMPARISON_AMBIGUOUS",
            ErrorCode::EmrUpstreamUnavailable => "EMR_UPSTREAM_UNAVAILABLE",
            ErrorCode::EmrAuthFailed => "EMR_AUTH_FAILED",
            ErrorCode::EmrBadResponse => "EMR_BAD_RESPONSE",
            ErrorCode::EmrNotConfigured => "EMR_NOT_CONFIGURED",
            ErrorCode::WecomUpstreamUnavailable => "WECOM_UPSTREAM_UNAVAILABLE",
            ErrorCode::WecomAuthFailed => "WECOM_AUTH_FAILED",
            ErrorCode::WecomBadResponse => "WECOM_BAD_RESPONSE",
            ErrorCode::WecomNotConfigured => "WECOM_NOT_CONFIGURED",
            ErrorCode::ReviewUnauthorized => "REVIEW_UNAUTHORIZED",
            ErrorCode::ReviewBadRequest => "REVIEW_BAD_REQUEST",
            ErrorCode::ReviewNotFound => "REVIEW_NOT_FOUND",
            ErrorCode::ReviewIllegalState => "REVIEW_ILLEGAL_STATE",
            ErrorCode::ReviewStateStoreUnavailable => "REVIEW_STATE_STORE_UNAVAILABLE",
        }
    }

    /// 错误码 ↔ 字符串往返（未知字符串返回 `None`）。
    ///
    /// 从 CSV / 库里读回错误码时用它 —— 读到表里没有的码就是**数据不对**，
    /// 宁可返回 `None` 让调用方显式处理，也不硬套一个默认码。
    pub fn lookup(raw: &str) -> Option<ErrorCode> {
        ErrorCode::ALL
            .iter()
            .copied()
            .find(|code| code.as_str() == raw)
    }

    /// 中文文案（失败清单与运行摘要直接展示）。
    pub fn message(self) -> &'static str {
        match self {
            ErrorCode::PartUnresolved => "部位词典未命中，未参与对拍",
            ErrorCode::UnitMissing => "数值缺单位，未产出数值",
            ErrorCode::ValueUnparseable => "数值不可解析，未产出数值",
            ErrorCode::ItemDictMissing => "测项词典未命中，未参与对拍",
            ErrorCode::CoercionNoPath => "量纲强转查不到合法路径，未产出数值",
            ErrorCode::Hl7SegmentMissing => "HL7 消息结构不完整（缺必需段或必需字段为空），未产出条目",
            ErrorCode::EncodingMixed => "报告正文混编或破损，编码判不出，未产出正文与条目",
            ErrorCode::EncodingUnknown => "报告正文编码无法判定（非文本或二进制），未产出正文与条目",
            ErrorCode::RisUpstreamUnavailable => "RIS 历史取数失败（上游不可达），本轮未对拍",
            ErrorCode::RisAuthFailed => "RIS 鉴权失败，本轮未对拍",
            ErrorCode::RisBadResponse => "RIS 响应不合契约，本轮未对拍",
            ErrorCode::RisNotConfigured => "RIS 出站读未配置（端点或凭据缺失），本轮未对拍",
            ErrorCode::ComparisonAmbiguous => "同一规范键命中多条历史，差值无法唯一确定，待人工指定参照历史",
            ErrorCode::EmrUpstreamUnavailable => "EMR 回写失败（上游不可达），未写入 EMR，待重试",
            ErrorCode::EmrAuthFailed => "EMR 鉴权失败，未写入 EMR，待重试",
            ErrorCode::EmrBadResponse => "EMR 拒绝该回写请求，未写入 EMR，待重试",
            ErrorCode::EmrNotConfigured => "EMR 出站写未配置（端点或凭据缺失），未写入 EMR，待重试",
            ErrorCode::WecomUpstreamUnavailable => "企微推送失败（上游不可达），未送达群机器人，待重试",
            ErrorCode::WecomAuthFailed => "企微推送鉴权失败（webhook 地址或 key 无效），未送达群机器人，待重试",
            ErrorCode::WecomBadResponse => "企微拒绝该推送（响应不合契约或 errcode 非 0），未送达群机器人，待重试",
            ErrorCode::WecomNotConfigured => "企微推送未配置（webhook 地址缺失），未送达群机器人，待重试",
            ErrorCode::ReviewUnauthorized => "确认回执鉴权失败（缺 Bearer 头或令牌不对），报告仍待医师确认",
            ErrorCode::ReviewBadRequest => "确认回执请求不合法（请求体 / 决定值 / 确认人），报告仍待医师确认",
            ErrorCode::ReviewNotFound => "确认回执找不到对象（路径不匹配或检查号不存在），报告仍待医师确认",
            ErrorCode::ReviewIllegalState => "确认回执撞上不可推进的报告状态，报告结论已在库里，等人处理",
            ErrorCode::ReviewStateStoreUnavailable => "确认回执端点读不到状态库，回执没落库，报告结论仍在库里",
        }
    }

    /// 处理策略（**码 → 策略的唯一映射表**）。
    ///
    /// 四条口径各自独立，可逐条断言：
    ///
    /// | 口径 | 含义 |
    /// |---|---|
    /// | [`FailurePolicy::blocks_report`] | 整份报告作废（不再解析其余部分） |
    /// | [`FailurePolicy::drops_item`] | 本条不落台账（台账里没有这一行） |
    /// | [`FailurePolicy::drops_value`] | 本条不产出数值（`value_si` 为空） |
    /// | [`FailurePolicy::excluded_from_comparison`] | 本条不参与与 RIS 历史值的对拍 |
    pub fn policy(self) -> FailurePolicy {
        match self {
            // 词典缺词：值本身可用（仍落台账、仍带数值），缺的是**规范键**
            // —— 没有部位键 / 测项码就对不上历史值，只能排除在对拍之外。
            ErrorCode::PartUnresolved | ErrorCode::ItemDictMissing => FailurePolicy {
                blocks_report: false,
                drops_item: false,
                drops_value: false,
                excluded_from_comparison: true,
                disposition: Disposition::AwaitingDictionary,
            },
            // 数值侧：缺单位 / 坏写法 —— 整条不落台账（原值留在失败清单里）。
            ErrorCode::UnitMissing | ErrorCode::ValueUnparseable => FailurePolicy {
                blocks_report: false,
                drops_item: true,
                drops_value: true,
                excluded_from_comparison: true,
                disposition: Disposition::AwaitingValue,
            },
            // 强转查不到路径：条目**在**（原值原单位照样留痕），但没有归一值，
            // 对不了历史 —— 运维一眼看出「这条差值算不出来」。
            ErrorCode::CoercionNoPath => FailurePolicy {
                blocks_report: false,
                drops_item: false,
                drops_value: true,
                excluded_from_comparison: true,
                disposition: Disposition::AwaitingValue,
            },
            // 结构不全 / 编码判不出 → 正文不可信，整份退回。
            ErrorCode::Hl7SegmentMissing
            | ErrorCode::EncodingMixed
            | ErrorCode::EncodingUnknown => FailurePolicy {
                blocks_report: true,
                drops_item: true,
                drops_value: true,
                excluded_from_comparison: true,
                disposition: Disposition::BlockReport,
            },
            // 出站读失败 → **报告本身没事**（测值照样落台账），只是这轮对不上历史。
            // 差值缺失必须显式可观测 —— 绝不当作「无差异」。
            ErrorCode::RisUpstreamUnavailable
            | ErrorCode::RisAuthFailed
            | ErrorCode::RisBadResponse
            | ErrorCode::RisNotConfigured => FailurePolicy {
                blocks_report: false,
                drops_item: false,
                drops_value: false,
                excluded_from_comparison: true,
                disposition: Disposition::AwaitingUpstream,
            },
            // 对拍歧义：测值本身**是对的**（仍落台账、仍带数值），
            // 对不上的是「参照哪一条历史」—— 报告侧与词典都不用改，
            // 要人指定参照哪一份 → 挂「待人工判定」，不挂「待修写法 / 待补词典」。
            ErrorCode::ComparisonAmbiguous => FailurePolicy {
                blocks_report: false,
                drops_item: false,
                drops_value: false,
                excluded_from_comparison: true,
                disposition: Disposition::AwaitingDisambiguation,
            },
            // 出站**写**失败 → 报告侧没事（结论算出来了、台账行在库里），
            // 没写进 EMR 的是**回写**这一步 → 挂「待回写重试」，
            // 不挂「待上游恢复」—— 那是 RIS 取不到历史值的处置动作，
            // 挂错了会把人引到 RIS 那一端白查半天。
            ErrorCode::EmrUpstreamUnavailable
            | ErrorCode::EmrAuthFailed
            | ErrorCode::EmrBadResponse
            | ErrorCode::EmrNotConfigured => FailurePolicy {
                blocks_report: false,
                drops_item: false,
                drops_value: false,
                excluded_from_comparison: true,
                disposition: Disposition::AwaitingWriteback,
            },
            // 出站**推**失败 → 结论算出来了（台账行在库里、甚至已写进 EMR），
            // 没出去的是**通知**这一步 → 挂「待推送重试」，
            // 不挂「待回写重试」（那是查 EMR 端点的，方向不同），
            // 也不挂「待上游恢复」（那是查 RIS 的）。
            ErrorCode::WecomUpstreamUnavailable
            | ErrorCode::WecomAuthFailed
            | ErrorCode::WecomBadResponse
            | ErrorCode::WecomNotConfigured => FailurePolicy {
                blocks_report: false,
                drops_item: false,
                drops_value: false,
                excluded_from_comparison: true,
                disposition: Disposition::AwaitingNotification,
            },
            // 确认回执没走通：报告侧**什么都没坏**（台账行与差值都在库里），
            // 差的是「医师点头」这一步 → 挂「待人工确认」。
            //
            // 与前四种处置动作都不同：词典 / 写法 / 整份退回 / 上游 / 回写 / 推送
            // 都不用查 —— 查的是**令牌、请求体、以及这份报告当前是什么状态**。
            ErrorCode::ReviewUnauthorized
            | ErrorCode::ReviewBadRequest
            | ErrorCode::ReviewNotFound
            | ErrorCode::ReviewIllegalState
            | ErrorCode::ReviewStateStoreUnavailable => FailurePolicy {
                blocks_report: false,
                drops_item: false,
                drops_value: false,
                excluded_from_comparison: true,
                disposition: Disposition::AwaitingReview,
            },
        }
    }

    /// 是否阻断整份报告（策略的一个字段，便于调用方直接问）。
    pub fn blocks_report(self) -> bool {
        self.policy().blocks_report
    }

    /// 运维处置动作（策略的一个字段，便于调用方直接问）。
    pub fn disposition(self) -> Disposition {
        self.policy().disposition
    }
}

/// 运维处置动作（同一错误码下的**下一步由谁做什么**）。
///
/// 失败清单要能被运维当**待办**用：看一眼就知道这条失败该找谁。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Disposition {
    /// 补词典：部位 / 测项词典缺词 —— 运维去补词表，报告侧不用改。
    AwaitingDictionary,
    /// 修写法：缺单位 / 数值坏写法 / 查不到换算路径 —— 报告侧要改写或补单位。
    AwaitingValue,
    /// 整份退回：结构或编码不可信 —— 整份报告作废，退回工作站侧重新导出。
    BlockReport,
    /// 待上游恢复：RIS 取不到历史值 —— 运维去查网络 / 令牌 / 接口契约。
    ///
    /// 与前三种的区别：**报告侧不用改**（测值是对的，只是缺历史），
    /// 要改的是出站那一端 —— 所以不能挂到「待补词典 / 待修写法」上，
    /// 那两个会把人引到报告侧白查半天。
    AwaitingUpstream,
    /// 待人工判定：同一规范键命中多条历史 —— 差值算得出、但**指不对唯一对象**。
    ///
    /// 与前四种都不同：**报告侧、词典、上游都不用改**，
    /// 要做的是人指定参照哪一份历史（或清掉重复的那份）——
    /// 挂到前四种任何一个都会把人引到错误的地方白查半天。
    AwaitingDisambiguation,
    /// 待回写重试：结论算出来了，但**没写进 EMR** —— 查 EMR 那一端。
    ///
    /// 与 [`Disposition::AwaitingUpstream`] 分开：后者是**取不到历史值**（RIS 那一端坏了），
    /// 这一种是**回写没写进去**（EMR 那一端拒收 / 连不上）。
    /// 台账行**保留**（结论没丢），转 `pending_writeback` 等重试或人工补写。
    AwaitingWriteback,
    /// 待推送重试：结论算出来了（可能也已写进 EMR），但**企微群里没收到通知** ——
    /// 查群机器人地址 / 网络那一端。
    ///
    /// 与前两种都不同：**报告侧、词典、RIS、EMR 都不用查** ——
    /// 差的那一步只是「通知没发出去」，挂到「待上游恢复」（查 RIS）或
    /// 「待回写重试」（查 EMR）都会把人引到错误的那一端白查半天。
    AwaitingNotification,
    /// 待人工确认：确认回执没走通 —— **报告侧、词典、RIS、EMR、企微都不用查**。
    ///
    /// 差的那一步只是「医师点头」：查**令牌对不对、请求体写对没、这份报告现在是什么状态**。
    /// 挂到前六种任何一个都会把人引到报告侧或上游白查半天 ——
    /// 报告本身好好的，它只是还没被确认。
    AwaitingReview,
}

impl Disposition {
    /// 处置动作字符串（进运行摘要的聚合键）。
    pub fn as_str(self) -> &'static str {
        match self {
            Disposition::AwaitingDictionary => "AWAITING_DICTIONARY",
            Disposition::AwaitingValue => "AWAITING_VALUE",
            Disposition::BlockReport => "BLOCK_REPORT",
            Disposition::AwaitingUpstream => "AWAITING_UPSTREAM",
            Disposition::AwaitingDisambiguation => "AWAITING_DISAMBIGUATION",
            Disposition::AwaitingWriteback => "AWAITING_WRITEBACK",
            Disposition::AwaitingNotification => "AWAITING_NOTIFICATION",
            Disposition::AwaitingReview => "AWAITING_REVIEW",
        }
    }

    /// 中文文案（失败清单与运行摘要直接展示）。
    pub fn message(self) -> &'static str {
        match self {
            Disposition::AwaitingDictionary => "待补词典（部位 / 测项词表缺词）",
            Disposition::AwaitingValue => "待修写法（补单位 / 改数值写法 / 核对量纲）",
            Disposition::BlockReport => "整份退回（结构或编码不可信）",
            Disposition::AwaitingUpstream => "待上游恢复（RIS 取不到历史值：查网络 / 令牌 / 接口契约）",
            Disposition::AwaitingDisambiguation => "待人工判定（同一规范键命中多条历史，需指定参照哪一份）",
            Disposition::AwaitingWriteback => "待回写重试（结论没写进 EMR：查 EMR 端点 / 令牌 / 接口契约；台账行已保留）",
            Disposition::AwaitingNotification => "待推送重试（企微群没收到通知：查群机器人 webhook 地址 / key / 网络；台账行已保留）",
            Disposition::AwaitingReview => "待人工确认（医师确认回执没走通：查令牌 / 请求体 / 报告当前状态 / 状态库；台账行已保留）",
        }
    }
}

/// 一个错误码的**处理策略**（处置口径集中在此，码表只做一次映射）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FailurePolicy {
    /// 整份报告作废（不再解析其余部分，不落任何台账行）。
    pub blocks_report: bool,
    /// 本条不落台账（台账里没有这一行）。
    pub drops_item: bool,
    /// 本条不产出数值（`value_si` 为空）。
    pub drops_value: bool,
    /// 本条排除在对拍之外（键不全或值不可用，对不了 RIS 历史值）。
    pub excluded_from_comparison: bool,
    /// 运维处置动作。
    pub disposition: Disposition,
}

/// 失败明细归类（同一错误码下的**具体坏法**）。
///
/// 错误码要稳定（运维按码聚合），明细归类要具体（运维要知道**哪一种**坏）——
/// 两者分开存放，互不污染。明细码同样登记在本表里，
/// 不在调用点临时拼字符串。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DetailCode {
    /// 数值词面为空。
    ValueEmpty,
    /// 纯符号：只有 `+` / `-` / `.` / `×`，一个数字都没有。
    ValuePureSign,
    /// 多个小数点：`3.2.5`。
    ValueMultipleDots,
    /// 超长数字串（多半是串行粘连）。
    ValueTooLong,
    /// 含非数值字符。
    ValueNotNumeric,
    /// HL7 缺必需段或必需字段为空。
    Hl7SegmentAbsent,
    /// 报告正文混编（含判定依据）。
    EncodingMixedBytes,
    /// 报告正文非文本 / 二进制（含判定依据）。
    EncodingUnknownBytes,
    /// RIS 请求超时（超过配置的出站超时上限）。
    RisTimeout,
    /// RIS 连不上（连接被拒 / DNS 解析失败 / 传输中断）。
    RisConnectionFailed,
    /// RIS 返回 5xx（上游服务自身出错）。
    RisServerError,
    /// RIS 返回 401 / 403（令牌无效或无权限）。
    RisUnauthorized,
    /// RIS 返回其他 4xx（请求本身被拒：参数错 / 路径不存在）。
    RisClientError,
    /// RIS 响应体不是 JSON。
    RisBodyNotJson,
    /// RIS 响应体是 JSON，但字段不符合接口契约（缺字段 / 类型不对）。
    RisBodySchemaInvalid,
    /// RIS 响应体读不出来（连接中途断 / 超大响应被截断）。
    RisBodyUnreadable,
    /// 对拍歧义：**多份**历史报告命中同一规范键（各取一条测量值）。
    ComparisonMultipleReports,
    /// 对拍歧义：**同一份**历史报告内同一测项出现多条测量值。
    ComparisonMultipleMeasurements,
    /// EMR 请求超时（超过配置的出站超时上限）。
    EmrTimeout,
    /// EMR 连不上（连接被拒 / DNS 解析失败 / 传输中断）。
    EmrConnectionFailed,
    /// EMR 返回 5xx（上游服务自身出错）。
    EmrServerError,
    /// EMR 返回 401 / 403（令牌无效或无权限）。
    EmrUnauthorized,
    /// EMR 返回其他 4xx（请求被拒：字段不合契约 / 幂等键重复 / 路径不存在）。
    EmrClientError,
    /// 出站写请求**本身**不合法（幂等键缺失 / 检查号为空 / 没有可回写的结论）。
    EmrRequestInvalid,
    /// 企微请求超时（超过配置的出站超时上限）。
    WecomTimeout,
    /// 企微连不上（连接被拒 / DNS 解析失败 / 传输中断）。
    WecomConnectionFailed,
    /// 企微返回 5xx（上游服务自身出错）。
    WecomServerError,
    /// 企微返回其他 4xx（请求被拒：路径不存在 / 网关拦截）。
    WecomClientError,
    /// 企微 webhook 地址或 key 无效（`errcode` 93000 / 93004）—— 重试无用，要换地址。
    WecomUnauthorized,
    /// 企微响应体不是 JSON（多半是网关返回的 HTML 错误页）。
    WecomBodyNotJson,
    /// 企微响应体是 JSON，但字段不符合接口契约（缺 `errcode` 字段）。
    WecomBodySchemaInvalid,
    /// 企微响应体 `errcode` 非 0 —— **HTTP 200 也是拒收**。
    WecomErrcodeRejected,
    /// 推送请求**本身**不合法（消息内容为空 / 检查号为空）。
    WecomRequestInvalid,
    /// 确认回执缺 `Authorization` 头。
    ReviewTokenMissing,
    /// 确认回执的 `Authorization` 头存在，但不是 `Bearer <令牌>` 或令牌不对。
    ReviewTokenInvalid,
    /// 确认回执的路径不匹配这个端点（不是 `POST /api/v1/reports/{study_no}/review`）。
    ReviewRouteNotFound,
    /// 确认回执的方法不对（端点只收 POST）。
    ReviewMethodNotAllowed,
    /// 确认回执的请求体不是 JSON。
    ReviewBodyNotJson,
    /// 确认回执的 `decision` 不是 `confirm` / `reject`（不猜默认动作）。
    ReviewDecisionInvalid,
    /// 确认回执缺确认人（审计链要说得清「这一步是谁做的」）。
    ReviewReviewerMissing,
    /// 确认回执的检查号在状态库里没有。
    ReviewReportNotFound,
    /// 确认回执撞上不可推进的报告状态（已回写 / 已驳回 / 解析失败 / 还没解析出来）。
    ReviewStateNotReviewable,
    /// 这份报告没有可回写的结构化结论（台账行数为 0）—— 不回写空报告。
    ReviewNoItem,
}

impl DetailCode {
    /// **全部明细归类**（穷举清单）。
    pub const ALL: [DetailCode; 43] = [
        DetailCode::ValueEmpty,
        DetailCode::ValuePureSign,
        DetailCode::ValueMultipleDots,
        DetailCode::ValueTooLong,
        DetailCode::ValueNotNumeric,
        DetailCode::Hl7SegmentAbsent,
        DetailCode::EncodingMixedBytes,
        DetailCode::EncodingUnknownBytes,
        DetailCode::RisTimeout,
        DetailCode::RisConnectionFailed,
        DetailCode::RisServerError,
        DetailCode::RisUnauthorized,
        DetailCode::RisClientError,
        DetailCode::RisBodyNotJson,
        DetailCode::RisBodySchemaInvalid,
        DetailCode::RisBodyUnreadable,
        DetailCode::ComparisonMultipleReports,
        DetailCode::ComparisonMultipleMeasurements,
        DetailCode::EmrTimeout,
        DetailCode::EmrConnectionFailed,
        DetailCode::EmrServerError,
        DetailCode::EmrUnauthorized,
        DetailCode::EmrClientError,
        DetailCode::EmrRequestInvalid,
        DetailCode::WecomTimeout,
        DetailCode::WecomConnectionFailed,
        DetailCode::WecomServerError,
        DetailCode::WecomClientError,
        DetailCode::WecomUnauthorized,
        DetailCode::WecomBodyNotJson,
        DetailCode::WecomBodySchemaInvalid,
        DetailCode::WecomErrcodeRejected,
        DetailCode::WecomRequestInvalid,
        DetailCode::ReviewTokenMissing,
        DetailCode::ReviewTokenInvalid,
        DetailCode::ReviewRouteNotFound,
        DetailCode::ReviewMethodNotAllowed,
        DetailCode::ReviewBodyNotJson,
        DetailCode::ReviewDecisionInvalid,
        DetailCode::ReviewReviewerMissing,
        DetailCode::ReviewReportNotFound,
        DetailCode::ReviewStateNotReviewable,
        DetailCode::ReviewNoItem,
    ];

    /// 明细归类字符串（进失败明细与运行摘要）。
    pub fn as_str(self) -> &'static str {
        match self {
            DetailCode::ValueEmpty => "VALUE_EMPTY",
            DetailCode::ValuePureSign => "VALUE_PURE_SIGN",
            DetailCode::ValueMultipleDots => "VALUE_MULTIPLE_DOTS",
            DetailCode::ValueTooLong => "VALUE_TOO_LONG",
            DetailCode::ValueNotNumeric => "VALUE_NOT_NUMERIC",
            DetailCode::Hl7SegmentAbsent => "HL7_SEGMENT_ABSENT",
            DetailCode::EncodingMixedBytes => "ENCODING_MIXED_BYTES",
            DetailCode::EncodingUnknownBytes => "ENCODING_UNKNOWN_BYTES",
            DetailCode::RisTimeout => "RIS_TIMEOUT",
            DetailCode::RisConnectionFailed => "RIS_CONNECTION_FAILED",
            DetailCode::RisServerError => "RIS_SERVER_ERROR",
            DetailCode::RisUnauthorized => "RIS_UNAUTHORIZED",
            DetailCode::RisClientError => "RIS_CLIENT_ERROR",
            DetailCode::RisBodyNotJson => "RIS_BODY_NOT_JSON",
            DetailCode::RisBodySchemaInvalid => "RIS_BODY_SCHEMA_INVALID",
            DetailCode::RisBodyUnreadable => "RIS_BODY_UNREADABLE",
            DetailCode::ComparisonMultipleReports => "COMPARISON_MULTIPLE_REPORTS",
            DetailCode::ComparisonMultipleMeasurements => "COMPARISON_MULTIPLE_MEASUREMENTS",
            DetailCode::EmrTimeout => "EMR_TIMEOUT",
            DetailCode::EmrConnectionFailed => "EMR_CONNECTION_FAILED",
            DetailCode::EmrServerError => "EMR_SERVER_ERROR",
            DetailCode::EmrUnauthorized => "EMR_UNAUTHORIZED",
            DetailCode::EmrClientError => "EMR_CLIENT_ERROR",
            DetailCode::EmrRequestInvalid => "EMR_REQUEST_INVALID",
            DetailCode::WecomTimeout => "WECOM_TIMEOUT",
            DetailCode::WecomConnectionFailed => "WECOM_CONNECTION_FAILED",
            DetailCode::WecomServerError => "WECOM_SERVER_ERROR",
            DetailCode::WecomClientError => "WECOM_CLIENT_ERROR",
            DetailCode::WecomUnauthorized => "WECOM_UNAUTHORIZED",
            DetailCode::WecomBodyNotJson => "WECOM_BODY_NOT_JSON",
            DetailCode::WecomBodySchemaInvalid => "WECOM_BODY_SCHEMA_INVALID",
            DetailCode::WecomErrcodeRejected => "WECOM_ERRCODE_REJECTED",
            DetailCode::WecomRequestInvalid => "WECOM_REQUEST_INVALID",
            DetailCode::ReviewTokenMissing => "确认回执缺 Authorization 头",
            DetailCode::ReviewTokenInvalid => "确认回执的 Bearer 令牌不合法或不对",
            DetailCode::ReviewRouteNotFound => "确认回执的路径不匹配这个端点",
            DetailCode::ReviewMethodNotAllowed => "确认回执端点只收 POST",
            DetailCode::ReviewBodyNotJson => "确认回执的请求体不是 JSON",
            DetailCode::ReviewDecisionInvalid => "确认回执的 decision 不是 confirm / reject",
            DetailCode::ReviewReviewerMissing => "确认回执缺确认人",
            DetailCode::ReviewReportNotFound => "确认回执的检查号在状态库里没有",
            DetailCode::ReviewStateNotReviewable => "确认回执撞上不可推进的报告状态",
            DetailCode::ReviewNoItem => "这份报告没有可回写的结构化结论",
        }
    }

    /// 明细归类 ↔ 字符串往返（未知字符串返回 `None`）。
    pub fn lookup(raw: &str) -> Option<DetailCode> {
        DetailCode::ALL
            .iter()
            .copied()
            .find(|code| code.as_str() == raw)
    }

    /// 中文文案（失败清单与运行摘要直接展示）。
    pub fn message(self) -> &'static str {
        match self {
            DetailCode::ValueEmpty => "数值词面为空",
            DetailCode::ValuePureSign => "数值位置只有符号，没有数字",
            DetailCode::ValueMultipleDots => "数值词面含多个小数点",
            DetailCode::ValueTooLong => "数值词面过长，多半是串行粘连",
            DetailCode::ValueNotNumeric => "数值词面含非数值字符",
            DetailCode::Hl7SegmentAbsent => "HL7 缺必需段或必需字段为空",
            DetailCode::EncodingMixedBytes => "报告正文混编，含编码判定依据",
            DetailCode::EncodingUnknownBytes => "报告正文非文本或二进制，含编码判定依据",
            DetailCode::RisTimeout => "RIS 请求超时（超过出站超时上限）",
            DetailCode::RisConnectionFailed => "RIS 连不上（连接被拒 / 域名解析失败 / 传输中断）",
            DetailCode::RisServerError => "RIS 返回 5xx（上游服务自身出错）",
            DetailCode::RisUnauthorized => "RIS 返回 401 / 403（令牌无效或无权限）",
            DetailCode::RisClientError => "RIS 返回其他 4xx（请求被拒：参数错 / 路径不存在）",
            DetailCode::RisBodyNotJson => "RIS 响应体不是 JSON",
            DetailCode::RisBodySchemaInvalid => "RIS 响应体字段不符合接口契约",
            DetailCode::RisBodyUnreadable => "RIS 响应体读不出来（连接中途断 / 响应超限）",
            DetailCode::ComparisonMultipleReports => "多份历史报告命中同一规范键，参照历史无法唯一确定",
            DetailCode::ComparisonMultipleMeasurements => "同一份历史报告内同一测项有多条测量值，参照历史无法唯一确定",
            DetailCode::EmrTimeout => "EMR 请求超时（超过出站超时上限）",
            DetailCode::EmrConnectionFailed => "EMR 连不上（连接被拒 / 域名解析失败 / 传输中断）",
            DetailCode::EmrServerError => "EMR 返回 5xx（上游服务自身出错）",
            DetailCode::EmrUnauthorized => "EMR 返回 401 / 403（令牌无效或无权限）",
            DetailCode::EmrClientError => "EMR 返回其他 4xx（请求被拒：字段不合契约 / 幂等键重复 / 路径不存在）",
            DetailCode::EmrRequestInvalid => "出站写请求本身不合法（幂等键缺失 / 检查号为空 / 没有可回写的结论）",
            DetailCode::WecomTimeout => "企微请求超时（超过出站超时上限）",
            DetailCode::WecomConnectionFailed => "企微连不上（连接被拒 / 域名解析失败 / 传输中断）",
            DetailCode::WecomServerError => "企微返回 5xx（上游服务自身出错）",
            DetailCode::WecomClientError => "企微返回其他 4xx（请求被拒：路径不存在 / 网关拦截）",
            DetailCode::WecomUnauthorized => "企微 webhook 地址或 key 无效（errcode 93000 / 93004），换地址才能好",
            DetailCode::WecomBodyNotJson => "企微响应体不是 JSON（多半是网关返回的 HTML 错误页）",
            DetailCode::WecomBodySchemaInvalid => "企微响应体字段不符合接口契约（缺 errcode 字段）",
            DetailCode::WecomErrcodeRejected => "企微响应体 errcode 非 0（HTTP 200 也是拒收）",
            DetailCode::WecomRequestInvalid => "推送请求本身不合法（消息内容为空 / 检查号为空）",
            DetailCode::ReviewTokenMissing => "确认回执缺 Authorization 头",
            DetailCode::ReviewTokenInvalid => "确认回执的 Bearer 令牌不合法或不对",
            DetailCode::ReviewRouteNotFound => "确认回执的路径不匹配这个端点",
            DetailCode::ReviewMethodNotAllowed => "确认回执端点只收 POST",
            DetailCode::ReviewBodyNotJson => "确认回执的请求体不是 JSON",
            DetailCode::ReviewDecisionInvalid => "确认回执的 decision 不是 confirm / reject",
            DetailCode::ReviewReviewerMissing => "确认回执缺确认人",
            DetailCode::ReviewReportNotFound => "确认回执的检查号在状态库里没有",
            DetailCode::ReviewStateNotReviewable => "确认回执撞上不可推进的报告状态",
            DetailCode::ReviewNoItem => "这份报告没有可回写的结构化结论",
        }
    }
}

/// 一条失败记录：稳定错误码 + 原文位置 + 原文片段。
#[derive(Clone, Debug, PartialEq)]
pub struct Failure {
    /// 稳定错误码。
    pub code: ErrorCode,
    /// 1 基行号（未知为 0）。
    pub line: usize,
    /// 1 基列号（未知为 0）。
    pub column: usize,
    /// 1 基字符光标（未知为 0）。
    pub char_start: usize,
    /// 原文片段（脱敏后的短片段，供人眼定位）。
    ///
    /// **只放原文**（不掺说明文字）—— 片段是「照着这一段能在原报告里找到它」的锚点，
    /// 掺进别的字就锚不准了。细分说明走 [`Failure::detail`]。
    ///
    /// 唯一的例外是**编码类失败**：正文压根没解出来，没有原文可指，
    /// 片段位放编码判定依据（哪一段接不上、第几个字节非法）。
    pub raw_snippet: String,
    /// 细分说明（可选）：同一错误码下的**具体归类**（如 `VALUE_MULTIPLE_DOTS`）。
    ///
    /// 错误码要稳定（运维按码聚合），归类要具体（运维要知道是哪一种坏）——
    /// 两者分开存放，互不污染。取值必须是 [`DetailCode`] 里登记过的码。
    pub detail: String,
}

impl Failure {
    /// 构造失败记录（无细分说明）。
    pub fn new(code: ErrorCode, char_start: usize, line: usize, column: usize, snippet: &str) -> Self {
        Failure {
            code,
            line,
            column,
            char_start,
            raw_snippet: snippet.to_string(),
            detail: String::new(),
        }
    }

    /// 附加细分说明（链式调用）。
    ///
    /// 只接受 [`DetailCode`] 里登记过的码 —— 调用点临时拼的字符串没法聚合，
    /// 也没法回归。传入未登记的码在测试构建里直接失败（`debug_assert`）。
    pub fn with_detail(mut self, detail: &str) -> Self {
        debug_assert!(
            DetailCode::lookup(detail).is_some(),
            "明细归类必须先登记到 DetailCode：{}",
            detail
        );
        self.detail = detail.to_string();
        self
    }

    /// 附加**已登记**的细分归类（链式调用；首选这个）。
    pub fn with_detail_code(self, detail: DetailCode) -> Self {
        self.with_detail(detail.as_str())
    }

    /// 细分归类（未登记或为空时为 `None`）。
    pub fn detail_code(&self) -> Option<DetailCode> {
        DetailCode::lookup(&self.detail)
    }

    /// 面向人的说明：错误码中文文案 + 细分归类（若有）。
    pub fn message(&self) -> String {
        if self.detail.is_empty() {
            self.code.message().to_string()
        } else {
            format!("{}（{}）", self.code.message(), self.detail)
        }
    }

    /// 本条失败的处理策略（取自错误码的策略表）。
    pub fn policy(&self) -> FailurePolicy {
        self.code.policy()
    }

    /// 本条失败的运维处置动作。
    pub fn disposition(&self) -> Disposition {
        self.code.disposition()
    }
}

/// 造一条带位置的失败条目（位置由字符光标换算，未知为 0，不编造）。
pub fn failure_at(code: ErrorCode, char_start: usize, text: &str, snippet: &str) -> Failure {
    let (line, column) = locate(text, char_start);
    Failure::new(code, char_start, line, column, snippet)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 错误码稳定且有中文文案() {
        assert_eq!(ErrorCode::UnitMissing.as_str(), "UNIT_MISSING");
        assert_eq!(ErrorCode::CoercionNoPath.as_str(), "COERCION_NO_PATH");
        assert!(!ErrorCode::UnitMissing.message().is_empty());
    }

    #[test]
    fn 本切片错误码均不阻断整份报告() {
        for code in [
            ErrorCode::PartUnresolved,
            ErrorCode::UnitMissing,
            ErrorCode::ValueUnparseable,
            ErrorCode::ItemDictMissing,
            ErrorCode::CoercionNoPath,
        ] {
            assert!(!code.blocks_report());
        }
    }

    #[test]
    fn 错误码字符串往返() {
        for code in ErrorCode::ALL {
            assert_eq!(ErrorCode::lookup(code.as_str()), Some(code));
            assert!(!code.message().is_empty());
            assert!(!code.policy().disposition.as_str().is_empty());
        }
        assert_eq!(ErrorCode::lookup("NO_SUCH_CODE"), None);
        assert!("NO_SUCH_CODE".parse::<ErrorCode>().is_err());
    }

    #[test]
    fn 错误码字符串互不相同() {
        let mut codes: Vec<&str> = ErrorCode::ALL.iter().map(|c| c.as_str()).collect();
        codes.sort_unstable();
        let before = codes.len();
        codes.dedup();
        assert_eq!(codes.len(), before, "错误码字符串重复了");
    }

    #[test]
    fn 明细归类字符串往返() {
        for code in DetailCode::ALL {
            assert_eq!(DetailCode::lookup(code.as_str()), Some(code));
            assert!(!code.message().is_empty());
        }
        assert_eq!(DetailCode::lookup("NO_SUCH_DETAIL"), None);
        assert!("NO_SUCH_DETAIL".parse::<DetailCode>().is_err());
    }

    #[test]
    fn 策略表按口径分流() {
        // 缺单位 / 坏值：不落台账、不产出数值，等报告侧改写法。
        assert!(ErrorCode::UnitMissing.policy().drops_item);
        assert!(ErrorCode::ValueUnparseable.policy().drops_item);
        assert!(ErrorCode::UnitMissing.policy().drops_value);
        // 词典缺词：值还在（仍落台账），但对不了历史。
        assert!(!ErrorCode::PartUnresolved.policy().drops_item);
        assert!(!ErrorCode::PartUnresolved.policy().drops_value);
        assert!(ErrorCode::PartUnresolved.policy().excluded_from_comparison);
        // 强转无路径：条目在，数值不在。
        assert!(!ErrorCode::CoercionNoPath.policy().drops_item);
        assert!(ErrorCode::CoercionNoPath.policy().drops_value);
        // 编码 / 结构不可信：整份退回。
        assert!(ErrorCode::EncodingMixed.policy().blocks_report);
        assert_eq!(
            ErrorCode::EncodingMixed.disposition(),
            Disposition::BlockReport
        );
        assert_eq!(
            ErrorCode::PartUnresolved.disposition(),
            Disposition::AwaitingDictionary
        );
        assert_eq!(
            ErrorCode::UnitMissing.disposition(),
            Disposition::AwaitingValue
        );
    }

    #[test]
    fn 所有失败都排除在对拍之外() {
        // 失败条目一律不进对拍：键不全或值不可用，对了也是错的。
        for code in ErrorCode::ALL {
            assert!(
                code.policy().excluded_from_comparison,
                "{} 应排除在对拍之外",
                code.as_str()
            );
        }
    }

    #[test]
    fn 对拍歧义码的策略是待人工判定() {
        // 歧义不是「报告坏了」：测值仍落台账、仍带数值，只是不产差值。
        let policy = ErrorCode::ComparisonAmbiguous.policy();
        assert!(!policy.blocks_report);
        assert!(!policy.drops_item);
        assert!(!policy.drops_value);
        assert!(policy.excluded_from_comparison);
        assert_eq!(
            policy.disposition,
            Disposition::AwaitingDisambiguation
        );
        assert_eq!(
            ErrorCode::ComparisonAmbiguous.as_str(),
            "COMPARISON_AMBIGUOUS"
        );
        assert_eq!(
            Disposition::AwaitingDisambiguation.as_str(),
            "AWAITING_DISAMBIGUATION"
        );
        // 明细两类歧义各自登记。
        assert_eq!(
            DetailCode::ComparisonMultipleReports.as_str(),
            "COMPARISON_MULTIPLE_REPORTS"
        );
        assert_eq!(
            DetailCode::ComparisonMultipleMeasurements.as_str(),
            "COMPARISON_MULTIPLE_MEASUREMENTS"
        );
    }
}
