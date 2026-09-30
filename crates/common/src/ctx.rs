//! What a verb reads from its environment: the state directory, the text cache budget, and
//! the page pool tuning.

use std::path::{Path, PathBuf};

use crate::error::GoatError;
use crate::ledger::LEDGER_FILE;
use crate::pool::PoolTuning;
use crate::textcache::{CACHE_FILE, DEFAULT_CAP_BYTES, cache_limit_bytes};

/// A verb's view of its environment.
#[derive(Clone, Debug)]
pub struct Ctx {
    home: PathBuf,
    cache_cap_bytes: u64,
    pool: Result<PoolTuning, GoatError>,
}

impl Ctx {
    /// `PDF_GOAT_HOME` (default `~/.pdf-goat`, taken as written), `PDF_GOAT_CACHE_MB`, and
    /// `PDF_GOAT_WORKERS`.
    pub fn from_env() -> Self {
        let home = match std::env::var_os("PDF_GOAT_HOME") {
            Some(home) => PathBuf::from(home),
            None => std::env::home_dir().unwrap_or_default().join(".pdf-goat"),
        };
        let setting =
            |name| std::env::var_os(name).map(|value| value.to_string_lossy().into_owned());
        Self {
            home,
            cache_cap_bytes: cache_limit_bytes(setting("PDF_GOAT_CACHE_MB").as_deref()),
            pool: PoolTuning::from_setting(setting("PDF_GOAT_WORKERS").as_deref()),
        }
    }

    /// A context on `home` with the default cache budget and pool tuning.
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            cache_cap_bytes: DEFAULT_CAP_BYTES,
            pool: PoolTuning::from_setting(None),
        }
    }

    /// The same context with a cache budget of `bytes`; zero disables the cache.
    pub fn with_cache_cap(self, bytes: u64) -> Self {
        Self {
            cache_cap_bytes: bytes,
            ..self
        }
    }

    /// The same context with this pool tuning.
    pub fn with_pool(self, tuning: PoolTuning) -> Self {
        Self {
            pool: Ok(tuning),
            ..self
        }
    }

    /// The state directory that holds the ledger and the text cache.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The ledger database.
    pub fn ledger_path(&self) -> PathBuf {
        self.home.join(LEDGER_FILE)
    }

    /// The text cache database.
    pub fn cache_path(&self) -> PathBuf {
        self.home.join(CACHE_FILE)
    }

    /// The text cache budget in bytes.
    pub fn cache_cap_bytes(&self) -> u64 {
        self.cache_cap_bytes
    }

    /// The page pool tuning, or the `ValueError` an unreadable `PDF_GOAT_WORKERS` raises.
    pub fn pool(&self) -> Result<PoolTuning, GoatError> {
        self.pool.clone()
    }
}
