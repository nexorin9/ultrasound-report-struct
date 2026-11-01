//! 超声报告结构化系统 —— 事件驱动守护进程入口。
//!
//! 本轮已接通的子命令：
//! - `parse` —— 解析一段报告正文，打印结构化条目与失败条目。
//! - `init-db` —— 建状态库（五张表 + 幂等迁移）。
//! - `ingest` —— 扫投递目录批次：解析 → 强转 → 落台账。
//! - `export` —— 导出台账 CSV 与失败清单 CSV。
//! - `fetch-public-data` —— 取回公开数据（负语料 + 口径核对清单），带完整性校验与断网降级。
//! - `daemon` —— **常驻守护**：监听投递目录，事件到达即处理；SIGTERM / SIGINT 优雅退出。
//! - `replay` —— **离线重放**：按幂等键重放已入库的报告，对出口径漂移；`--apply` 补处理。
//! - `selftest` —— **一条命令出可核对结果**：台账行数 / 失败条目数 / 差值条目数。
//! - `ris-history` —— **出站读**：回院内 RIS 取该患者的历史超声报告（真实 HTTP + Bearer 凭据）。
//!
//! **全局参数**：
//! - `--config <路径>` —— 配置文件（路径与端点）；给了就跑启动自检。
//! - `--state-db <路径>` —— 状态库路径（覆盖配置文件与环境变量）。
//! - `--dry-run` —— **演练模式**：照常解析与统计，**不落状态库、不写出站报文**。
//! - `--require-credentials` —— 按**守护档**要求凭据齐备（缺项逐条列出并非零退出）。

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use ultrasound_report_struct::cli::args::{opt_value, split_globals, wants_help};
use ultrasound_report_struct::cli::replay::{replay_inbox, retry_pending_writebacks, ReplayMode};
use ultrasound_report_struct::cli::selftest::{run_selftest, SelfTestOptions};
use ultrasound_report_struct::config::{
    startup_checks, Config, Credentials, Profile, CONFIG_EXIT_CODE, ENV_RIS_BASE_URL,
};
use ultrasound_report_struct::compare::ris_client::{RisClient, RisQuery};
use ultrasound_report_struct::coerce::dictionary_lifecycle::Dictionaries;
use ultrasound_report_struct::daemon::scheduler::DaemonConfig;
use ultrasound_report_struct::daemon::signal;
use ultrasound_report_struct::daemon::{ComparingProcessor, IngestProcessor, Scheduler};
use ultrasound_report_struct::export::export_all;
use ultrasound_report_struct::ingest::encoding::decode_report;
use ultrasound_report_struct::ingest::ingest_inbox;
use ultrasound_report_struct::log::{Logger, RunSummary};
use ultrasound_report_struct::outbound::emr::EmrClient;
use ultrasound_report_struct::outbound::retry::RetryPolicy;
use ultrasound_report_struct::outbound::review_api::{ReviewHandler, ReviewServer};
use ultrasound_report_struct::outbound::wecom::WeComClient;
use ultrasound_report_struct::parse::{parse_report, ParseOutcome};
use ultrasound_report_struct::publicdata::{fetch_public_data, FetchOptions};
use ultrasound_report_struct::state::repo::Repo;
use ultrasound_report_struct::workflow::summary::{
    summarize, SummaryError, SummaryWindow,
};

/// 公开数据校验失败时的退出码（与「网络不可用 → 降级、退出码 0」区分开）。
const PUBLIC_DATA_INTEGRITY_EXIT_CODE: u8 = 1;

/// 编码判别失败时的退出码（混编 / 非文本：显式失败，不猜转码）。
const ENCODING_EXIT_CODE: u8 = 4;

/// RIS 出站读失败时的退出码（连不上 / 鉴权失败 / 响应不合契约 / 未配置）。
const RIS_EXIT_CODE: u8 = 5;

/// 运行摘要查不到状态库 / 统计失败时的退出码。
const SUMMARY_EXIT_CODE: u8 = 6;

/// 自检未通过的退出码（自检跑得起来但有项目没过）。
const SELFTEST_EXIT_CODE: u8 = 7;

const USAGE: &str = "\
用法：ultrasound-report-struct <子命令> [参数]

子命令：
  parse --text <文本>            解析一段报告正文，打印结构化条目与失败条目
  parse --file <路径>            解析一个报告文本文件
  init-db [--state-db <路径>]    建状态库（幂等迁移，重复执行安全）
  ingest --inbox <目录>          扫投递目录批次：解析 → 强转 → 落台账
  export --out <目录>            导出台账 CSV 与失败清单 CSV
  summary [--date <日期>]        运行摘要：受理份数 / 失败率 / 待确认数 / 失败码分布
  summary --yesterday           运行摘要：看昨天（运维接班最常问的一句）
  fetch-public-data [--offline]  取回公开数据（负语料 + 口径核对清单）
  reload-dictionary              重载词典（运维改了词典文件后手动换一代口径）
  ris-history --patient <主索引> 回院内 RIS 取该患者的历史超声报告（出站读）
  replay --inbox <目录>          离线重放：按幂等键重放已入库报告，对出口径漂移
  replay --inbox <目录> --apply  补处理：把漂移的报告刷新成重算结果（不新增报告行）
  replay --retry-pending         补处理：重新回写 EMR 那一步没写成的报告（待回写）
  selftest                       自检：一条命令出可核对结果（台账行数 / 失败条目数 / 差值条目数）
  daemon                         常驻监听投递目录：事件到达即处理（Ctrl-C / SIGTERM 优雅退出）
  --help                         显示本帮助

