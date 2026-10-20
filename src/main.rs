mod cli;

use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand, ValueEnum};

use cli::ingest::{self, IngestOpts};
use cli::print::{self, Mode, Printer, exit_on_broken_pipe, thousands};
use rotlog::config::{Console, Dur, Format, Fsync, Size, human_size, parse_duration, parse_size};
use rotlog::maintain::{self, Event};
use rotlog::read::{self, Cond, Filter, Record};
use rotlog::rotation::{Every, Paths};
use rotlog::time::{Tz, now_micros, parse_when};
use rotlog::{Config, Level, Logger, Value, control, upload};

#[derive(Parser)]
#[command(
    name = "rotlog",
    version,
    about = "Fast rotating logger: write, rotate, gzip, upload to Google Cloud Storage, tail and query logs",
    arg_required_else_help = true,
    after_help = "Examples:\n  \
        bun server.ts | rotlog pipe -N api\n  \
        rotlog run -N api --every hourly --bucket my-logs -- bun server.ts\n  \
        rotlog write -l error 'backup failed' -f job=nightly -f code=3\n  \
        rotlog tail -f -l warn\n  \
        rotlog query --since 2h --status 5xx --user alice\n  \
        rotlog stats --since 1d\n  \
        rotlog level debug        # change a running writer's level"
)]
struct Cli {
    #[command(flatten)]
    stream: StreamArgs,
    /// Colorize output
    #[arg(long, global = true, value_enum, default_value_t = ColorWhen::Auto)]
    color: ColorWhen,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum ColorWhen {
    Auto,
    Always,
    Never,
}

#[derive(Args)]
#[command(next_help_heading = "Stream options")]
struct StreamArgs {
    /// Config file (TOML or JSON) [default: ./rotlog.toml if present]
    #[arg(short, long, global = true, env = "ROTLOG_CONFIG")]
    config: Option<PathBuf>,
    /// Log directory
    #[arg(short, long, global = true, env = "ROTLOG_DIR")]
    dir: Option<PathBuf>,
    /// Stream name: the active file is <dir>/<name>.<ext>
    #[arg(short = 'N', long, global = true, env = "ROTLOG_NAME")]
    name: Option<String>,
    /// Active file extension
    #[arg(long, global = true)]
    ext: Option<String>,
    /// Minimum level written by this process
    #[arg(long, global = true)]
    min_level: Option<Level>,
    /// Line format
    #[arg(long, global = true, value_parser = ["text", "json"])]
    format: Option<String>,
    /// Time zone for timestamps, boundaries and archive names
    #[arg(long, global = true, value_parser = ["local", "utc"])]
    tz: Option<String>,
    /// Rotation schedule: never, minutely, hourly, daily, weekly[:sun], 15m, 6h, 2d…
    #[arg(long, global = true)]
    every: Option<String>,
    /// Also rotate past this size (100MB, 1GB; 0 = off)
    #[arg(long, global = true)]
    max_size: Option<String>,
    /// Archives kept locally (0 = all)
    #[arg(long, global = true)]
    keep: Option<u32>,
    /// Delete local archives older than this (30d; 0 = never)
    #[arg(long, global = true)]
    max_age: Option<String>,
    /// Gzip archives this long after rotation
    #[arg(long, global = true)]
    compress_after: Option<String>,
    /// Do not compress archives
    #[arg(long, global = true)]
    no_compress: bool,
    /// Google Cloud Storage bucket (enables upload)
    #[arg(long, global = true, env = "ROTLOG_BUCKET")]
    bucket: Option<String>,
    /// Object name prefix, e.g. "prod/"
    #[arg(long, global = true)]
    prefix: Option<String>,
    /// Object name template ({prefix} {host} {name} {file} {yyyy} {mm} {dd} {hh})
    #[arg(long, global = true)]
    object: Option<String>,
    /// Upload archives this long after rotation/compression
    #[arg(long, global = true)]
    upload_after: Option<String>,
    /// Delete local archives once uploaded
    #[arg(long, global = true)]
    delete_after_upload: bool,
    /// Do not upload even if a bucket is configured
    #[arg(long, global = true)]
    no_upload: bool,
    /// Service account / ADC JSON file
    #[arg(long, global = true)]
    credentials: Option<PathBuf>,
    /// Storage API endpoint (emulators)
    #[arg(long, global = true)]
    endpoint: Option<String>,
    /// Echo written lines to the console
    #[arg(long, global = true, value_parser = ["off", "stdout", "stderr"])]
    console: Option<String>,
    /// Max time a record waits in memory before being written (0 = immediately)
    #[arg(long, global = true)]
    flush_interval: Option<String>,
    /// When to fsync
    #[arg(long, global = true, value_parser = ["never", "rotate", "always"])]
    fsync: Option<String>,
    /// How often the writer compresses/uploads/prunes in the background (0 = never)
    #[arg(long, global = true)]
    maintenance_interval: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write a commented rotlog.toml
    Init {
        #[arg(default_value = "rotlog.toml")]
        path: PathBuf,
        /// Overwrite an existing file
        #[arg(long)]
        force: bool,
    },
    /// Print the effective configuration (file + env + flags) as TOML
    Config,
    /// Append one record (scripts, cron jobs, CI)
    Write(WriteArgs),
    /// Log every line read from stdin: `app | rotlog pipe`
    Pipe(PipeArgs),
    /// Run a command and log its stdout and stderr: `rotlog run -- bun server.ts`
    Run(RunArgs),
    /// Show the last records and optionally follow new ones
    Tail(TailArgs),
    /// Search records across the active file and all archives (gzip included)
    Query(QueryArgs),
    /// Summarize levels, status codes, latency, top routes, users and errors
    Stats(StatsArgs),
    /// List the stream's files
    Ls(LsArgs),
    /// Show the writing process, its counters and the stream's files
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Archive the active file now (through the running writer if there is one)
    Rotate,
    /// Make the running writer write its buffer to disk
    Flush,
    /// Show or change the running writer's level: `rotlog level debug`
    Level { level: Option<Level> },
    /// Gzip archives that are due (all of them with --now)
    Compress(MaintArgs),
    /// Upload archives to Google Cloud Storage (check access with --check)
    Upload(UploadArgs),
    /// Delete archives beyond retention (keep / max_age)
    Prune(MaintArgs),
    /// Compress, upload and prune: once, or continuously with --watch
    Maintain(MaintainArgs),
    /// Measure throughput and per-call latency on this machine
    Bench(BenchArgs),
}

#[derive(Args, Clone, Default)]
#[command(next_help_heading = "Filters")]
struct FilterArgs {
    /// Minimum level: trace, debug, info, warn, error, fatal
    #[arg(short, long)]
    level: Option<Level>,
    /// Logger name
    #[arg(long)]
    logger: Option<String>,
    /// Case-insensitive text anywhere in the line
    #[arg(short, long)]
    grep: Option<String>,
    /// From: RFC 3339, "YYYY-MM-DD[ HH:MM]", or an age like 15m, 2h, 7d
    #[arg(long)]
    since: Option<String>,
    /// Until (same formats as --since)
    #[arg(long)]
    until: Option<String>,
    /// Field condition (repeatable): status>=500, user=alice, path~/api, status=5xx, dur_ms>250
    #[arg(short = 'w', long = "where")]
    conds: Vec<String>,
    /// Same as --where status=<v> (404, 5xx)
    #[arg(long)]
    status: Option<String>,
    /// Same as --where user=<v>
    #[arg(long)]
    user: Option<String>,
    /// Same as --where method=<v>
    #[arg(long)]
    method: Option<String>,
    /// Path contains
    #[arg(long)]
    path: Option<String>,
    /// Same as --where req_id=<v>
    #[arg(long)]
    req: Option<String>,
}

#[derive(Args, Clone, Copy, Default)]
#[command(next_help_heading = "Output")]
struct OutputArgs {
    /// Print lines exactly as stored
    #[arg(long, conflicts_with = "json")]
    raw: bool,
    /// Print records as normalized JSON lines
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct WriteArgs {
    /// Record level
    #[arg(short, long, default_value = "info")]
    level: Level,
    /// Logger name (default: stream name)
    #[arg(long)]
    logger: Option<String>,
    /// Field (repeatable): -f user=alice -f status=500 -f ok=true
    #[arg(short, long = "field")]
    fields: Vec<String>,
    /// Fields as a JSON object
    #[arg(long, conflicts_with = "fields")]
    json_fields: Option<String>,
    /// Message (several words are joined with spaces)
    #[arg(required = true, num_args = 1..)]
    message: Vec<String>,
}

#[derive(Args)]
struct PipeArgs {
    /// Logger name (default: stream name)
    #[arg(long)]
    logger: Option<String>,
    /// Level of lines without a recognizable level
    #[arg(long, default_value = "info")]
    default_level: Level,
    /// Do not detect levels in lines (ERROR, [warn], level=debug, JSON "level")
    #[arg(long)]
    no_detect: bool,
    /// Keep ANSI color codes
    #[arg(long)]
    keep_ansi: bool,
    /// Also copy input to stdout
    #[arg(long)]
    tee: bool,
}

#[derive(Args)]
struct RunArgs {
    /// Logger name (default: stream name)
    #[arg(long)]
    logger: Option<String>,
    /// Level of stdout lines without a recognizable level
    #[arg(long, default_value = "info")]
    stdout_level: Level,
    /// Level of stderr lines without a recognizable level
    #[arg(long, default_value = "error")]
    stderr_level: Level,
    /// Do not detect levels in lines
    #[arg(long)]
    no_detect: bool,
    /// Keep ANSI color codes
    #[arg(long)]
    keep_ansi: bool,
    /// Also pass the child's output through to our stdout/stderr
    #[arg(long)]
    tee: bool,
    /// Command and arguments
    #[arg(required = true, num_args = 1.., trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Args)]
struct TailArgs {
    /// Number of records to show first
    #[arg(short = 'n', long, default_value_t = 20)]
    lines: usize,
    /// Keep printing new records (follows rotations)
    #[arg(short, long)]
    follow: bool,
    #[command(flatten)]
    filter: FilterArgs,
    #[command(flatten)]
    output: OutputArgs,
}

#[derive(Args)]
struct QueryArgs {
    /// Stop after this many matches
    #[arg(long)]
    limit: Option<usize>,
    /// Only print the number of matches
    #[arg(long)]
    count: bool,
    #[command(flatten)]
    filter: FilterArgs,
    #[command(flatten)]
    output: OutputArgs,
}

#[derive(Args)]
struct StatsArgs {
    /// Rows in each top list
    #[arg(long, default_value_t = 10)]
    top: usize,
    /// Print JSON
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    filter: FilterArgs,
}

#[derive(Args)]
struct LsArgs {
    /// Print JSON
    #[arg(long)]
    json: bool,
    /// List every stream found in the directory
    #[arg(long)]
    all: bool,
}

#[derive(Args, Clone, Copy)]
struct MaintArgs {
    /// Ignore the configured delays (compress.after, upload.after)
    #[arg(long)]
    now: bool,
    /// Only show what would be done
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args)]
struct UploadArgs {
    #[command(flatten)]
    maint: MaintArgs,
    /// Verify credentials and bucket access, upload nothing
    #[arg(long)]
    check: bool,
}

#[derive(Args)]
struct MaintainArgs {
    #[command(flatten)]
    maint: MaintArgs,
    /// Keep running, every --interval (default: maintenance_interval)
    #[arg(long)]
    watch: bool,
    #[arg(long)]
    interval: Option<String>,
}

#[derive(Args)]
struct BenchArgs {
    /// Records per thread
    #[arg(long, default_value_t = 1_000_000)]
    records: u64,
    /// Writer threads
    #[arg(long, default_value_t = 1)]
    threads: u32,
    /// Message length in bytes
    #[arg(long, default_value_t = 64)]
    msg_size: usize,
    /// Attach 4 structured fields to each record
    #[arg(long)]
    with_fields: bool,
    /// Keep the generated files (in --dir, default a temp dir)
    #[arg(long)]
    keep_files: bool,
}

type Res<T = ()> = Result<T, String>;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let color = match cli.color {
        ColorWhen::Always => true,
        ColorWhen::Never => false,
        ColorWhen::Auto => std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
    };
    match dispatch(cli, color) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("rotlog: {e}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(cli: Cli, color: bool) -> Res<i32> {
    let s = &cli.stream;
    match cli.cmd {
        Cmd::Init { path, force } => init(&path, force),
        Cmd::Config => {
            print!("{}", load_config(s)?.to_toml());
            Ok(0)
        }
        Cmd::Write(a) => write_one(load_config(s)?, a),
        Cmd::Pipe(a) => {
            ingest::signals::block();
            let log = open(load_config(s)?)?;
            let opts = IngestOpts { logger: a.logger, detect: !a.no_detect, keep_ansi: a.keep_ansi, tee: a.tee };
            Ok(ingest::pipe(log, opts, a.default_level))
        }
        Cmd::Run(a) => {
            ingest::signals::block();
            let log = open(load_config(s)?)?;
            let opts = IngestOpts { logger: a.logger, detect: !a.no_detect, keep_ansi: a.keep_ansi, tee: a.tee };
            Ok(ingest::run(log, opts, &a.command, a.stdout_level, a.stderr_level))
        }
        Cmd::Tail(a) => tail(&load_config(s)?, a, color),
        Cmd::Query(a) => query(&load_config(s)?, a, color),
        Cmd::Stats(a) => stats(&load_config(s)?, a, color),
        Cmd::Ls(a) => ls(&load_config(s)?, a),
        Cmd::Status { json } => status(&load_config(s)?, json),
        Cmd::Rotate => rotate(load_config(s)?),
        Cmd::Flush => ctl(&load_config(s)?, "flush"),
        Cmd::Level { level } => match level {
            Some(l) => ctl(&load_config(s)?, &format!("level {l}")),
            None => ctl(&load_config(s)?, "level"),
        },
        Cmd::Compress(m) => run_maint(&load_config(s)?, m, (true, false, false)),
        Cmd::Upload(a) => {
            let cfg = load_config(s)?;
            if a.check {
                return check_upload(&cfg);
            }
            if !cfg.upload.active() {
                return Err("no bucket configured (set [upload] bucket or --bucket)".into());
            }
            run_maint(&cfg, a.maint, (true, true, false))
        }
        Cmd::Prune(m) => run_maint(&load_config(s)?, m, (false, false, true)),
        Cmd::Maintain(a) => maintain_cmd(&load_config(s)?, a),
        Cmd::Bench(a) => bench(load_config(s)?, a, s.dir.is_some()),
    }
}

// ---- configuration -----------------------------------------------------

fn load_config(s: &StreamArgs) -> Res<Config> {
    let mut c = match &s.config {
        Some(p) => Config::from_file(p)?,
        None if Path::new("rotlog.toml").exists() => Config::from_file(Path::new("rotlog.toml"))?,
        None => Config::default(),
    };
    let dur = |v: &str| parse_duration(v).map(Dur);
    if let Some(v) = &s.dir {
        c.dir = v.clone();
    }
    if let Some(v) = &s.name {
        c.name = v.clone();
    }
    if let Some(v) = &s.ext {
        c.ext = v.clone();
    }
    if let Some(v) = s.min_level {
        c.level = v;
    }
    if let Some(v) = &s.format {
        c.format = if v == "json" { Format::Json } else { Format::Text };
    }
    if let Some(v) = &s.tz {
        c.timezone = if v == "utc" { Tz::Utc } else { Tz::Local };
    }
    if let Some(v) = &s.every {
        c.rotation.every = Every::parse(v)?;
    }
    if let Some(v) = &s.max_size {
        c.rotation.max_size = Size(parse_size(v)?);
    }
    if let Some(v) = s.keep {
        c.rotation.keep = v;
    }
    if let Some(v) = &s.max_age {
        c.rotation.max_age = dur(v)?;
    }
    if let Some(v) = &s.compress_after {
        c.compress.after = dur(v)?;
    }
    if s.no_compress {
        c.compress.enabled = false;
    }
    if let Some(v) = &s.bucket {
        c.upload.bucket = v.clone();
    }
    if s.no_upload {
        c.upload.bucket.clear();
    }
    if let Some(v) = &s.prefix {
        c.upload.prefix = v.clone();
    }
    if let Some(v) = &s.object {
        c.upload.object = v.clone();
    }
    if let Some(v) = &s.upload_after {
        c.upload.after = dur(v)?;
    }
    if s.delete_after_upload {
        c.upload.delete_local = true;
    }
    if let Some(v) = &s.credentials {
        c.upload.credentials = Some(v.clone());
    }
    if let Some(v) = &s.endpoint {
        c.upload.endpoint = v.clone();
    }
    if let Some(v) = &s.console {
        c.console = match v.as_str() {
            "stdout" => Console::Stdout,
            "stderr" => Console::Stderr,
            _ => Console::Off,
        };
    }
    if let Some(v) = &s.flush_interval {
        c.flush_interval = dur(v)?;
    }
    if let Some(v) = &s.maintenance_interval {
        c.maintenance_interval = dur(v)?;
    }
    if let Some(v) = &s.fsync {
        c.fsync = match v.as_str() {
            "never" => Fsync::Never,
            "always" => Fsync::Always,
            _ => Fsync::Rotate,
        };
    }
    c.validate()?;
    Ok(c)
}

fn open(cfg: Config) -> Res<Logger> {
    Logger::open(cfg).map_err(|e| e.to_string())
}

fn init(path: &Path, force: bool) -> Res<i32> {
    if path.exists() && !force {
        return Err(format!("{} exists (use --force to overwrite)", path.display()));
    }
    fs::write(path, cli::INIT_TEMPLATE).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(0)
}

fn build_filter(f: &FilterArgs, tz: Tz) -> Res<Filter> {
    let now = now_micros();
    let when = |s: &Option<String>| -> Res<Option<i64>> {
        s.as_deref().map(|v| parse_when(v, tz, now).ok_or_else(|| format!("invalid time '{v}'"))).transpose()
    };
    let mut conds = f.conds.iter().map(|c| Cond::parse(c)).collect::<Res<Vec<_>>>()?;
    for (key, v) in [("status", &f.status), ("user", &f.user), ("method", &f.method), ("req_id", &f.req)] {
        if let Some(v) = v {
            conds.push(Cond { key: key.into(), op: read::Op::Eq, value: v.clone() });
        }
    }
    if let Some(p) = &f.path {
        conds.push(Cond { key: "path".into(), op: read::Op::Contains, value: p.clone() });
    }
    Ok(Filter {
        min_level: f.level,
        logger: f.logger.clone(),
        grep: f.grep.as_ref().map(|g| g.to_lowercase()),
        since: when(&f.since)?,
        until: when(&f.until)?,
        conds,
    })
}

fn mode(o: OutputArgs) -> Mode {
    if o.raw {
        Mode::Raw
    } else if o.json {
        Mode::Json
    } else {
        Mode::Pretty
    }
}

// ---- writing -------------------------------------------------------------

fn parse_field(s: &str) -> Res<(&str, Value<'_>)> {
    let (k, v) = s.split_once('=').ok_or_else(|| format!("invalid field '{s}' (expected key=value)"))?;
    let val = if let Ok(n) = v.parse::<i64>() {
        Value::I64(n)
    } else if let Ok(f) = v.parse::<f64>().map_err(|_| ()).and_then(|f| if f.is_finite() { Ok(f) } else { Err(()) }) {
        Value::F64(f)
    } else if v == "true" || v == "false" {
        Value::Bool(v == "true")
    } else if v == "null" {
        Value::Null
    } else {
        Value::Str(v)
    };
    Ok((k, val))
}

fn write_one(mut cfg: Config, a: WriteArgs) -> Res<i32> {
    let msg = a.message.join(" ");
    let fields = a.fields.iter().map(|f| parse_field(f)).collect::<Res<Vec<_>>>()?;
    let name = a.logger.clone().unwrap_or_else(|| cfg.name.clone());
    let paths = Paths::new(&cfg);
    if control::owner(&paths).is_some() {
        // Another process owns the stream: append atomically, leave rotation to it.
        let mut body = Vec::with_capacity(256);
        let mut ts = rotlog::time::TsFormatter::new(cfg.timezone);
        rotlog::format::write_header(cfg.format, &mut ts, now_micros(), &mut body);
        let f = match &a.json_fields {
            Some(j) => rotlog::format::Fields::Json(j),
            None => rotlog::format::Fields::Kv(&fields),
        };
        rotlog::format::encode_body(cfg.format, &mut body, a.level, &name, &msg, b"", f);
        let mut file =
            fs::OpenOptions::new().append(true).create(true).open(paths.active()).map_err(|e| e.to_string())?;
        file.write_all(&body).map_err(|e| e.to_string())?;
        return Ok(0);
    }
    cfg.maintenance_interval = Dur(Duration::ZERO);
    let log = open(cfg)?.named(&name);
    match &a.json_fields {
        Some(j) => {
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(j)
                .map_err(|e| format!("--json-fields: {e}"))?;
            log.log_json(a.level, None, &msg, j);
        }
        None => log.log(a.level, &msg, &fields),
    }
    log.close();
    Ok(0)
}

// ---- reading -------------------------------------------------------------

fn tail(cfg: &Config, a: TailArgs, color: bool) -> Res<i32> {
    let filter = build_filter(&a.filter, cfg.timezone)?;
    let paths = Paths::new(cfg);
    let active = paths.active();
    let mut printer = Printer::new(mode(a.output), color, cfg.timezone);
    let keep = |l: &str| filter.line_may_match(l) && filter.matches(&Record::parse(l));
    let (lines, mut pos) = match read::tail_lines(&active, a.lines, &keep) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && a.follow => (Vec::new(), 0),
        Err(e) => return Err(format!("{}: {e}", active.display())),
    };
    for l in &lines {
        exit_on_broken_pipe(printer.print(l, &Record::parse(l)));
    }
    exit_on_broken_pipe(printer.flush());
    if !a.follow {
        return Ok(0);
    }
    follow(&active, &mut pos, &filter, &mut printer)
}

fn file_id(p: &Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(p).ok().map(|m| (m.dev(), m.ino()))
    }
    #[cfg(not(unix))]
    {
        fs::metadata(p).ok().map(|m| (0, m.len()))
    }
}

