//! Per-frame CPU timing for the renderer, gated behind the `perf` feature.
//!
//! When the feature is off, [`FrameProfiler`] is a zero-sized twin whose
//! marker methods inline away to nothing, so the render path carries no
//! timing cost and `GpuContext` gains no real field. When on, it records the
//! surface-acquire, encode/submit, and present spans of each presented frame
//! plus the present-to-present interval into a fixed ring, and computes
//! percentiles on demand.
//!
//! Timing reads [`std::time::Instant`], i.e. real elapsed wall time, not the
//! scheduler's virtual clock.

#[cfg(feature = "perf")]
pub use enabled::{FrameProfiler, FrameSample, FrameStats, Percentiles};

#[cfg(feature = "perf")]
mod enabled {
    use std::time::{Duration, Instant};

    /// Frames retained in the ring. At 60-144 Hz this covers a 1.5-4 s
    /// window, enough for a stable interactive percentile readout.
    const RING: usize = 240;

    /// CPU-side timing of one presented frame.
    ///
    /// `gpu` is filled by the timestamp-query path when the adapter supports
    /// it. It stays `None` otherwise, and until a query result lands a few
    /// frames after the frame it measures.
    #[derive(Clone, Copy)]
    pub struct FrameSample {
        pub acquire: Duration,
        pub encode: Duration,
        pub present: Duration,
        pub interval: Duration,
        pub gpu: Option<Duration>,
    }

    impl FrameSample {
        /// The frame's total main-thread cost, summing surface acquire,
        /// encode/submit, and present.
        pub fn cpu(&self) -> Duration {
            self.acquire + self.encode + self.present
        }
    }

    /// p50/p95/worst of one metric over the retained ring.
    #[derive(Clone, Copy)]
    pub struct Percentiles {
        pub p50: Duration,
        pub p95: Duration,
        pub worst: Duration,
    }

    /// A snapshot of the ring holding the newest frame plus percentiles of
    /// the headline metrics.
    ///
    /// `gpu` is `Some` only when at least one retained frame carried a GPU
    /// duration.
    pub struct FrameStats {
        pub frames: usize,
        pub last: FrameSample,
        pub cpu: Percentiles,
        pub interval: Percentiles,
        pub gpu: Option<Percentiles>,
    }

    /// Fixed-ring recorder of per-frame CPU timing.
    ///
    /// Drive it once per presented frame. Call [`Self::begin_frame`] before
    /// the surface acquire, [`Self::mark_acquired`] after it,
    /// [`Self::mark_submitted`] after the last submit, and [`Self::end_frame`]
    /// after present. A frame the renderer skips on transient surface loss
    /// records nothing, because only `end_frame` pushes a sample and the next
    /// `begin_frame` resets the pending start.
    pub struct FrameProfiler {
        ring: Vec<FrameSample>,
        next: usize,
        frame_start: Option<Instant>,
        acquired_at: Option<Instant>,
        submitted_at: Option<Instant>,
        last_end: Option<Instant>,
    }

    impl Default for FrameProfiler {
        fn default() -> Self {
            FrameProfiler::new()
        }
    }

    impl FrameProfiler {
        pub fn new() -> FrameProfiler {
            FrameProfiler {
                ring: Vec::with_capacity(RING),
                next: 0,
                frame_start: None,
                acquired_at: None,
                submitted_at: None,
                last_end: None,
            }
        }

        pub fn begin_frame(&mut self) {
            self.frame_start = Some(Instant::now());
            self.acquired_at = None;
            self.submitted_at = None;
        }

        pub fn mark_acquired(&mut self) {
            self.acquired_at = Some(Instant::now());
        }

        pub fn mark_submitted(&mut self) {
            self.submitted_at = Some(Instant::now());
        }

        pub fn end_frame(&mut self) {
            let now = Instant::now();
            let start = self.frame_start.unwrap_or(now);
            let acquired = self.acquired_at.unwrap_or(start);
            let submitted = self.submitted_at.unwrap_or(acquired);
            let sample = FrameSample {
                acquire: acquired.saturating_duration_since(start),
                encode: submitted.saturating_duration_since(acquired),
                present: now.saturating_duration_since(submitted),
                interval: self
                    .last_end
                    .map(|prev| now.saturating_duration_since(prev))
                    .unwrap_or_default(),
                gpu: None,
            };
            self.record(sample);
            self.last_end = Some(now);
            self.frame_start = None;
        }

        /// Snapshot the ring, or `None` when no frame has been recorded yet.
        pub fn stats(&self) -> Option<FrameStats> {
            if self.ring.is_empty() {
                return None;
            }
            let last = self.ring[(self.next + RING - 1) % RING];
            let gpu: Vec<Duration> = self.ring.iter().filter_map(|s| s.gpu).collect();
            Some(FrameStats {
                frames: self.ring.len(),
                last,
                cpu: percentiles(self.ring.iter().map(FrameSample::cpu)),
                interval: percentiles(self.ring.iter().map(|s| s.interval)),
                gpu: (!gpu.is_empty()).then(|| percentiles(gpu.into_iter())),
            })
        }

        /// The retained frame samples in oldest-to-newest order.
        ///
        /// [`stats`](Self::stats) collapses the ring to percentiles. The perf
        /// HUD graphs the raw per-frame series instead, so it needs the samples
        /// laid out chronologically across the ring's circular split point.
        pub fn samples(&self) -> Vec<FrameSample> {
            let split = self.next.min(self.ring.len());
            let (older, newer) = self.ring.split_at(split);
            newer.iter().chain(older).copied().collect()
        }