全局参数：
  --config <路径>                配置文件（路径与端点）；给了就跑启动自检
  --state-db <路径>              状态库路径（覆盖配置文件与环境变量）
  --dry-run                      演练模式：照常解析与统计，不落状态库、不写出站报文
  --require-credentials          按守护档校验凭据齐备（缺项逐条列出并非零退出）

daemon 参数：
  --inbox <目录>                 投递目录（覆盖配置文件）
  --max-concurrency <数量>       并发上限（默认 4；同一检查号串行，不同检查号并行）
  --tick-ms <毫秒>               目录轮询间隔（默认 1000）
  --review-bind <地址>           内置确认回执端点监听地址（覆盖配置文件）

fetch-public-data 参数：
  --dest <目录>                  落地目录（默认 assets/public）
  --offline                      不发起网络请求，只校验仓库内置样本集
  --timeout <秒>                 单次请求超时（默认 120）

ris-history 参数：
  --patient <主索引>             患者主索引（必填）
  --body-part <部位>             部位过滤（可选）
  --since <时间>                 起始时间（可选）
  --timeout <秒>                 单次请求超时（默认取配置里的出站超时）

summary 参数：
  --date <YYYY-MM-DD>            统计哪一天（默认今天）
  --yesterday                    看昨天（与 --date 互斥）

replay 参数：
  --inbox <目录>                 要重放的投递目录（--retry-pending 之外必填）
  --apply                        把漂移的报告补处理（刷新台账行；不新增报告行）
  --retry-pending                补处理待回写（pending_writeback）的报告：重新回写 EMR

selftest 参数：
  --assets <目录>                资产目录（默认 assets；内置样例与对照表都在里面）";


fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("错误：{}", err);
            ExitCode::from(2)
        }
    }
}

fn run(args: &[String]) -> Result<ExitCode, String> {
    let (globals, rest) = split_globals(args)?;
    let logger = Arc::new(Logger::stdout());

    let sub = match rest.first() {
        Some(s) => s.as_str(),
        None => {
            println!("{}", USAGE);
            return Ok(ExitCode::from(2));
        }
    };

    // 0) `--help` 统一在分发前处理：**每个**子命令都打得开用法
    //    （不在各子命令里各写一遍 —— 少一个子命令就少一份帮助）。
    if wants_help(&rest[1..]) {
        println!("{}", USAGE);
        return Ok(ExitCode::SUCCESS);
    }

    // 0b) 守护进程与演练模式冲突 → **在启动自检之前**就说清楚。
    //     （自检要求凭据齐备；先跑自检会先报「缺凭据」，把真正的问题盖住。）
    if sub == "daemon" && globals.dry_run {
        return Err(
            "daemon 是常驻形态，不支持 --dry-run（演练模式 = 不落库，与守护语义冲突）。\
             要先看效果请用：ingest --inbox <目录> --dry-run"
                .to_string(),
        );
    }

    // 1) 加载配置：配置文件 → 环境变量覆盖 → 命令行覆盖。
    let config_path_label = globals.config.as_ref().map(|p| p.display().to_string());
    let mut config = match &globals.config {
        Some(path) => match Config::load(path) {
            Ok(cfg) => cfg,
            Err(err) => {
                eprintln!("{}", err);
                return Ok(ExitCode::from(CONFIG_EXIT_CODE));
            }
        },
        None => Config::default(),
    };
    config.apply_env_overrides();
    if let Some(db) = &globals.state_db {
        config.state_db = db.clone();
    }

    // 2) 词典加载：**两本一次装完，再打日志**。
    //
    //    日志行会带上当前生效的词典版本，而版本取自词典单例 —— 若在加载第二本
    //    之前先打一行日志，词典单例会被**默认路径**抢先初始化，配置文件里
    //    指定的词典路径就被悄悄忽略（运维改了词典却没生效，且没有任何提示）。
    //    词典文件缺失 → 退回编译期内置的同一份；文件存在却坏了 → 直接退出，
    //    不退回旧词典继续跑（那会让口径变了却看不出来）。
    let dicts = match Dictionaries::load(&config.parts_file, &config.items_file) {
        Ok(dicts) => dicts,
        Err(err) => {
            eprintln!("错误码={} 说明={}", err.code(), err.message());
            eprintln!(
                "{}词典不可用，进程退出（退出码 {}）。",
                err.subject(),
                CONFIG_EXIT_CODE
            );
            return Ok(ExitCode::from(CONFIG_EXIT_CODE));
        }
    };
    let snapshot = dicts.current().expect("词典已装载");
    logger.info(
        "DICT_LOADED",
        &[
            ("version", snapshot.version().to_string()),
            ("generation", snapshot.generation().to_string()),
            ("parts_version", snapshot.parts_version().to_string()),
            ("parts_source", snapshot.parts().source().to_string()),
            ("items_version", snapshot.items_version().to_string()),
            ("items_source", snapshot.items().source().to_string()),
        ],
    );

    if let Some(path) = config_path_label {
        logger.info("CONFIG_LOADED", &[("path", path)]);
    }

    // 3) 启动自检：给了配置文件或要求凭据时才跑；守护子命令按守护档校验。
    let profile = if sub == "daemon" || globals.require_credentials {
        Profile::Daemon
    } else {
        Profile::Offline
    };
    if globals.config.is_some() || globals.require_credentials || sub == "daemon" {
        let creds = Credentials::from_env();
        let report = startup_checks(&config, &creds, profile);
        println!("{}", report.render());
        if report.has_failure() {
            eprintln!("启动自检未通过，进程退出（退出码 {}）。", CONFIG_EXIT_CODE);
            return Ok(ExitCode::from(CONFIG_EXIT_CODE));
        }
        logger.info("STARTUP_CHECKED", &[("profile", format!("{:?}", profile))]);
    }

    match sub {
        "parse" => run_parse(&rest[1..]),
        "init-db" => run_init_db(&config, globals.dry_run),
        "ingest" => run_ingest(&config, &rest[1..], &logger, globals.dry_run),
        "export" => run_export(&config, &rest[1..], &logger, globals.dry_run),
        "summary" => run_summary(&config, &rest[1..], &logger),
        "fetch-public-data" => run_fetch_public_data(&rest[1..], &logger),
        "reload-dictionary" => run_reload_dictionary(&logger),
        "ris-history" => run_ris_history(&config, &rest[1..], &logger),
        "replay" => run_replay(&config, &rest[1..], &logger),
        "selftest" => run_selftest_cmd(&rest[1..], globals.require_credentials),
        "daemon" => run_daemon(&config, &rest[1..], &logger),
        "--help" | "-h" | "help" => {
            println!("{}", USAGE);
            Ok(ExitCode::SUCCESS)
        }
        other => Err(format!("未知子命令：{}", other)),
    }
}