/// Reads `f` to EOF, printing complete lines. Returns whether anything was read.
fn drain(
    f: &mut fs::File,
    pos: &mut u64,
    partial: &mut Vec<u8>,
    buf: &mut [u8],
    filter: &Filter,
    printer: &mut Printer,
) -> Res<bool> {
    use std::io::Read;
    let mut got = false;
    loop {
        let n = f.read(buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        got = true;
        *pos += n as u64;
        partial.extend_from_slice(&buf[..n]);
        let mut start = 0;
        while let Some(i) = partial[start..].iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&partial[start..start + i]);
            if filter.line_may_match(&line) {
                let r = Record::parse(&line);
                if filter.matches(&r) {
                    exit_on_broken_pipe(printer.print(&line, &r));
                }
            }
            start += i + 1;
        }
        partial.drain(..start);
    }
    exit_on_broken_pipe(printer.flush());
    Ok(got)
}

/// Polls the active file, printing new complete lines and switching files on rotation.
fn follow(active: &Path, pos: &mut u64, filter: &Filter, printer: &mut Printer) -> Res<i32> {
    use std::io::{Seek, SeekFrom};
    let mut file: Option<fs::File> = None;
    let mut id = None;
    let mut partial: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        if file.is_none()
            && let Ok(mut f) = fs::File::open(active)
        {
            f.seek(SeekFrom::Start(*pos)).map_err(|e| e.to_string())?;
            id = file_id(active);
            file = Some(f);
        }
        let got = match file.as_mut() {
            Some(f) => drain(f, pos, &mut partial, &mut buf, filter, printer)?,
            None => false,
        };
        if got {
            continue;
        }
        // Rotated (new inode) or truncated: the writer no longer appends to the
        // old file, so read what is left in it, then switch.
        let rotated = file.is_some() && file_id(active) != id;
        let truncated = fs::metadata(active).map(|m| m.len() < *pos).unwrap_or(false);
        if rotated || truncated {
            if let Some(f) = file.as_mut()
                && rotated
            {
                drain(f, pos, &mut partial, &mut buf, filter, printer)?;
            }
            file = None;
            *pos = 0;
            partial.clear();
            continue;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

fn query(cfg: &Config, a: QueryArgs, color: bool) -> Res<i32> {
    let filter = build_filter(&a.filter, cfg.timezone)?;
    let paths = Paths::new(cfg);
    let mut printer = Printer::new(mode(a.output), color, cfg.timezone);
    let limit = a.limit.unwrap_or(usize::MAX);
    let mut n = 0usize;
    for f in read::stream_files(&paths, filter.since, filter.until) {
        let r = read::for_each_line(&f, &mut |line| {
            if !filter.line_may_match(line) {
                return true;
            }
            let rec = Record::parse(line);
            if !filter.matches(&rec) {
                return true;
            }
            n += 1;
            if !a.count {
                exit_on_broken_pipe(printer.print(line, &rec));
            }
            n < limit
        });
        match r {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => eprintln!("rotlog: {}: {e}", f.display()),
        }
    }
    if a.count {
        exit_on_broken_pipe(writeln!(printer.writer(), "{n}"));
    }
    exit_on_broken_pipe(printer.flush());
    Ok(if n > 0 { 0 } else { 1 })
}

fn stats(cfg: &Config, a: StatsArgs, color: bool) -> Res<i32> {
    let filter = build_filter(&a.filter, cfg.timezone)?;
    let paths = Paths::new(cfg);
    let mut agg = cli::stats::Agg::default();
    for f in read::stream_files(&paths, filter.since, filter.until) {
        let r = read::for_each_line(&f, &mut |line| {
            if filter.line_may_match(line) {
                let rec = Record::parse(line);
                if filter.matches(&rec) {
                    agg.add(&rec);
                }
            }
            true
        });
        if let Err(e) = r {
            eprintln!("rotlog: {}: {e}", f.display());
        }
    }
    let mut out = std::io::stdout().lock();
    if a.json {
        exit_on_broken_pipe(writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&agg.into_json(a.top, cfg.timezone)).unwrap_or_default()
        ));
    } else {
        exit_on_broken_pipe(agg.print(a.top, cfg.timezone, color, &mut out));
    }
    Ok(0)
}

