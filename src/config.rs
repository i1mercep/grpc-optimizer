//! Server resource limits and validated environment configuration.

use std::{net::SocketAddr, str::FromStr};

/// Process-wide limits shared by RPC admission, model validation, and native solves.
///
/// Call [`Self::validate`] after constructing or modifying configuration directly.
#[derive(Clone, Debug)]
pub struct Config {
    pub listen_addr: SocketAddr,
    pub cpu_budget: usize,
    pub solver_threads: usize,
    pub max_concurrent_solves: usize,
    pub max_active_rpcs: usize,
    pub max_message_bytes: usize,
    pub stream_window: usize,
    pub default_time_limit_seconds: f64,
    pub max_time_limit_seconds: f64,
    pub max_variables: usize,
    pub max_constraints: usize,
    pub max_nonzeros: usize,
}

impl Default for Config {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map_or(1, usize::from);
        Self {
            listen_addr: "127.0.0.1:50051".parse().expect("valid default address"),
            cpu_budget: cpus,
            solver_threads: 1,
            max_concurrent_solves: cpus,
            max_active_rpcs: 32,
            max_message_bytes: 16 * 1024 * 1024,
            stream_window: 8,
            default_time_limit_seconds: 30.0,
            max_time_limit_seconds: 300.0,
            max_variables: 100_000,
            max_constraints: 100_000,
            max_nonzeros: 1_000_000,
        }
    }
}

impl Config {
    /// Loads `OPT_*` overrides and validates the resulting limits.
    ///
    /// Unless overridden, solve concurrency is the CPU budget divided by solver threads.
    /// Returns an error for malformed values or inconsistent resource limits.
    pub fn from_env() -> anyhow::Result<Self> {
        let mut c = Self::default();
        macro_rules! read {
            ($field:ident, $name:literal) => {
                c.$field = env_value($name, c.$field)?;
            };
        }
        read!(listen_addr, "OPT_LISTEN_ADDR");
        read!(cpu_budget, "OPT_CPU_BUDGET");
        read!(solver_threads, "OPT_SOLVER_THREADS");
        c.max_concurrent_solves = c.cpu_budget / c.solver_threads.max(1);
        read!(max_concurrent_solves, "OPT_MAX_CONCURRENT_SOLVES");
        read!(max_active_rpcs, "OPT_MAX_ACTIVE_RPCS");
        read!(max_message_bytes, "OPT_MAX_MESSAGE_BYTES");
        read!(stream_window, "OPT_STREAM_WINDOW");
        read!(default_time_limit_seconds, "OPT_DEFAULT_TIME_LIMIT_SECONDS");
        read!(max_time_limit_seconds, "OPT_MAX_TIME_LIMIT_SECONDS");
        read!(max_variables, "OPT_MAX_VARIABLES");
        read!(max_constraints, "OPT_MAX_CONSTRAINTS");
        read!(max_nonzeros, "OPT_MAX_NONZEROS");
        c.validate()?;
        Ok(c)
    }

    /// Rejects nonpositive or oversized limits, CPU oversubscription, and invalid time budgets.
    pub fn validate(&self) -> anyhow::Result<()> {
        for (name, value) in [
            ("CPU budget", self.cpu_budget),
            ("solver threads", self.solver_threads),
            ("concurrent solves", self.max_concurrent_solves),
            ("active RPCs", self.max_active_rpcs),
            ("message bytes", self.max_message_bytes),
            ("stream window", self.stream_window),
            ("variables", self.max_variables),
            ("constraints", self.max_constraints),
            ("nonzeros", self.max_nonzeros),
        ] {
            anyhow::ensure!(value > 0 && value <= i32::MAX as usize, "invalid {name}");
        }
        anyhow::ensure!(
            self.max_concurrent_solves
                .checked_mul(self.solver_threads)
                .is_some_and(|n| n <= self.cpu_budget),
            "concurrent solves * solver threads must fit CPU budget"
        );
        anyhow::ensure!(
            self.default_time_limit_seconds.is_finite()
                && self.max_time_limit_seconds.is_finite()
                && self.default_time_limit_seconds > 0.0
                && self.default_time_limit_seconds <= self.max_time_limit_seconds
                && self.max_time_limit_seconds <= 300.0,
            "time limits must satisfy 0 < default <= maximum <= 300 seconds"
        );
        Ok(())
    }
}

fn env_value<T: FromStr>(name: &str, default: T) -> anyhow::Result<T> {
    match std::env::var(name) {
        Ok(value) => value.parse().map_err(|_| anyhow::anyhow!("invalid {name}")),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(anyhow::anyhow!("invalid {name}: {error}")),
    }
}