        /// Attach a GPU duration to the most recently recorded frame.
        ///
        /// The GPU time is read back a few frames after the frame it measures,
        /// so it lands on a slightly later sample than the one it timed. The
        /// ring's percentiles are unaffected by that constant shift, so no
        /// per-frame matching is done. A no-op on an empty ring.
        pub fn attach_gpu(&mut self, gpu: Duration) {
            if self.ring.is_empty() {
                return;
            }
            let last = (self.next + RING - 1) % RING;
            self.ring[last].gpu = Some(gpu);
        }

        fn record(&mut self, sample: FrameSample) {
            if self.ring.len() < RING {
                self.ring.push(sample);
            } else {
                self.ring[self.next] = sample;
            }
            self.next = (self.next + 1) % RING;
        }
    }

    fn percentiles(values: impl Iterator<Item = Duration>) -> Percentiles {
        let mut sorted: Vec<Duration> = values.collect();
        sorted.sort_unstable();
        Percentiles {
            p50: nearest_rank(&sorted, 0.50),
            p95: nearest_rank(&sorted, 0.95),
            worst: *sorted
                .last()
                .expect("percentiles called over a non-empty ring"),
        }
    }

    /// Nearest-rank percentile of an ascending slice, `p` in `0.0..=1.0`.
    fn nearest_rank(sorted: &[Duration], p: f64) -> Duration {
        let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn ms(n: u64) -> Duration {
            Duration::from_millis(n)
        }

        /// A sample whose cpu() is `cpu_ms` and interval is twice that, so
        /// the two metrics stay distinguishable in assertions.
        fn sample(cpu_ms: u64) -> FrameSample {
            FrameSample {
                acquire: ms(cpu_ms),
                encode: Duration::ZERO,
                present: Duration::ZERO,
                interval: ms(cpu_ms * 2),
                gpu: None,
            }
        }

        #[test]
        fn stats_is_none_until_a_frame_is_recorded() {
            assert!(FrameProfiler::new().stats().is_none());
        }

        #[test]
        fn percentiles_pick_by_nearest_rank() {
            let mut p = FrameProfiler::new();
            for i in 1..=100 {
                p.record(sample(i));
            }
            let stats = p.stats().expect("stats");
            assert_eq!(stats.frames, 100);
            assert_eq!(stats.last.cpu(), ms(100));
            assert_eq!(stats.cpu.p50, ms(51));
            assert_eq!(stats.cpu.p95, ms(95));
            assert_eq!(stats.cpu.worst, ms(100));
            assert_eq!(stats.interval.p50, ms(102));
            assert_eq!(stats.interval.worst, ms(200));
        }

        #[test]
        fn ring_retains_the_newest_240_frames() {
            let mut p = FrameProfiler::new();
            for i in 1..=300 {
                p.record(sample(i));
            }
            let stats = p.stats().expect("stats");
            assert_eq!(stats.frames, 240);
            assert_eq!(stats.last.cpu(), ms(300));
            assert_eq!(stats.cpu.worst, ms(300));
            assert_eq!(stats.cpu.p50, ms(181));
        }

        #[test]
        fn samples_are_oldest_to_newest_across_the_ring_split() {
            let mut p = FrameProfiler::new();
            for i in 1..=3 {
                p.record(sample(i));
            }
            let filling: Vec<Duration> = p.samples().iter().map(FrameSample::cpu).collect();
            assert_eq!(filling, vec![ms(1), ms(2), ms(3)]);

            for i in 4..=300 {
                p.record(sample(i));
            }
            let wrapped = p.samples();
            assert_eq!(wrapped.len(), 240);
            assert_eq!(wrapped.first().unwrap().cpu(), ms(61));
            assert_eq!(wrapped.last().unwrap().cpu(), ms(300));
        }

        #[test]
        fn attach_gpu_sets_the_most_recent_sample() {
            let mut p = FrameProfiler::new();
            p.record(sample(10));
            p.attach_gpu(ms(4));
            assert_eq!(p.stats().expect("stats").last.gpu, Some(ms(4)));

            let mut empty = FrameProfiler::new();
            empty.attach_gpu(ms(4));
            assert!(
                empty.stats().is_none(),
                "attach on an empty ring is a no-op"
            );
        }

        #[test]
        fn gpu_percentiles_are_absent_until_a_gpu_sample_lands() {
            let mut p = FrameProfiler::new();
            p.record(sample(10));
            assert!(p.stats().expect("stats").gpu.is_none());

            let mut with_gpu = sample(20);
            with_gpu.gpu = Some(ms(5));
            p.record(with_gpu);
            let gpu = p.stats().expect("stats").gpu.expect("gpu present");
            assert_eq!(gpu.worst, ms(5));
        }
    }
}

#[cfg(not(feature = "perf"))]
pub use disabled::FrameProfiler;

#[cfg(not(feature = "perf"))]
mod disabled {
    /// Zero-sized stand-in compiled when the `perf` feature is off. Every
    /// marker is an inlined no-op, so the render path pays nothing and
    /// `GpuContext` carries no timing state.
    #[derive(Default)]
    pub struct FrameProfiler;

    impl FrameProfiler {
        #[inline]
        pub fn new() -> FrameProfiler {
            FrameProfiler
        }

        #[inline]
        pub fn begin_frame(&mut self) {}

        #[inline]
        pub fn mark_acquired(&mut self) {}

        #[inline]
        pub fn mark_submitted(&mut self) {}

        #[inline]
        pub fn end_frame(&mut self) {}
    }
}