/// **离线重放**：按幂等键把指定目录的报告重新走一遍。
///
/// **对账模式**（默认）：重新解析并与库里已有条目逐条比对，
/// 指出**哪些报告的口径漂了** —— 台账不动。
/// **补处理模式**（`--apply`）：把漂移的报告**刷新**成重算结果
/// （台账行替换，**报告行不新增**）。
fn run_replay(config: &Config, args: &[String], logger: &Logger) -> Result<ExitCode, String> {
    if wants_help(args) {
        println!("{}", USAGE);
        return Ok(ExitCode::SUCCESS);
    }

    // **待回写补处理**是另一件事：台账行是对的，只是回写 EMR 那一步当时没写成。
    // 它不需要投递目录（源文件可能早就挪走了），按库里的状态找人。
    if args.iter().any(|a| a == "--retry-pending") {
        return run_retry_pending(config, logger);
    }

    let inbox = opt_value(args, "--inbox")?.ok_or_else(|| "需要 --inbox <目录>".to_string())?;
    let apply = args.iter().any(|a| a == "--apply");
    let mode = if apply {
        ReplayMode::Refresh
    } else {
        ReplayMode::Verify
    };

    let repo = Repo::open(&config.state_db).map_err(|e| format!("打开状态库失败：{}", e))?;
    let summary = replay_inbox(&repo, std::path::Path::new(&inbox), mode)
        .map_err(|e| format!("重放失败：{}", e))?;

    logger.info(
        "REPLAY_DONE",
        &[
            ("inbox", inbox.clone()),
            ("mode", if apply { "refresh" } else { "verify" }.to_string()),
            ("scanned", summary.scanned.to_string()),
            ("replayed", summary.replayed.to_string()),
            ("consistent", summary.consistent.to_string()),
            ("drifted", summary.drifted.to_string()),
            ("not_replayable", summary.not_replayable.to_string()),
            ("refreshed", summary.refreshed.to_string()),
            ("ledger_rows", summary.ledger_rows.to_string()),
        ],
    );

    println!("{}", summary.render());
    for drift in &summary.drifts {
        println!(
            "[漂移] {} · {}：{}",
            drift.study_no,
            drift.kind.message(),
            drift.detail
        );
    }
    if apply && summary.refreshed == 0 {
        println!("补处理：没有需要刷新的报告（口径没漂，或目录里的报告不在库里）。");
    }
    Ok(ExitCode::SUCCESS)
}

/// **补处理待回写的报告**（`pending_writeback` → 重新回写 EMR）。
///
/// **与口径漂移补处理是两件事**：台账行是对的，只是回写 EMR 那一步当时没写成。
/// 它不需要投递目录（源文件可能早就挪走了），按库里的状态找人。
/// 走**同一个回写通道**（退避重试 + 失败升级 + 告警 + 成功收口）——
/// 补处理与确认回执两条路的重试口径只有一份。
fn run_retry_pending(config: &Config, logger: &Logger) -> Result<ExitCode, String> {
    let repo = Repo::open(&config.state_db).map_err(|e| format!("打开状态库失败：{}", e))?;
    let creds = Credentials::from_env();
    let emr = EmrClient::from_config(config, &creds).ok();
    let wecom = WeComClient::from_config(config, &creds).ok();
    let policy = RetryPolicy::from_config(config);

    let summary = retry_pending_writebacks(&repo, emr.as_ref(), wecom.as_ref(), policy)
        .map_err(|e| format!("补处理待回写报告失败：{}", e))?;

    logger.info(
        "RETRY_PENDING_DONE",
        &[
            ("candidates", summary.candidates.to_string()),
            ("written", summary.written.to_string()),
            ("failed", summary.failed.to_string()),
            ("skipped", summary.skipped.to_string()),
        ],
    );

    println!("{}", summary.render());
    for detail in &summary.details {
        println!("[补处理] {}", detail);
    }
    if summary.candidates == 0 {
        println!("库里没有待回写（pending_writeback）的报告。");
    }
    if emr.is_none() {
        eprintln!("提示：EMR 出站写未配置（端点或凭据缺失）—— 本次补处理不会发出站写请求。");
    }
    Ok(ExitCode::SUCCESS)
}

