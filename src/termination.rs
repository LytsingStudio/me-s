use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::Result;

pub struct TerminationSignals {
    requested: Arc<AtomicBool>,
    registrations: Vec<signal_hook::SigId>,
}

impl TerminationSignals {
    pub fn install() -> Result<Self> {
        let requested = Arc::new(AtomicBool::new(false));
        let mut signals = signal_hook::consts::TERM_SIGNALS.to_vec();
        #[cfg(unix)]
        signals.push(signal_hook::consts::SIGHUP);
        signals.sort_unstable();
        signals.dedup();
        let mut registrations = Vec::with_capacity(signals.len());
        for signal in signals {
            match signal_hook::flag::register(signal, Arc::clone(&requested)) {
                Ok(registration) => registrations.push(registration),
                Err(error) => {
                    for registration in registrations {
                        signal_hook::low_level::unregister(registration);
                    }
                    return Err(error.into());
                }
            }
        }
        Ok(Self {
            requested,
            registrations,
        })
    }

    pub fn flag(&self) -> &AtomicBool {
        &self.requested
    }

    pub fn requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }
}

impl Drop for TerminationSignals {
    fn drop(&mut self) {
        for registration in self.registrations.drain(..) {
            signal_hook::low_level::unregister(registration);
        }
    }
}