// ---- files & control -----------------------------------------------------

#[derive(serde::Serialize)]
struct FileRow {
    file: String,
    bytes: u64,
    state: String,
    start: Option<String>,
}

fn stream_rows(cfg: &Config) -> Vec<FileRow> {
    let paths = Paths::new(cfg);
    let uploaded = maintain::read_manifest(&paths);
    let mut rows = Vec::new();
    for a in paths.archives() {
        let mut state = vec![if a.compressed { "gzip" } else { "archive" }];
        if uploaded.contains(&a.file_name) {
            state.push("uploaded");
        }
        rows.push(FileRow {
            bytes: fs::metadata(&a.path).map(|m| m.len()).unwrap_or(0),
            state: state.join(","),
            start: a.start_micros(cfg.timezone).map(|t| print::short_ts(t, cfg.timezone)[..19].to_string()),
            file: a.file_name,
        });
    }
    let active = paths.active();
    if let Ok(m) = fs::metadata(&active) {
        rows.push(FileRow {
            file: active.file_name().unwrap_or_default().to_string_lossy().into_owned(),
            bytes: m.len(),
            state: "active".into(),
            start: None,
        });
    }
    rows
}

fn ls(cfg: &Config, a: LsArgs) -> Res<i32> {
    let names = if a.all { Paths::discover(&cfg.dir) } else { vec![cfg.name.clone()] };
    let mut all = serde_json::Map::new();
    for name in names {
        let c = Config { name: name.clone(), ..cfg.clone() };
        let rows = stream_rows(&c);
        if a.json {
            all.insert(name, serde_json::to_value(&rows).unwrap_or_default());
            continue;
        }
        if rows.is_empty() {
            println!("no files for stream '{name}' in {}", cfg.dir.display());
            continue;
        }
        let w = rows.iter().map(|r| r.file.len()).max().unwrap_or(4).max(4);
        println!("{:<w$}  {:>9}  {:<16}  START", "FILE", "SIZE", "STATE");
        let mut total = 0;
        for r in &rows {
            total += r.bytes;
            println!(
                "{:<w$}  {:>9}  {:<16}  {}",
                r.file,
                human_size(r.bytes),
                r.state,
                r.start.as_deref().unwrap_or("-")
            );
        }
        println!("{} files, {}", rows.len(), human_size(total));
    }
    if a.json {
        println!("{}", serde_json::to_string_pretty(&all).unwrap_or_default());
    }
    Ok(0)
}