/// **自检**：一条命令出可核对结果。
///
/// **不碰业务数据**：状态库开在内存里，语料与对照表取仓库内置资产。
/// 任一项没过 → 退出码非 0（自检的意义就是「一条命令就能回答是不是好的」）。
fn run_selftest_cmd(args: &[String], require_credentials: bool) -> Result<ExitCode, String> {
    if wants_help(args) {
        println!("{}", USAGE);
        return Ok(ExitCode::SUCCESS);
    }
    let assets_dir = opt_value(args, "--assets")?
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("assets"));

    let options = SelfTestOptions {
        assets_dir,
        require_credentials,
    };
    let report = match run_selftest(&options) {
        Ok(report) => report,
        Err(err) => {
            // 跑不起来就说清楚（**不 panic**）：缺资产、资产坏了，各自有说法。
            eprintln!("自检跑不起来：{}", err);
            return Ok(ExitCode::from(SELFTEST_EXIT_CODE));
        }
    };
    println!("{}", report.render());
    if report.passed() {
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!("自检未通过（退出码 {}）。", SELFTEST_EXIT_CODE);
        Ok(ExitCode::from(SELFTEST_EXIT_CODE))
    }
}

/// 守护子命令：常驻监听投递目录，事件到达即处理。
///
/// **停机语义**：SIGTERM / SIGINT → 停止取新任务 → 等在途任务结束 → 落库 → 退出码 0。
/// 排队未开始的任务被丢弃，但**源文件仍在投递目录里**，重启后会被重新发现。
fn run_daemon(
    config: &Config,
    args: &[String],
    logger: &Arc<Logger>,
) -> Result<ExitCode, String> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", USAGE);
        return Ok(ExitCode::SUCCESS);
    }

    let mut inbox = config.inbox_dir.clone();
    let mut max_concurrency = config.max_concurrency;
    let mut tick_ms = config.tick_interval_ms;
    let mut review_bind = config.review_bind.clone();

    let mut idx = 0usize;
    while idx < args.len() {
        match args[idx].as_str() {
            "--inbox" => {
                idx += 1;
                inbox = PathBuf::from(
                    args.get(idx)
                        .cloned()
                        .ok_or_else(|| "--inbox 缺少取值".to_string())?,
                );
            }
            "--review-bind" => {
                idx += 1;
                review_bind = args
                    .get(idx)
                    .cloned()
                    .ok_or_else(|| "--review-bind 缺少取值".to_string())?;
            }
            "--max-concurrency" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .cloned()
                    .ok_or_else(|| "--max-concurrency 缺少取值".to_string())?;
                max_concurrency = value
                    .parse::<usize>()
                    .map_err(|_| format!("--max-concurrency 取值非法：{}", value))?;
                if max_concurrency == 0 {
                    return Err("--max-concurrency 必须大于 0".to_string());
                }
            }
            "--tick-ms" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .cloned()
                    .ok_or_else(|| "--tick-ms 缺少取值".to_string())?;
                tick_ms = value
                    .parse::<u64>()
                    .map_err(|_| format!("--tick-ms 取值非法：{}", value))?;
                if tick_ms == 0 {
                    return Err("--tick-ms 必须大于 0".to_string());
                }
            }
            other => return Err(format!("未知参数：{}", other)),
        }
        idx += 1;
    }

    let daemon_config = DaemonConfig {
        inbox: inbox.clone(),
        state_db: config.state_db.clone(),
        max_concurrency,
        tick_interval: Duration::from_millis(tick_ms),
        // 写入稳定判定窗口：与轮询间隔同量级（工作站写完一份报告约需几百毫秒）。
        min_stable: Duration::from_millis(tick_ms.max(1000)),
        heartbeat_every_ticks: 1,
    };

    // 停止信号：SIGTERM / SIGINT → 调度器停机。
    signal::install_stop_handlers();

    // **对拍段**：落台账之后接着与 RIS 历史对拍（真实出站读）。
    //
    // 端点或凭据缺失 → **不猜**：这一轮显式跳过（`comparison_skipped` +
    // `RIS_NOT_CONFIGURED`），台账与测值照样落，报告照样进人工确认队列 ——
    // 「没比成」绝不冒充「跟上次一样」。
    let creds = Credentials::from_env();
    let ris = RisClient::from_config(config, &creds).ok();
    if ris.is_none() {
        eprintln!(
            "警告：RIS 出站读未配置（RIS_BASE_URL / RIS_API_TOKEN）—— 对拍将显式跳过，\
             测值照常落台账、差值暂缺。"
        );
    }
    let scheduler = Arc::new(Scheduler::new(
        daemon_config,
        Arc::new(ComparingProcessor::new(IngestProcessor, ris)),
        Arc::clone(logger),
    ));
    // 信号 → 停机的接线（50ms 一跳，与调度器内部的停机检查同粒度）。
    {
        let scheduler = Arc::clone(&scheduler);
        std::thread::spawn(move || {
            while !signal::stop_requested() {
                std::thread::sleep(Duration::from_millis(50));
            }
            scheduler.request_stop_with_reason("收到 SIGTERM / SIGINT 停机信号");
        });
    }

    println!(
        "守护进程启动：投递目录 {}，状态库 {}，并发上限 {}，轮询间隔 {} 毫秒",
        inbox.display(),
        config.state_db.display(),
        max_concurrency,
        tick_ms
    );

    // **内置确认回执端点**：医师在工作站上确认，回执打到这里。
    //
    // 绑不上**不致命** —— 守护进程的主职责是处理投递目录，
    // 端点绑不上（端口被占 / 机器上已有实例）时明确报出来并继续跑：
    // 「医师确认暂时用不了」这件事要看得见，但不该把整台机器上的投递处理也一起停掉。
    let creds = Credentials::from_env();
    let review_handler = ReviewHandler::from_config(config, &creds);
    let mut review_server = match ReviewServer::bind(&review_bind, review_handler) {
        Ok(server) => {
            let addr = server.local_addr().map(|a| a.to_string()).unwrap_or_default();
            logger.info("REVIEW_API_STARTED", &[("address", addr.clone())]);
            println!(
                "确认回执端点已启动：http://{}/api/v1/reports/{{study_no}}/review",
                addr
            );
            Some(server.spawn())
        }
        Err(err) => {
            logger.error(
                "REVIEW_BIND_FAILED",
                &[("bind", review_bind.clone()), ("reason", err.to_string())],
            );
            eprintln!(
                "警告：确认回执端点绑定失败（{}）：{} —— 医师确认暂时用不了，守护进程继续处理投递目录。",
                review_bind, err
            );
            None
        }
    };

    scheduler
        .run()
        .map_err(|e| format!("守护进程异常退出：{}", e))?;

    if let Some(server) = review_server.as_mut() {
        server.stop();
    }
    println!("守护进程已退出（在途任务已落库）。");
    Ok(ExitCode::SUCCESS)
}

