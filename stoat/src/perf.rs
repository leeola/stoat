//! Main-thread latency instrumentation, compiled only under the `perf`
//! feature.
//!
//! Rendering is on-demand, so the meaningful measure is how long the loop
//! takes to turn an event into a published frame, not a frame rate. Each
//! metric is recorded into a fixed ring with percentiles computed on demand,
//! so a live readout stays cheap and needs no histogram dependency.
//!
//! Timing reads [`std::time::Instant`], i.e. real elapsed wall time, not the
//! scheduler's virtual clock.

use std::time::Duration;

/// Samples retained per metric. At a few hundred frames a second of activity
/// this is several seconds of history, enough for a stable percentile
/// readout.
const RING: usize = 4096;

/// last/p50/p95/worst of one metric over its retained ring, in the metric's
/// own unit (nanoseconds for durations, a raw count for `coalesced`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MetricStats {
    pub last: u64,
    pub p50: u64,
    pub p95: u64,
    pub worst: u64,
}

/// Per-frame main-thread metrics, each a fixed ring of recent samples.
///
/// Populated by [`crate::app::Stoat::run`] around the update, drain, and paint
/// steps. The status-bar readout and the exit summary read the percentiles
/// back through the `*_stats` accessors.
#[derive(Default)]
pub struct PerfStats {
    update: Ring,
    paint: Ring,
    input_to_publish: Ring,
    coalesced: Ring,
    anim_tick: Ring,
}

impl PerfStats {
    /// Time spent applying one event in `update`.
    pub fn record_update(&mut self, elapsed: Duration) {
        self.update.record(elapsed.as_nanos() as u64);
    }

    /// Time spent painting the frame buffer.
    pub fn record_paint(&mut self, elapsed: Duration) {
        self.paint.record(elapsed.as_nanos() as u64);
    }

    /// Latency from the first event of a frame to publishing that frame.
    pub fn record_input_to_publish(&mut self, elapsed: Duration) {
        self.input_to_publish.record(elapsed.as_nanos() as u64);
    }

    /// How many further messages a frame coalesced after its first event.
    pub fn record_coalesced(&mut self, count: usize) {
        self.coalesced.record(count as u64);
    }

    /// Interval between consecutive animation ticks.
    pub fn record_anim_tick(&mut self, interval: Duration) {
        self.anim_tick.record(interval.as_nanos() as u64);
    }

    pub fn update_stats(&self) -> Option<MetricStats> {
        self.update.stats()
    }

    pub fn paint_stats(&self) -> Option<MetricStats> {
        self.paint.stats()
    }

    pub fn input_to_publish_stats(&self) -> Option<MetricStats> {
        self.input_to_publish.stats()
    }

    pub fn coalesced_stats(&self) -> Option<MetricStats> {
        self.coalesced.stats()
    }

    pub fn anim_tick_stats(&self) -> Option<MetricStats> {
        self.anim_tick.stats()
    }
}

/// A fixed-capacity ring of `u64` samples with on-demand percentiles.
#[derive(Default)]
struct Ring {
    data: Vec<u64>,
    next: usize,
}

impl Ring {
    fn record(&mut self, value: u64) {
        if self.data.len() < RING {
            self.data.push(value);
        } else {
            self.data[self.next] = value;
        }
        self.next = (self.next + 1) % RING;
    }

    fn stats(&self) -> Option<MetricStats> {
        if self.data.is_empty() {
            return None;
        }
        let last = self.data[(self.next + RING - 1) % RING];
        let mut sorted = self.data.clone();
        sorted.sort_unstable();
        Some(MetricStats {
            last,
            p50: nearest_rank(&sorted, 0.50),
            p95: nearest_rank(&sorted, 0.95),
            worst: *sorted.last().expect("ring is non-empty"),
        })
    }
}

/// Nearest-rank percentile of an ascending slice, `p` in `0.0..=1.0`.
fn nearest_rank(sorted: &[u64], p: f64) -> u64 {
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_stats_is_none_until_a_sample_lands() {
        assert!(Ring::default().stats().is_none());
    }

    #[test]
    fn percentiles_pick_by_nearest_rank() {
        let mut ring = Ring::default();
        for i in 1..=100 {
            ring.record(i);
        }
        assert_eq!(
            ring.stats().expect("stats"),
            MetricStats {
                last: 100,
                p50: 51,
                p95: 95,
                worst: 100,
            },
        );
    }

    #[test]
    fn ring_retains_the_newest_samples() {
        let mut ring = Ring::default();
        for i in 1..=(RING as u64 + 10) {
            ring.record(i);
        }
        let stats = ring.stats().expect("stats");
        assert_eq!(stats.last, RING as u64 + 10);
        assert_eq!(stats.worst, RING as u64 + 10);
        // The first ten samples were overwritten, so the oldest retained is 11.
        assert_eq!(
            stats.p50,
            nearest_rank(&(11..=RING as u64 + 10).collect::<Vec<_>>(), 0.50)
        );
    }

    #[test]
    fn each_record_routes_to_its_own_metric() {
        let mut perf = PerfStats::default();
        perf.record_update(Duration::from_micros(5));
        perf.record_paint(Duration::from_micros(10));
        perf.record_input_to_publish(Duration::from_micros(20));
        perf.record_coalesced(3);
        perf.record_anim_tick(Duration::from_millis(8));

        assert_eq!(perf.update_stats().expect("update").last, 5_000);
        assert_eq!(perf.paint_stats().expect("paint").last, 10_000);
        assert_eq!(perf.input_to_publish_stats().expect("latency").last, 20_000);
        assert_eq!(perf.coalesced_stats().expect("coalesced").last, 3);
        assert_eq!(perf.anim_tick_stats().expect("anim").last, 8_000_000);
    }
}
