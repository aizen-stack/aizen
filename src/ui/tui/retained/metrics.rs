//! Privacy-preserving retained-frame metrics. Only timings, dimensions, hashes and counts are kept.

use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::time::Duration;

const WINDOW: usize = 240;

#[derive(Default)]
pub(super) struct FrameMetrics {
    render_us: VecDeque<u64>,
    previous_rows: Vec<u64>,
    pub frames: u64,
    pub changed_rows: u64,
    pub blinks: u64,
    pub mass_reflows: u64,
}

impl FrameMetrics {
    pub fn record(&mut self, elapsed: Duration, row_hashes: Vec<u64>, resized: bool) {
        self.frames = self.frames.saturating_add(1);
        self.render_us
            .push_back(elapsed.as_micros().min(u64::MAX as u128) as u64);
        while self.render_us.len() > WINDOW {
            self.render_us.pop_front();
        }
        if !resized && !self.previous_rows.is_empty() {
            let max = self.previous_rows.len().max(row_hashes.len());
            let same = self
                .previous_rows
                .iter()
                .zip(&row_hashes)
                .filter(|(a, b)| a == b)
                .count();
            let changed = max.saturating_sub(same);
            self.changed_rows = self.changed_rows.saturating_add(changed as u64);
            if changed >= 5 && changed * 2 >= max.max(1) {
                self.mass_reflows = self.mass_reflows.saturating_add(1);
            }
            if row_hashes.is_empty() && !self.previous_rows.is_empty() {
                self.blinks = self.blinks.saturating_add(1);
            }
        }
        self.previous_rows = row_hashes;
    }

    #[allow(dead_code)]
    pub fn summary(&self, cache_hits: u64, cache_misses: u64) -> String {
        let mut samples: Vec<u64> = self.render_us.iter().copied().collect();
        samples.sort_unstable();
        let quantile = |q: f64| -> u64 {
            if samples.is_empty() {
                return 0;
            }
            let idx = ((samples.len() - 1) as f64 * q).round() as usize;
            samples[idx]
        };
        format!(
            "frames={} render p50={:.2}ms p95={:.2}ms max={:.2}ms changed_rows={} blink={} mass_reflow={} cache={}/{}",
            self.frames,
            quantile(0.50) as f64 / 1000.0,
            quantile(0.95) as f64 / 1000.0,
            samples.last().copied().unwrap_or(0) as f64 / 1000.0,
            self.changed_rows,
            self.blinks,
            self.mass_reflows,
            cache_hits,
            cache_hits.saturating_add(cache_misses),
        )
    }
}

pub(super) fn hash_rows(rows: &[String]) -> Vec<u64> {
    rows.iter()
        .map(|row| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            row.hash(&mut h);
            h.finish()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_is_not_counted_as_reflow() {
        let mut m = FrameMetrics::default();
        m.record(
            Duration::from_millis(1),
            hash_rows(&["a".into(), "b".into()]),
            false,
        );
        m.record(
            Duration::from_millis(2),
            hash_rows(&vec!["x".to_string(); 20]),
            true,
        );
        assert_eq!(m.mass_reflows, 0);
    }

    #[test]
    fn large_unexplained_change_is_counted() {
        let mut m = FrameMetrics::default();
        m.record(
            Duration::from_millis(1),
            hash_rows(&vec!["a".into(); 10]),
            false,
        );
        m.record(
            Duration::from_millis(1),
            hash_rows(&vec!["b".into(); 10]),
            false,
        );
        assert_eq!(m.mass_reflows, 1);
    }
}