/// 取回公开数据：负语料 + 单位码 / 部位编码核对清单。
///
/// 退出码：0 = 成功或降级（断网改用内置样本集不算失败）；
/// 1 = 完整性校验不通过（来源与文件名已在报告里点名）。
fn run_fetch_public_data(args: &[String], logger: &Logger) -> Result<ExitCode, String> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", USAGE);
        return Ok(ExitCode::SUCCESS);
    }

    let mut options = FetchOptions::default();
    let mut idx = 0usize;
    while idx < args.len() {
        match args[idx].as_str() {
            "--offline" => options.offline = true,
            "--dest" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .cloned()
                    .ok_or_else(|| "--dest 缺少取值".to_string())?;
                options.dest = std::path::PathBuf::from(value);
            }
            "--timeout" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .cloned()
                    .ok_or_else(|| "--timeout 缺少取值".to_string())?;
                options.timeout_secs = value
                    .parse::<u64>()
                    .map_err(|_| format!("--timeout 取值非法：{}", value))?;
            }
            other => return Err(format!("未知参数：{}", other)),
        }
        idx += 1;
    }

    let report = fetch_public_data(&options).map_err(|e| e.to_string())?;
    println!("{}", report.render());

    logger.info(
        "PUBLIC_DATA_FETCHED",
        &[
            ("offline", options.offline.to_string()),
            ("checksums", report.checksums.to_string()),
            (
                "degraded",
                report.has_degradation().to_string(),
            ),
            ("failed", report.has_failure().to_string()),
        ],
    );

    if report.has_failure() {
        eprintln!(
            "公开数据完整性校验未通过，进程退出（退出码 {}）。",
            PUBLIC_DATA_INTEGRITY_EXIT_CODE
        );
        return Ok(ExitCode::from(PUBLIC_DATA_INTEGRITY_EXIT_CODE));
    }
    Ok(ExitCode::SUCCESS)
}

