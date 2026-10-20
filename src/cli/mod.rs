pub mod ingest;
pub mod print;
pub mod stats;

pub const INIT_TEMPLATE: &str = r#"# rotlog configuration. Every key is optional; values shown are the defaults.
# Command line flags (rotlog --help) override this file.

dir = "logs"              # directory for the active file and archives
name = "app"              # active file: <dir>/<name>.<ext>
ext = "log"
level = "info"            # trace | debug | info | warn | error | fatal | off
format = "text"           # text (logfmt fields) | json (one object per line)
timezone = "local"        # local | utc  (timestamps, rotation boundaries, archive names)
console = "off"           # off | stdout | stderr: also echo lines to the console
flush_interval = "100ms"  # max time a record waits in memory ("0" = write immediately)
buffer_size = "256KB"     # pending bytes that trigger an early write
max_buffer = "64MB"       # pending bytes at which callers wait (or drop, see overflow)
overflow = "block"        # block (never lose records) | drop (count and skip)
fsync = "rotate"          # never | rotate (before archiving) | always (every write)
flush_on_error = true     # write error/fatal records immediately
maintenance_interval = "1m"  # background compress/upload/retention ("0" = disabled)

[rotation]
every = "daily"           # never | minutely | hourly | daily | weekly[:sun] | 15m | 6h | 2d ...
max_size = "0"            # also rotate past this size, e.g. "100MB" ("0" = off)
keep = 30                 # archives kept locally ("0" = all)
max_age = "0"             # delete local archives older than this, e.g. "30d"

[compress]
enabled = true
after = "0s"              # gzip archives this long after rotation
level = 6                 # 1 (fast) .. 9 (small)

[upload]
# Uploading is active when a bucket is set. Archives are uploaded once compressed,
# never overwritten (ifGenerationMatch=0), and never deleted locally before upload.
provider = "gcs"
bucket = ""
prefix = ""               # e.g. "prod/"
object = "{prefix}{host}/{name}/{yyyy}/{mm}/{dd}/{file}"
after = "0s"
delete_local = false      # remove the local archive after a successful upload
# credentials = "/etc/rotlog/service-account.json"   # default: GOOGLE_APPLICATION_CREDENTIALS,
#                                                    # gcloud ADC, then the GCP metadata server
endpoint = "https://storage.googleapis.com"
timeout = "2m"
"#;