fn status(cfg: &Config, json: bool) -> Res<i32> {
    let paths = Paths::new(cfg);
    let owner = control::owner(&paths);
    let live = owner.and_then(|_| control::send(&paths, "status", Duration::from_secs(3)).ok());
    let rows = stream_rows(cfg);
    if json {
        let v = serde_json::json!({
            "stream": cfg.name,
            "dir": cfg.dir,
            "writer_pid": owner,
            "writer": live.as_deref().and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()),
            "files": rows,
        });
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        return Ok(0);
    }
    println!("stream   {} ({})", cfg.name, paths.active().display());
    match (owner, &live) {
        (Some(pid), Some(s)) => {
            let v: serde_json::Value = serde_json::from_str(s).unwrap_or_default();
            println!("writer   pid {pid}, level {}, up {}s", v["level"].as_str().unwrap_or("?"), v["uptime_secs"]);
            println!(
                "records  {} written ({}), {} dropped, {} pending bytes",
                thousands(v["records"].as_u64().unwrap_or(0)),
                human_size(v["bytes"].as_u64().unwrap_or(0)),
                v["dropped"],
                v["pending_bytes"]
            );
            println!(
                "archive  {} rotations, {} compressed, {} uploaded ({} failed), {} deleted",
                v["rotations"], v["compressed"], v["uploaded"], v["upload_errors"], v["deleted"]
            );
            if let Some(e) = v["last_error"].as_str() {
                println!("error    {e}");
            }
        }
        (Some(pid), None) => println!("writer   pid {pid} (not answering)"),
        (None, _) => println!("writer   none running"),
    }
    let archives = rows.iter().filter(|r| r.state != "active").count();
    let up = rows.iter().filter(|r| r.state.contains("uploaded")).count();
    let total: u64 = rows.iter().map(|r| r.bytes).sum();
    println!("files    {archives} archives ({up} uploaded), {} on disk", human_size(total));
    // Prefer what the running writer actually uses over this command's config.
    let live_policy = live
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|v| v["policy"].as_str().map(str::to_string));
    match live_policy {
        Some(p) => println!("policy   {p}"),
        None => println!("policy   {} (from config)", cfg.policy_summary()),
    }
    Ok(0)
}

