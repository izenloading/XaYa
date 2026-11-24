//! Opt-in bounded diagnostics. These times do not measure physical presentation.

use std::time::{Duration, Instant};

use serde::Serialize;

const MAX_SAMPLES: usize = 4096;

pub fn enabled() -> bool {
    std::env::var_os("ALACRITTY_RENDER_METRICS").is_some_and(|value| value == "1")
}

#[derive(Default)]
pub struct Samples {
    values: Vec<f64>,
    count: u64,
    total: f64,
    max: f64,
}

#[derive(Default, Serialize)]
pub struct Summary {
    pub count: u64,
    pub retained_samples: usize,
    pub mean_us: f64,
    pub p50_us: f64,
    pub p95_us: f64,
    pub max_us: f64,
}

impl Samples {
    pub fn record(&mut self, microseconds: f64) {
        if !microseconds.is_finite() || microseconds < 0. {
            return;
        }
        if self.values.len() < MAX_SAMPLES {
            self.values.push(microseconds);
        } else {
            self.values[self.count as usize % MAX_SAMPLES] = microseconds;
        }
        self.count += 1;
        self.total += microseconds;
        self.max = self.max.max(microseconds);
    }

    pub fn summary(&self) -> Summary {
        if self.values.is_empty() {
            return Summary::default();
        }
        let mut values = self.values.clone();
        values.sort_by(f64::total_cmp);
        let percentile = |percent: usize| values[(values.len() * percent).div_ceil(100) - 1];
        Summary {
            count: self.count,
            retained_samples: values.len(),
            mean_us: self.total / self.count as f64,
            p50_us: percentile(50),
            p95_us: percentile(95),
            max_us: self.max,
        }
    }
}

#[derive(Default)]
pub struct FrameMetrics {
    cpu: Samples,
    input_to_submit: Samples,
    pending_input: Option<Instant>,
}

impl FrameMetrics {
    pub fn input(&mut self) {
        self.pending_input.get_or_insert_with(Instant::now);
    }

    pub fn discard_input(&mut self) {
        self.pending_input = None;
    }

    pub fn submitted(&mut self, elapsed: Duration) {
        self.cpu.record(elapsed.as_secs_f64() * 1e6);
        if let Some(input) = self.pending_input.take() {
            self.input_to_submit.record(input.elapsed().as_secs_f64() * 1e6);
        }
    }

    pub fn log(&self, window: u64) {
        log::info!(
            "Render metrics: {}",
            serde_json::json!({
                "window": window,
                "scope": "observed input event to next dirty-frame submission; not correlated echo or physical display latency",
                "cpu_frame_us": self.cpu.summary(),
                "input_event_to_submit_us": self.input_to_submit.summary(),
            })
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_percentiles_and_rejects_nonfinite_samples() {
        let mut samples = Samples::default();
        for value in [10., 40., 20., 30., f64::NAN, f64::INFINITY, -1.] {
            samples.record(value);
        }
        let summary = samples.summary();
        assert_eq!(summary.count, 4);
        assert_eq!(summary.mean_us, 25.);
        assert_eq!(summary.p50_us, 20.);
        assert_eq!(summary.p95_us, 40.);
    }

    #[test]
    fn ignored_input_does_not_pollute_a_later_frame_sample() {
        let mut metrics = FrameMetrics::default();
        metrics.input();
        metrics.discard_input();
        metrics.submitted(Duration::from_micros(10));
        assert_eq!(metrics.input_to_submit.summary().count, 0);
        assert_eq!(metrics.cpu.summary().count, 1);
    }

    #[test]
    fn bounds_storage_without_losing_lifetime_counts() {
        let mut samples = Samples::default();
        for index in 0..MAX_SAMPLES * 2 {
            samples.record(index as f64);
        }
        let summary = samples.summary();
        assert_eq!(summary.count, (MAX_SAMPLES * 2) as u64);
        assert_eq!(summary.retained_samples, MAX_SAMPLES);
        assert!(summary.p50_us >= MAX_SAMPLES as f64);
        assert_eq!(summary.max_us, (MAX_SAMPLES * 2 - 1) as f64);
        assert_eq!(Samples::default().summary().count, 0);
    }
}
