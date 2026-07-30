use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

static VERBOSE: AtomicBool = AtomicBool::new(false);

pub const VERBOSE_ENV: &str = "RULES_OCI_RUNTIME_VERBOSE";

/// Container output is the only thing on stderr unless logging is opted into.
pub fn init(flag: bool) {
    let from_env = std::env::var_os(VERBOSE_ENV).is_some_and(|v| !v.is_empty() && v != "0");
    VERBOSE.store(flag || from_env, Ordering::Relaxed);
}

pub fn is_verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

pub fn emit(message: impl AsRef<str>) {
    if is_verbose() {
        let _ = writeln!(std::io::stderr(), "{}", message.as_ref());
    }
}

pub fn warn(message: impl AsRef<str>) {
    let _ = writeln!(std::io::stderr(), "warning: {}", message.as_ref());
}

macro_rules! log {
    ($($arg:tt)*) => {
        if $crate::log::is_verbose() {
            $crate::log::emit(format!($($arg)*));
        }
    };
}

macro_rules! warning {
    ($($arg:tt)*) => {
        $crate::log::warn(format!($($arg)*));
    };
}

pub(crate) use log;
pub(crate) use warning;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbosity_can_be_forced_on() {
        init(true);
        assert!(is_verbose());
        init(false);
    }
}
