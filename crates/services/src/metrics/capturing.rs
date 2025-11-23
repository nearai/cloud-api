use std::time::Duration;
use crate::metrics::MetricsServiceTrait;
use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct RecordedMetric {
    pub name: String,
    pub value: MetricValue,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum MetricValue {
    Latency(Duration),
    Count(i64),
    Histogram(f64),
}

pub struct CapturingMetricsService {
    pub metrics: std::sync::Mutex<Vec<RecordedMetric>>,
}

impl CapturingMetricsService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_metrics(&self) -> Vec<RecordedMetric> {
        self.metrics.lock().unwrap().clone()
    }
}

impl Default for CapturingMetricsService {
    fn default() -> Self {
        Self {
            metrics: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl MetricsServiceTrait for CapturingMetricsService {
    fn record_latency(&self, name: &str, duration: Duration, tags: &[&str]) {
        let mut metrics = self.metrics.lock().unwrap();
        metrics.push(RecordedMetric {
            name: name.to_string(),
            value: MetricValue::Latency(duration),
            tags: tags.iter().map(|s| s.to_string()).collect(),
        });
    }

    fn record_count(&self, name: &str, value: i64, tags: &[&str]) {
        let mut metrics = self.metrics.lock().unwrap();
        metrics.push(RecordedMetric {
            name: name.to_string(),
            value: MetricValue::Count(value),
            tags: tags.iter().map(|s| s.to_string()).collect(),
        });
    }

    fn record_histogram(&self, name: &str, value: f64, tags: &[&str]) {
        let mut metrics = self.metrics.lock().unwrap();
        metrics.push(RecordedMetric {
            name: name.to_string(),
            value: MetricValue::Histogram(value),
            tags: tags.iter().map(|s| s.to_string()).collect(),
        });
    }
}
