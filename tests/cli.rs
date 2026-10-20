use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

fn tmpdir(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "rotlog-cli-{}-{tag}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn rotlog(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rotlog"))
        .current_dir(dir)
        .args(["--dir", "logs", "--name", "app", "--color", "never"])
        .args(args)
        .env_remove("ROTLOG_CONFIG")
        .env_remove("ROTLOG_DIR")
        .env_remove("ROTLOG_NAME")
        .env_remove("ROTLOG_BUCKET")
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    assert!(
        o.status.success(),
        "failed: {}\n{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn pipe_in(dir: &Path, args: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rotlog"))
        .current_dir(dir)
        .args(["--dir", "logs", "--name", "app"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn write_then_query_with_filters() {
    let d = tmpdir("query");
    stdout(&rotlog(
        &d,
        &["write", "-l", "error", "db", "timeout", "-f", "user=alice", "-f", "status=503", "-f", "dur_ms=812.5"],
    ));
    stdout(&rotlog(&d, &["write", "login", "ok", "-f", "user=bob", "-f", "status=200"]));
    stdout(&rotlog(&d, &["write", "-l", "warn", "--json-fields", r#"{"user":"alice","status":404}"#, "not found"]));

    assert_eq!(stdout(&rotlog(&d, &["query", "--count"])).trim(), "3");
    assert_eq!(stdout(&rotlog(&d, &["query", "--count", "-l", "warn"])).trim(), "2");
    assert_eq!(stdout(&rotlog(&d, &["query", "--count", "--user", "alice"])).trim(), "2");
    assert_eq!(stdout(&rotlog(&d, &["query", "--count", "--status", "5xx"])).trim(), "1");
    assert_eq!(stdout(&rotlog(&d, &["query", "--count", "-w", "dur_ms>800", "-g", "TIMEOUT"])).trim(), "1");
    assert_eq!(stdout(&rotlog(&d, &["query", "--count", "--since", "1h"])).trim(), "3");
    let none = rotlog(&d, &["query", "--count", "--until", "2000-01-01"]);
    assert_eq!((none.status.code(), String::from_utf8_lossy(&none.stdout).trim()), (Some(1), "0"));

    let json = stdout(&rotlog(&d, &["query", "--json", "--status", "503"]));
    let v: serde_json::Value = serde_json::from_str(json.trim()).unwrap();
    assert_eq!(v["msg"], "db timeout");
    assert_eq!(v["level"], "error");
    assert_eq!(v["status"], 503);
    assert_eq!(v["dur_ms"], 812.5);

    // No match: exit code 1, like grep.
    assert_eq!(rotlog(&d, &["query", "--user", "nobody"]).status.code(), Some(1));
    fs::remove_dir_all(&d).unwrap();
}

#[test]
fn pipe_detects_levels_and_stats_summarize() {
    let d = tmpdir("pipe");
    let input = "INFO boot\n[warn] slow disk\nERROR crashed\n\
        {\"level\":\"info\",\"msg\":\"req\",\"method\":\"GET\",\"path\":\"/u/1\",\"status\":200,\"dur_ms\":10,\"user\":\"al\"}\n\
        {\"level\":\"error\",\"msg\":\"req\",\"method\":\"GET\",\"path\":\"/u/2\",\"status\":500,\"dur_ms\":30,\"user\":\"al\"}\n\
        no level here\n";
    let o = pipe_in(&d, &["pipe"], input);
    assert!(o.status.success());
    let s = stdout(&rotlog(&d, &["stats", "--json"]));
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(v["records"], 6);
    assert_eq!(v["levels"]["info"], 3);
    assert_eq!(v["levels"]["warn"], 1);
    assert_eq!(v["levels"]["error"], 2);
    assert_eq!(v["requests"]["total"], 2);
    assert_eq!(v["requests"]["5xx"], 1);
    assert_eq!(v["routes"][0]["route"], "GET /u/:id");
    assert_eq!(v["routes"][0]["count"], 2);
    assert_eq!(v["users"][0]["user"], "al");
    assert_eq!(v["users"][0]["errors"], 1);
    fs::remove_dir_all(&d).unwrap();
}

#[test]
fn rotate_compress_list_and_read_back() {
    let d = tmpdir("rotate");
    stdout(&rotlog(&d, &["write", "first"]));
    assert_eq!(stdout(&rotlog(&d, &["--compress-after", "1d", "rotate"])).trim(), "rotated");
    assert!(stdout(&rotlog(&d, &["rotate"])).contains("nothing to rotate"));
    stdout(&rotlog(&d, &["write", "second"]));

    assert!(stdout(&rotlog(&d, &["compress", "--dry-run"])).contains("would compress app."));
    assert!(stdout(&rotlog(&d, &["compress"])).contains("compressed app."));
    let ls: serde_json::Value = serde_json::from_str(&stdout(&rotlog(&d, &["ls", "--json"]))).unwrap();
    let files = ls["app"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0]["state"], "gzip");
    assert_eq!(files[1]["state"], "active");

    let out = stdout(&rotlog(&d, &["query", "--raw"]));
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].ends_with("first") && lines[1].ends_with("second"));
    fs::remove_dir_all(&d).unwrap();
}

#[test]
fn prune_respects_keep() {
    let d = tmpdir("prune");
    for i in 0..4 {
        stdout(&rotlog(&d, &["write", &format!("n{i}")]));
        stdout(&rotlog(&d, &["--no-compress", "rotate"]));
    }
    let plan = stdout(&rotlog(&d, &["--keep", "1", "prune", "--dry-run"]));
    assert_eq!(plan.lines().filter(|l| l.starts_with("would delete")).count(), 3);
    stdout(&rotlog(&d, &["--keep", "1", "prune"]));
    assert_eq!(stdout(&rotlog(&d, &["query", "--raw"])).lines().count(), 1);
    fs::remove_dir_all(&d).unwrap();
}

#[test]
fn run_logs_both_streams_and_propagates_exit_code() {
    let d = tmpdir("run");
    let o = rotlog(&d, &["run", "--", "sh", "-c", "echo out line; echo WARN careful >&2; echo boom >&2; exit 3"]);
    assert_eq!(o.status.code(), Some(3));
    let json = stdout(&rotlog(&d, &["query", "--json"]));
    let recs: Vec<serde_json::Value> = json.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let find = |m: &str| recs.iter().find(|r| r["msg"] == m).unwrap_or_else(|| panic!("missing {m}: {json}")).clone();
    assert_eq!(find("out line")["level"], "info");
    assert_eq!(find("WARN careful")["level"], "warn");
    assert_eq!(find("boom")["level"], "error");
    let exit = find("process exited");
    assert_eq!(exit["code"], 3);
    assert_eq!(exit["level"], "error");
    assert_eq!(exit["logger"], "rotlog");
    fs::remove_dir_all(&d).unwrap();
}

#[test]
fn config_file_and_flag_precedence() {
    let d = tmpdir("config");
    stdout(&rotlog(&d, &["init"]));
    let toml = fs::read_to_string(d.join("rotlog.toml")).unwrap().replace("level = \"info\"", "level = \"warn\"");
    fs::write(d.join("rotlog.toml"), toml).unwrap();
    let cfg = stdout(&rotlog(&d, &["--every", "6h", "--bucket", "b1", "config"]));
    assert!(cfg.contains("level = \"warn\""), "{cfg}");
    assert!(cfg.contains("every = \"6h\""), "{cfg}");
    assert!(cfg.contains("bucket = \"b1\""), "{cfg}");
    assert!(!rotlog(&d, &["init"]).status.success(), "init must not overwrite");
    let bad = rotlog(&d, &["--every", "sometimes", "config"]);
    assert!(String::from_utf8_lossy(&bad.stderr).contains("invalid"));
    let no_bucket = rotlog(&d, &["--no-upload", "upload"]);
    assert!(String::from_utf8_lossy(&no_bucket.stderr).contains("no bucket configured"));
    fs::remove_dir_all(&d).unwrap();
}
