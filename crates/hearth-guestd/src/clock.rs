//! CLOCK_REALTIME correction after a snapshot restore.
//!
//! CHV ≥ 53 advances the guest's monotonic clock across the stopped window
//! but leaves CLOCK_REALTIME lagging by exactly that window (~52s measured
//! live on CHV 53.0). systemd-timesyncd fixes that
//! eventually, but its next poll can be half an hour out — so the restored
//! ack carries the host's wall clock and guestd steps immediately. The step
//! itself wakes timesyncd (its poll timer is armed with
//! TFD_TIMER_CANCEL_ON_SET), which then re-disciplines against real NTP.

use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

/// Offsets below this are left to NTP slewing: at this size the "offset" is
/// ack latency plus ordinary drift, not a restore gap, and setting the clock
/// would be churn for nothing.
const MIN_STEP_MS: u64 = 1000;

/// Step CLOCK_REALTIME to the host's clock if they differ by at least
/// [`MIN_STEP_MS`]. Requires CAP_SYS_TIME (guestd runs as root).
pub fn step_to_host_time(host_time_ms: u64) {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let Some(delta_ms) = step_delta_ms(host_time_ms, now_ms) else {
        return;
    };
    // `as _` so the field types drive the casts: naming `libc::time_t` is
    // deprecated on musl (64-bit since musl 1.2).
    let ts = libc::timespec {
        tv_sec: (host_time_ms / 1000) as _,
        tv_nsec: ((host_time_ms % 1000) * 1_000_000) as _,
    };
    if unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) } == 0 {
        info!(delta_ms, "stepped CLOCK_REALTIME to the host clock");
    } else {
        warn!(
            delta_ms,
            error = %std::io::Error::last_os_error(),
            "failed to step CLOCK_REALTIME"
        );
    }
}

/// The signed correction (host − guest) worth applying, or `None` when the
/// clocks already agree to within [`MIN_STEP_MS`].
fn step_delta_ms(host_time_ms: u64, now_ms: u64) -> Option<i64> {
    let delta = host_time_ms as i64 - now_ms as i64;
    (delta.unsigned_abs() >= MIN_STEP_MS).then_some(delta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_only_past_the_slew_threshold_in_either_direction() {
        // The verified restore gap: guest 52s behind the host.
        assert_eq!(
            step_delta_ms(1_752_000_052_000, 1_752_000_000_000),
            Some(52_000)
        );
        // Guest ahead of the host (clock was set forward while stopped).
        assert_eq!(
            step_delta_ms(1_752_000_000_000, 1_752_000_052_000),
            Some(-52_000)
        );
        // Sub-threshold offsets are NTP's job, in both directions.
        assert_eq!(step_delta_ms(1_752_000_000_999, 1_752_000_000_000), None);
        assert_eq!(step_delta_ms(1_752_000_000_000, 1_752_000_000_999), None);
        assert_eq!(
            step_delta_ms(1_752_000_001_000, 1_752_000_000_000),
            Some(1000)
        );
    }
}