fn ctl(cfg: &Config, cmd: &str) -> Res<i32> {
    let reply = control::send(&Paths::new(cfg), cmd, Duration::from_secs(10))?;
    println!("{reply}");
    Ok(0)
}

fn rotate(mut cfg: Config) -> Res<i32> {
    let paths = Paths::new(&cfg);
    if control::owner(&paths).is_some() {
        return ctl(&cfg, "rotate");
    }
    cfg.maintenance_interval = Dur(Duration::ZERO);
    let log = open(cfg)?;
    let rotated = log.rotate().map_err(|e| e.to_string())?;
    log.close();
    println!("{}", if rotated { "rotated" } else { "nothing to rotate (active file is empty)" });
    Ok(0)
}

// ---- maintenance ---------------------------------------------------------

fn uploader_for(cfg: &Config) -> Res<Option<Box<dyn upload::Uploader>>> {
    if cfg.upload.active() { upload::from_config(&cfg.upload).map(Some) } else { Ok(None) }
}

fn report_event(ev: Event) {
    match ev {
        Event::Compressed { file, before, after } => {
            println!("compressed {file} ({} -> {})", human_size(before), human_size(after))
        }
        Event::Uploaded { file, target, bytes, existed } => {
            println!(
                "uploaded   {file} -> {target} ({}{})",
                human_size(bytes),
                if existed { ", already present" } else { "" }
            )
        }
        Event::UploadFailed { file, error } => eprintln!("failed     {file}: {error}"),
        Event::Deleted { file, reason } => println!("deleted    {file} ({reason})"),
        Event::Error { msg } => eprintln!("error      {msg}"),
    }
}

