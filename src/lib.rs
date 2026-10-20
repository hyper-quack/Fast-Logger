//! # rotlog
//!
//! A small, fast rotating logger:
//!
//! - time (aligned to the wall clock) and size based rotation,
//! - gzip compression of archives after a delay,
//! - upload of archives to Google Cloud Storage, retention,
//! - text (logfmt) or JSON lines, microsecond timestamps,
//! - request logging helpers,
//! - a CLI (`rotlog`) and a C ABI used by the Bun/TypeScript binding.
//!
//! ```no_run
//! use rotlog::{Config, Logger, Level};
//!
//! let log = Logger::open(Config { name: "api".into(), ..Config::default() })?;
//! log.info("server started", &[("port", 8080.into())]);
//! let db = log.named("db").with(&[("pool", "primary".into())]);
//! db.warn("slow query", &[("ms", 812.into())]);
//! log.flush()?;
//! # Ok::<(), std::io::Error>(())
//! ```

pub mod compress;
pub mod config;
pub mod control;
pub mod ffi;
pub mod format;
#[cfg(feature = "gcs")]
pub mod gcs;
pub mod level;
pub mod logger;
pub mod maintain;
pub mod read;
pub mod rotation;
pub mod sys;
pub mod time;
pub mod upload;

pub use config::Config;
pub use format::Value;
pub use level::Level;
pub use logger::{Logger, Request, Stats};