/// **运行摘要**：回答「昨天有多少报告没解析出来」。
///
/// 口径是**库口径**（跨重启、跨进程）：受理份数 / 整份失败率 / 待确认数 /
/// 待回写数 / 失败条目按错误码与处置动作聚合。
///
/// **日期不对就报错**：格式不合 `YYYY-MM-DD` → 显式失败，**不猜是哪一天**
/// （猜出来的窗口会静悄悄地少算一天，值班的人看不出来）。
fn run_summary(config: &Config, args: &[String], logger: &Logger) -> Result<ExitCode, String> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", USAGE);
        return Ok(ExitCode::SUCCESS);
    }

    let date = opt_value(args, "--date")?;
    let yesterday = args.iter().any(|a| a == "--yesterday");
    if date.is_some() && yesterday {
        return Err("--date 与 --yesterday 互斥（只能给一个）".to_string());
    }

    let window = if yesterday {
        SummaryWindow::yesterday()
    } else if let Some(raw) = date {
        let raw = raw.trim().to_string();
        SummaryWindow::day(&raw).ok_or_else(|| {
            SummaryError::InvalidDate(raw.clone()).to_string()
        })?
    } else {
        SummaryWindow::today()
    };

    // 状态库不存在 → 显式报错（**不凭空建一个空库**再报「一切正常」）。
    if !config.state_db.exists() {
        eprintln!(
            "状态库不存在：{}（尚未 init-db，或 --state-db 指错了）",
            config.state_db.display()
        );
        return Ok(ExitCode::from(SUMMARY_EXIT_CODE));
    }

    let repo = Repo::open(&config.state_db).map_err(|e| format!("状态库打开失败：{}", e))?;
    let summary = summarize(&repo, &window).map_err(|e| format!("运行摘要统计失败：{}", e))?;

    println!("{}", summary.render_report());
    logger.info(
        "SUMMARY_REPORTED",
        &[
            ("window", summary.window_label.clone()),
            ("processed", summary.processed.to_string()),
            ("failed", summary.failed.to_string()),
            ("failed_rate", format!("{:.3}", summary.failed_rate())),
            ("awaiting_review", summary.awaiting_review.to_string()),
            ("pending_writeback", summary.pending_writeback.to_string()),
            ("error_codes", summary.render_error_codes()),
        ],
    );

    // 未登记的错误码要**看得见**：库里出现了码表之外的失败码，
    // 说明有人绕过了「失败分类口径」—— 报告它，但不让它把整条命令判成失败。
    let unknown = summary.unknown_error_codes();
    if !unknown.is_empty() {
        eprintln!(
            "警告：库里出现未登记的错误码：{}（不猜处置动作）",
            unknown.join("、")
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// 重载词典：运维改了词典文件后手动换一代口径。
///
/// **失败即不改**：词典文件坏了 → 报错，**当前这一代原样留着** ——
/// 宁可继续用旧口径（口径留痕说得清），也不换上坏口径（口径变了却看不出来）。
/// 守护进程本身每轮都会检查词典文件是否变了，无需停机重载。
fn run_reload_dictionary(logger: &Logger) -> Result<ExitCode, String> {    let dicts = Dictionaries::get().ok_or_else(|| "词典尚未加载".to_string())?;
    match dicts.reload() {
        Ok(outcome) => {
            println!("{}", outcome.render());
            logger.info(
                "DICT_RELOADED",
                &[
                    ("generation", outcome.generation.to_string()),
                    ("previous_version", outcome.previous_version.clone()),
                    ("version", outcome.current_version.clone()),
                    ("retired_holders", outcome.retired_holders.to_string()),
                ],
            );
            Ok(ExitCode::SUCCESS)
        }
        Err(err) => {
            eprintln!("错误码={} 说明={}", err.code(), err.message());
            eprintln!("词典重载失败（当前口径保持不变）。");
            Ok(ExitCode::from(CONFIG_EXIT_CODE))
        }
    }
}

/// **出站读**：回院内 RIS 取该患者的历史超声报告。
///
/// **凭据只从环境变量读**（`RIS_API_TOKEN`）；端点取配置 / `RIS_BASE_URL`。
/// 取不到历史值时**显式失败**（稳定错误码 + 非零退出码）——
/// 绝不把「没取到」当成「没有差异」糊过去。
fn run_ris_history(config: &Config, args: &[String], logger: &Logger) -> Result<ExitCode, String> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", USAGE);
        return Ok(ExitCode::SUCCESS);
    }

    let patient = opt_value(args, "--patient")?.ok_or("需要 --patient <患者主索引>".to_string())?;
    let body_part = opt_value(args, "--body-part")?;
    let since = opt_value(args, "--since")?;
    let timeout_secs = match opt_value(args, "--timeout")? {
        Some(raw) => raw
            .parse::<u64>()
            .map_err(|_| format!("--timeout 取值不对：{}", raw))?,
        None => config.request_timeout_secs,
    };

    // 端点：配置 / 环境变量（`apply_env_overrides` 已把 RIS_BASE_URL 灌进来）。
    // 凭据：只从环境变量读（配置文件里不落密钥）。
    let creds = Credentials::from_env();
    let mut endpoint = config.ris_base_url.clone().unwrap_or_default();
    if endpoint.trim().is_empty() {
        endpoint = std::env::var(ENV_RIS_BASE_URL).unwrap_or_default();
    }
    let client = match RisClient::new(
        &endpoint,
        &creds.ris_api_token.clone().unwrap_or_default(),
        Duration::from_secs(timeout_secs.max(1)),
    ) {
        Ok(client) => client,
        Err(err) => {
            eprintln!("{}", err.message());
            return Ok(ExitCode::from(RIS_EXIT_CODE));
        }
    };

    let mut query = RisQuery::for_patient(patient.trim());
    if let Some(part) = body_part {
        query = query.with_body_part(part);
    }
    if let Some(from) = since {
        query = query.with_since(from);
    }

    logger.info(
        "RIS_FETCH_STARTED",
        &[
            ("patient_id", query.patient_id.clone()),
            ("endpoint", client.endpoint()),
        ],
    );

    match client.fetch_history(&query) {
        Ok(reports) => {
            logger.info(
                "RIS_FETCH_DONE",
                &[
                    ("patient_id", query.patient_id.clone()),
                    ("reports", reports.len().to_string()),
                ],
            );
            println!(
                "RIS 历史取数完成：患者主索引 {}，历史报告 {} 份",
                query.patient_id,
                reports.len()
            );
            for report in &reports {
                println!(
                    "[{}] 部位={} 检查日期={} 测量值 {} 条",
                    report.study_no,
                    report.body_part.as_deref().unwrap_or("未标注"),
                    report.performed_at.as_deref().unwrap_or("未标注"),
                    report.measurements.len()
                );
                for m in &report.measurements {
                    println!(
                        "  - {} = {} {}",
                        m.item_code,
                        m.value
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "无值".to_string()),
                        m.unit.as_deref().unwrap_or("")
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(err) => {
            logger.error(
                "RIS_FETCH_FAILED",
                &[
                    ("patient_id", query.patient_id.clone()),
                    ("error_code", err.code().to_string()),
                    ("semantic", err.kind.as_str().to_string()),
                    ("attempts", err.attempts.to_string()),
                ],
            );
            eprintln!(
                "错误码={} 语义={} 明细={} 尝试次数={} 原因={}",
                err.code(),
                err.kind.as_str(),
                err.detail
                    .map(|d| d.as_str())
                    .unwrap_or("无"),
                err.attempts,
                err.reason
            );
            Ok(ExitCode::from(RIS_EXIT_CODE))
        }
    }
}

/// 拆出全局参数（其余参数留给子命令）—— 实现在 [`ultrasound_report_struct::cli::args`]。
fn run_parse(args: &[String]) -> Result<ExitCode, String> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", USAGE);
        return Ok(ExitCode::SUCCESS);
    }

    let mut text: Option<String> = None;
    let mut file: Option<String> = None;
    let mut idx = 0usize;
    while idx < args.len() {
        match args[idx].as_str() {
            "--text" => {
                idx += 1;
                text = Some(
                    args.get(idx)
                        .cloned()
                        .ok_or_else(|| "--text 缺少取值".to_string())?,
                );
            }
            "--file" => {
                idx += 1;
                file = Some(
                    args.get(idx)
                        .cloned()
                        .ok_or_else(|| "--file 缺少取值".to_string())?,
                );
            }
            other => return Err(format!("未知参数：{}", other)),
        }
        idx += 1;
    }

    let body = match (text, file) {
        (Some(t), _) => t,
        (None, Some(p)) => {
            // 读文件 → **编码判别**（UTF-8 → GBK → 混编 / 未知）。
            // 判不出来就**显式失败**：不猜转码、不产出条目，
            // 并以非零退出码把错误码报给调用方。
            let bytes = std::fs::read(&p)
                .map_err(|e| format!("读取文件失败（{}）：{}", p, e))?;
            match decode_report(&bytes) {
                Ok(decoded) => decoded.text,
                Err(e) => {
                    eprintln!("错误码={} 说明={}", e.error_code().as_str(), e.reason());
                    return Ok(ExitCode::from(ENCODING_EXIT_CODE));
                }
            }
        }
        (None, None) => return Err("需要 --text <文本> 或 --file <路径>".to_string()),
    };

    let outcome = parse_report(&body);
    print_outcome(&outcome);
    Ok(ExitCode::SUCCESS)
}

/// 建状态库（幂等）。
fn run_init_db(config: &Config, dry_run: bool) -> Result<ExitCode, String> {
    let path = &config.state_db;
    if dry_run {
        println!(
            "演练模式：将建状态库 {}（schema 版本 {}）—— 本次不落库。",
            path.display(),
            ultrasound_report_struct::state::schema::SCHEMA_VERSION
        );
        return Ok(ExitCode::SUCCESS);
    }
    let repo = Repo::open(path).map_err(|e| format!("建库失败：{}", e))?;
    let version: i64 = repo
        .connection()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|e| format!("读取 schema 版本失败：{}", e))?;
    println!("状态库已就绪：{}（schema 版本 {}）", path.display(), version);
    Ok(ExitCode::SUCCESS)
}

