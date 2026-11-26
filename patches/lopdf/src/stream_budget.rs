//! Cumulative accounting for the single-threaded reader. A parser worker
//! explicitly inherits its caller's budget, including for nested validations.
use crate::{DecompressError, Error, Result};
use std::cell::RefCell;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct StreamBudget {
    per_stream: usize,
    shared: Arc<Mutex<Balance>>,
}

struct Balance {
    remaining: usize,
    exceeded: Option<usize>,
}

thread_local! {
    static ACTIVE: RefCell<Option<StreamBudget>> = const { RefCell::new(None) };
}

struct Restore(Option<StreamBudget>);

impl Drop for Restore {
    fn drop(&mut self) {
        ACTIVE.set(self.0.take());
    }
}

impl StreamBudget {
    pub fn new(per_stream: usize, total: usize) -> Self {
        Self {
            per_stream,
            shared: Arc::new(Mutex::new(Balance {
                remaining: total,
                exceeded: None,
            })),
        }
    }

    pub fn current() -> Option<Self> {
        ACTIVE.with_borrow(Clone::clone)
    }

    /// Rayon would decode outside the installed thread scope.
    #[cfg(not(feature = "rayon"))]
    pub fn run<T>(&self, f: impl FnOnce() -> T) -> Result<T> {
        self.check()?;
        let _restore = Restore(ACTIVE.replace(Some(self.clone())));
        let result = f();
        // Lenient recovery and font lookup can otherwise swallow decoder errors.
        self.check()?;
        Ok(result)
    }

    pub fn check(&self) -> Result<()> {
        match self.shared.lock().unwrap().exceeded {
            Some(limit) => Err(DecompressError::MemoryLimitExceeded { limit }.into()),
            None => Ok(()),
        }
    }
}

/// Run one filter stage while holding the document balance. No other worker
/// can spend the same bytes. Unfiltered content copies take the same path.
pub(crate) fn decode(limit: Option<usize>, f: impl FnOnce(Option<usize>) -> Result<Vec<u8>>) -> Result<Vec<u8>> {
    let Some(budget) = StreamBudget::current() else {
        return f(limit);
    };
    let mut balance = budget.shared.lock().unwrap();
    if let Some(limit) = balance.exceeded {
        return Err(DecompressError::MemoryLimitExceeded { limit }.into());
    }
    let cap = limit
        .unwrap_or(usize::MAX)
        .min(budget.per_stream)
        .min(balance.remaining);
    let result = f(Some(cap));
    match &result {
        Ok(output) => balance.remaining -= output.len(),
        Err(Error::Decompress(DecompressError::MemoryLimitExceeded { limit })) => {
            balance.exceeded = Some(*limit);
        }
        // A failed filter may have produced partial output before failing.
        Err(_) => balance.remaining -= cap,
    }
    result
}