fn run_maint(cfg: &Config, m: MaintArgs, (compress, upload, prune): (bool, bool, bool)) -> Res<i32> {
    let up = if upload { uploader_for(cfg)? } else { None };
    let opts = maintain::Options { compress, upload, prune, force: m.now, dry_run: m.dry_run, wait_lock: true };
    let r = maintain::run(cfg, up.as_deref(), &opts, &mut report_event);
    for p in &r.planned {
        println!("would {p}");
    }
    if !m.dry_run && r.compressed + r.uploaded + r.deleted + r.errors == 0 {
        println!("nothing to do");
    }
    Ok(if r.errors > 0 { 1 } else { 0 })
}

fn maintain_cmd(cfg: &Config, a: MaintainArgs) -> Res<i32> {
    if !a.watch {
        return run_maint(cfg, a.maint, (true, true, true));
    }
    let interval = match &a.interval {
        Some(i) => parse_duration(i)?,
        None => cfg.maintenance_interval.0.max(Duration::from_secs(1)),
    };
    let up = uploader_for(cfg)?;
    let opts =
        maintain::Options { force: a.maint.now, dry_run: a.maint.dry_run, wait_lock: true, ..Default::default() };
    eprintln!("rotlog: maintaining '{}' in {} every {}", cfg.name, cfg.dir.display(), Dur(interval));
    loop {
        maintain::run(cfg, up.as_deref(), &opts, &mut report_event);
        std::thread::sleep(interval);
    }
}