/// 扫投递目录批次并落库。
///
/// **`--dry-run`（演练）**：照常解码、解析、强转，但**一个字节都不落库** ——
/// 运维先看一眼「这批会解析成什么样」，再决定要不要真跑。
fn run_ingest(
    config: &Config,
    args: &[String],
    logger: &Logger,
    dry_run: bool,
) -> Result<ExitCode, String> {
    let inbox = opt_value(args, "--inbox")?
        .ok_or_else(|| "需要 --inbox <目录>".to_string())?;
    let db_path = &config.state_db;

    if dry_run {
        // 演练：**只读**投递目录，不碰状态库。
        let dirs = ultrasound_report_struct::ingest::list_report_dirs(std::path::Path::new(&inbox))
            .map_err(|e| format!("读投递目录失败：{}", e))?;
        let mut reports = 0usize;
        let mut items = 0usize;
        let mut failures = 0usize;
        let mut encoding_failed = 0usize;
        for dir in &dirs {
            let scanned =
                ultrasound_report_struct::ingest::read_report_dir(dir)
                    .map_err(|e| format!("读投递目录失败：{}", e))?;
            let scanned = match scanned {
                Some(scanned) => scanned,
                None => continue,
            };
            reports += 1;
            items += scanned.items.len();
            failures += scanned.failures.len();
            if scanned.encoding_failed {
                encoding_failed += 1;
            }
        }
        logger.info(
            "INGEST_DRY_RUN",
            &[
                ("inbox", inbox.clone()),
                ("reports", reports.to_string()),
                ("items", items.to_string()),
                ("failures", failures.to_string()),
                ("encoding_failed", encoding_failed.to_string()),
            ],
        );
        println!(
            "演练模式（不落库）：投递目录 {} 份报告，将落 {} 条台账行，失败条目 {} 条（编码判不出 {} 份）",
            reports, items, failures, encoding_failed
        );
        println!("状态库：{}（本次未改动）", db_path.display());
        return Ok(ExitCode::SUCCESS);
    }

    let repo = Repo::open(db_path).map_err(|e| format!("打开状态库失败：{}", e))?;
    let summary = ingest_inbox(&repo, std::path::Path::new(&inbox))
        .map_err(|e| format!("入库失败：{}", e))?;

    let mut run = RunSummary::new();
    run.record_processed_n(summary.reports as u64);
    run.record_failed_n(summary.failures as u64);

    logger.info(
        "INGEST_BATCH_DONE",
        &[
            ("inbox", inbox.clone()),
            ("reports", summary.reports.to_string()),
            ("items", summary.items.to_string()),
            ("failures", summary.failures.to_string()),
            ("duplicates", summary.duplicates.to_string()),
            ("skipped", summary.skipped.to_string()),
            ("encoding_failed", summary.encoding_failed.to_string()),
        ],
    );
    println!(
        "投递批次处理完成：报告 {} 份，条目 {} 条，失败条目 {} 条，重复跳过 {} 份，跳过目录 {} 个，编码判不出 {} 份",
        summary.reports,
        summary.items,
        summary.failures,
        summary.duplicates,
        summary.skipped,
        summary.encoding_failed
    );
    println!("{}", run.render());
    println!("状态库：{}", db_path.display());
    Ok(ExitCode::SUCCESS)
}

