# Ultrasound Report Structuring System

> A resident service that parses ultrasound workstation deliveries on arrival into a normalized measurement ledger, compares each value against the previous same-side result for that patient, and refuses to guess: anything it cannot read is reported back with its exact line and column so a physician can confirm item by item before anything is written back to the hospital EMR.

Ultrasound reports arrive as free text — body-part names vary, units mix centimeters and millimeters, measurement item names are inconsistent, and a dropped unit usually goes unnoticed. This system is a **single resident executable**: it watches the ultrasound workstation delivery directory, and on every arrival it parses the report, normalizes units, compares against the same-side history held in the hospital RIS, and produces a filterable structured ledger plus a failure list that points back to source line/column. Deltas travel into the EMR message and the WeCom (企业微信) notification only after the physician confirms. The ledger is a **draft pending confirmation** — until an ultrasound physician confirms or rejects each row, no write-back request is sent to the EMR structured-report interface.

**No changes to HIS / LIS / PACS, no ultrasound workstation plugin**: the workstation only has to export reports into the delivery directory — the single most important cost difference when this product is deployed inside a hospital. **Who it is for**: ultrasound department QC staff, ultrasound physicians, and hospital IT operations staff who need a filterable measurement ledger, same-side historical comparison, and a daily run summary.

## Core Capabilities

- **Automatic measurement extraction** — every number in the report is extracted together with its body part, measurement item, and source position, precise down to line and column, not just a bare value.
- **Units normalized to the hospital convention** — length is always millimeters, volume always milliliters; each record states which conversion path it took, and values with **no valid conversion path are never force-converted**.
- **Same-side historical comparison** — automatically matches the previous value for the same patient + body part + item in the RIS, producing the delta and its direction; first-time exams and ambiguous multi-history matches are flagged explicitly rather than conflated.
- **Failures are never silent** — missing unit, unrecognized body-part wording, item not in the dictionary, and unparseable values each get a stable error code plus a source position, so a dropped unit is always noticed and dictionary gaps can be aggregated by code.
- **Written back only after physician confirmation** — the product emits a draft pending confirmation; only after each item is confirmed or rejected does the write-back request go to the EMR, with a WeCom push alongside it. Nothing is auto-written and nothing is signed on the physician's behalf.
- **Duplicate deliveries collapse to one** — resending the same report, whether by machine or by hand, leaves both the ledger row count and the number of write-backs unchanged.

## Quick Start

### Requirements

Rust stable (>= 1.74, edition 2021). If `cargo` is not on your PATH, run `export PATH="$HOME/.cargo/bin:$PATH"`. No system OpenSSL is required — outbound HTTPS is implemented in pure Rust, so offline hospital environments need no extra libraries. Report bodies are supported in **UTF-8 and GBK**, the two encodings common in Chinese hospitals (the GBK table is built into a dependency); mixed-encoding or non-text files are **rejected explicitly** rather than blindly transcoded. Outbound integration requires reachable in-hospital RIS / EMR endpoints; when credentials are missing, the startup self-check reports exactly which environment variable is absent instead of running on defaults.

### Install, configure, verify

```bash
cd ultrasound-report-struct
cargo build --release                       # artifact: target/release/ultrasound-report-struct

cp config.example.toml config.toml         # edit directory paths and in-hospital endpoints
export RIS_BASE_URL=https://ris.hospital.internal
export RIS_API_TOKEN=...                    # RIS access token
export EMR_BASE_URL=https://emr.hospital.internal
export EMR_API_TOKEN=...                    # EMR access token
export WECOM_WEBHOOK_URL=https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=...
export DAEMON_API_TOKEN=...                 # auth token for the physician-confirmation receipt endpoint

export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release && cargo test && cargo run -- selftest   # build -> all tests -> self-check
```

Tokens and secrets live **only in environment variables** — never in the config file, never in the logs (full list in `.env.example`). A config file containing keys named `token` / `secret` / `password` / `webhook` is **rejected outright** with error code `CONFIG_SECRET_IN_FILE`, because config files often get forwarded into department chat groups and a leaked secret is a leaked secret. When `--config` is supplied the process first runs a **startup self-check**: dictionary files parse, the state database directory is writable, required environment variables are present; missing items are listed one by one with stable error codes and the process exits with code **3** rather than running on half a configuration.

Walk the bundled de-identified samples through delivery -> ledger -> export:
```bash
cargo run -- init-db                             # create the state store (idempotent migration, safe to repeat)
cargo run -- ingest --inbox assets/samples/inbox # scan the delivery directory: parse -> coerce -> write ledger
cargo run -- export --out ./export-out           # export ledger CSV and failure-list CSV
# expected: 1 ledger row (3.2cm -> 32mm via UnitConvert); 1 failure row (UNIT_MISSING, with line and column)
```

### Main commands

```bash
ultrasound-report-struct --config config.toml daemon     # resident watcher: processes each report on arrival
ultrasound-report-struct parse --text '左肾积水 3.2cm'  # parse one report and print structured items
ultrasound-report-struct ingest --inbox inbox           # batch-process a delivery directory
ultrasound-report-struct export --out ./export-out      # export ledger / failure-list CSVs
ultrasound-report-struct summary --yesterday            # run summary: how many reports failed to parse yesterday
ultrasound-report-struct replay --inbox inbox           # offline replay of the same batch (idempotency and backfill)
```

This product is a resident process with **no graphical interface** — the working surface is the terminal session itself: a background process parked in a small program, appending one structured log line per processed report and emitting heartbeat counters each round. The physician-confirmation receipt endpoint is built in and listens on `127.0.0.1:8731` (internal network only) at `POST /api/v1/reports/{study_no}/review`, accepting `decision=confirm|reject` plus reviewer and note, authenticated with the `DAEMON_API_TOKEN` bearer token.

## Repository Structure

```
ultrasound-report-struct/
├── src/            # Rust sources: CLI entry, config, ingest/parse/coerce pipeline, compare, workflow state machine, outbound clients
├── tests/          # unit tests, protocol contract tests, end-to-end acceptance scripts
├── assets/         # in-hospital body-part and item dictionaries, de-identified samples and annotation tables, public data with sha256 checks
├── docs/           # concepts/ visual goal card, screenshots/ real captured runs with .txt transcripts, negatives/ negative-corpus report
├── scripts/        # scripted public-data download (re-runnable, sha256-verified, offline fallback)
├── config.example.toml / .env.example  # config template (paths and in-hospital endpoints, no secrets) + environment variable checklist
├── Cargo.toml     # narrowed to 8 crates: serde / serde_json / regex / ureq+rustls / sha2 / rusqlite / encoding_rs (GBK table)
└── README.md      # full Chinese product documentation
```


Full documentation: see README.md (Chinese).

---

## Follow Us

Scan the QR code to follow our official account for updates and community access:

![Follow Us](qrcode.jpg)