fn check_upload(cfg: &Config) -> Res<i32> {
    let up = uploader_for(cfg)?.ok_or("no bucket configured (set [upload] bucket or --bucket)")?;
    println!("{}", up.check()?);
    Ok(0)
}

// ---- bench -----------------------------------------------------------------

fn bench(mut cfg: Config, a: BenchArgs, dir_given: bool) -> Res<i32> {
    let tmp = std::env::temp_dir().join(format!("rotlog-bench-{}", std::process::id()));
    if !dir_given {
        cfg.dir = tmp.clone();
    }
    cfg.name = "bench".into();
    cfg.console = Console::Off;
    cfg.level = Level::Info;
    cfg.upload.bucket.clear();
    cfg.maintenance_interval = Dur(Duration::ZERO);
    let _ = fs::remove_dir_all(cfg.dir.join("bench.log"));
    let msg = "x".repeat(a.msg_size);
    let log = open(cfg.clone())?;
    let total = a.records * a.threads as u64;
    let (tx, rx) = mpsc::channel::<Vec<u64>>();
    let start = Instant::now();
    std::thread::scope(|s| {
        for t in 0..a.threads {
            let log = log.clone();
            let msg = msg.as_str();
            let tx = tx.clone();
            s.spawn(move || {
                // Sample every 64th call for latency percentiles.
                let mut samples = Vec::with_capacity((a.records / 64) as usize + 1);
                for i in 0..a.records {
                    let t0 = if i % 64 == 0 { Some(Instant::now()) } else { None };
                    if a.with_fields {
                        log.info(
                            msg,
                            &[
                                ("method", "GET".into()),
                                ("path", "/api/users/42".into()),
                                ("status", 200.into()),
                                ("thread", t.into()),
                            ],
                        );
                    } else {
                        log.info(msg, &[]);
                    }
                    if let Some(t0) = t0 {
                        samples.push(t0.elapsed().as_nanos() as u64);
                    }
                }
                let _ = tx.send(samples);
            });
        }
    });
    drop(tx);
    let logged = start.elapsed();
    log.flush().map_err(|e| e.to_string())?;
    let flushed = start.elapsed();
    let bytes = log.stats().bytes;
    log.close();
    let mut lat: Vec<u64> = rx.iter().flatten().collect();
    lat.sort_unstable();
    let p = |q: f64| lat.get(((lat.len() as f64 - 1.0) * q) as usize).copied().unwrap_or(0);
    println!(
        "records      {} ({} threads, {} B messages{})",
        thousands(total),
        a.threads,
        a.msg_size,
        if a.with_fields { ", 4 fields" } else { "" }
    );
    println!("format       {:?}", cfg.format);
    println!("call rate    {:.2} M records/s", total as f64 / logged.as_secs_f64() / 1e6);
    println!(
        "on disk      {:.2} M records/s, {:.0} MB/s ({} written)",
        total as f64 / flushed.as_secs_f64() / 1e6,
        bytes as f64 / flushed.as_secs_f64() / 1e6,
        human_size(bytes)
    );
    println!(
        "latency      p50 {} ns  p99 {} ns  p99.9 {} ns  max {} ns",
        p(0.5),
        p(0.99),
        p(0.999),
        lat.last().copied().unwrap_or(0)
    );
    if !a.keep_files {
        let paths = Paths::new(&cfg);
        let _ = fs::remove_file(paths.active());
        for ar in paths.archives() {
            let _ = fs::remove_file(ar.path);
        }
        for p in [paths.lock(), paths.maint_lock()] {
            let _ = fs::remove_file(p);
        }
        if !dir_given {
            let _ = fs::remove_dir_all(&tmp);
        }
    }
    Ok(0)
}