/// 导出台账与失败清单。
///
/// **`--dry-run`（演练）**：报「将导出到哪、多少行」，**不写文件**。
fn run_export(
    config: &Config,
    args: &[String],
    logger: &Logger,
    dry_run: bool,
) -> Result<ExitCode, String> {
    let out_dir = opt_value(args, "--out")?
        .ok_or_else(|| "需要 --out <目录>".to_string())?;
    let db_path = &config.state_db;

    let repo = Repo::open(db_path).map_err(|e| format!("打开状态库失败：{}", e))?;

    if dry_run {
        let ledger_rows: i64 = repo
            .connection()
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .map_err(|e| format!("统计台账行数失败：{}", e))?;
        let failure_rows: i64 = repo
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM audit_log WHERE action = 'failed'",
                [],
                |row| row.get(0),
            )
            .map_err(|e| format!("统计失败条目数失败：{}", e))?;
        logger.info(
            "EXPORT_DRY_RUN",
            &[
                ("out", out_dir.clone()),
                ("ledger_rows", ledger_rows.to_string()),
                ("failure_rows", failure_rows.to_string()),
            ],
        );
        println!(
            "演练模式（不写文件）：将导出到 {}/ledger.csv（{} 行）与 {}/failures.csv（{} 行）",
            out_dir, ledger_rows, out_dir, failure_rows
        );
        return Ok(ExitCode::SUCCESS);
    }

    let summary = export_all(repo.connection(), std::path::Path::new(&out_dir))
        .map_err(|e| format!("导出失败：{}", e))?;

    logger.info(
        "EXPORT_DONE",
        &[
            ("out", out_dir.clone()),
            ("ledger_rows", summary.ledger_rows.to_string()),
            ("failure_rows", summary.failure_rows.to_string()),
        ],
    );
    println!(
        "导出完成：台账 {}（{} 行）/ 失败清单 {}（{} 行）",
        summary.ledger_path.display(),
        summary.ledger_rows,
        summary.failure_path.display(),
        summary.failure_rows
    );
    Ok(ExitCode::SUCCESS)
}

fn print_outcome(outcome: &ParseOutcome) {
    println!(
        "解析完成：条目 {} 条，失败 {} 条",
        outcome.items.len(),
        outcome.failures.len()
    );
    for item in &outcome.items {
        println!(
            "[条目] part_key={} item_code={} value_raw={} unit_raw={} value_si={} unit_code={} coercion_path={} line={} column={}",
            item.part_key.clone().unwrap_or_else(|| "-".to_string()),
            item.item_code.clone().unwrap_or_else(|| "-".to_string()),
            fmt_num(item.value_raw),
            item.unit_raw.clone().unwrap_or_else(|| "-".to_string()),
            fmt_num(item.value_si),
            item.unit_code.clone().unwrap_or_else(|| "-".to_string()),
            item.coercion_path.as_str(),
            item.line,
            item.column
        );
    }
    for failure in &outcome.failures {
        println!(
            "[失败] error_code={} line={} column={} raw_snippet={} detail={}",
            failure.code.as_str(),
            failure.line,
            failure.column,
            failure.raw_snippet,
            failure.message()
        );
    }
}

/// 数值输出：整数不带小数尾巴（`32` 而不是 `32.0`）。
fn fmt_num(v: Option<f64>) -> String {
    match v {
        None => "-".to_string(),
        Some(n) => {
            if n.fract() == 0.0 && n.abs() < 1e15 {
                format!("{}", n as i64)
            } else {
                format!("{}", n)
            }
        }
    }
}
