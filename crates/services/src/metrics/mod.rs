pub mod capturing;
pub mod consts;

use async_trait::async_trait;
use opentelemetry::{
    metrics::{Counter, Histogram, Meter, MeterProvider as _},
    KeyValue,
};
use opentelemetry_sdk::metrics::MeterProvider;
use std::time::Duration;

#[async_trait]
pub trait MetricsServiceTrait: Send + Sync {
    fn record_latency(&self, name: &str, duration: Duration, tags: &[&str]);
    fn record_count(&self, name: &str, value: i64, tags: &[&str]);
    fn record_histogram(&self, name: &str, value: f64, tags: &[&str]);
}

pub struct OtlpMetricsService {
    meter: Meter,
    // Cache instruments to avoid recreating them
    latency_histograms: std::sync::Mutex<std::collections::HashMap<String, Histogram<u64>>>,
    counters: std::sync::Mutex<std::collections::HashMap<String, Counter<u64>>>,
    value_histograms: std::sync::Mutex<std::collections::HashMap<String, Histogram<f64>>>,
}

impl OtlpMetricsService {
    pub fn new(meter_provider: &MeterProvider) -> Self {
        let meter = meter_provider.meter("cloud-api");
        Self {
            meter,
            latency_histograms: std::sync::Mutex::new(std::collections::HashMap::new()),
            counters: std::sync::Mutex::new(std::collections::HashMap::new()),
            value_histograms: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn parse_tags(tags: &[&str]) -> Vec<KeyValue> {
        tags.iter()
            .filter_map(|tag| {
                let parts: Vec<&str> = tag.splitn(2, ':').collect();
                if parts.len() == 2 {
                    Some(KeyValue::new(parts[0].to_string(), parts[1].to_string()))
                } else {
                    None
                }
            })
            .collect()
    }
}

#[async_trait]
impl MetricsServiceTrait for OtlpMetricsService {
    fn record_latency(&self, name: &str, duration: Duration, tags: &[&str]) {
        let mut histograms = self.latency_histograms.lock().unwrap();
        let histogram = histograms.entry(name.to_string()).or_insert_with(|| {
            let description = match name {
                consts::METRIC_LATENCY_TTFT => {
                    "Backend TTFT: Time from provider request to first token"
                }
                consts::METRIC_LATENCY_TTFT_TOTAL => {
                    "E2E TTFT: Time from service request to first token (includes queue time)"
                }
                consts::METRIC_LATENCY_QUEUE_TIME => {
                    "Queue/Wait time: Internal overhead before provider call"
                }
                consts::METRIC_LATENCY_TOTAL => "Total E2E request processing time",
                consts::METRIC_LATENCY_DECODING_TIME => {
                    "Time from first token to last token (decoding phase)"
                }
                consts::METRIC_VERIFICATION_DURATION => "Time to complete verification operation",
                consts::METRIC_HTTP_DURATION => "HTTP request processing time",
                _ => "Latency measurement",
            };

            self.meter
                .u64_histogram(name.to_string())
                .with_description(description)
                .with_unit(opentelemetry::metrics::Unit::new("ms"))
                .init()
        });

        let kv_tags = Self::parse_tags(tags);
        histogram.record(duration.as_millis() as u64, &kv_tags);
    }

    fn record_count(&self, name: &str, value: i64, tags: &[&str]) {
        let mut counters = self.counters.lock().unwrap();
        let counter = counters.entry(name.to_string()).or_insert_with(|| {
            let description = match name {
                consts::METRIC_REQUEST_COUNT => "Total number of API requests",
                consts::METRIC_TOKENS_INPUT => "Input tokens consumed",
                consts::METRIC_TOKENS_OUTPUT => "Output tokens generated",
                consts::METRIC_VERIFICATION_SUCCESS => "Successful verification operations",
                consts::METRIC_VERIFICATION_FAILURE => "Failed verification operations",
                consts::METRIC_HTTP_REQUESTS => {
                    "Total HTTP requests by endpoint, method, and status"
                }
                consts::METRIC_REQUEST_ERRORS => "API request errors by error type",
                consts::METRIC_COST_USD => "Total cost in nano-dollars (USD) by model",
                _ => "Count",
            };

            self.meter
                .u64_counter(name.to_string())
                .with_description(description)
                .init()
        });

        let kv_tags = Self::parse_tags(tags);
        counter.add(value as u64, &kv_tags);
    }

    fn record_histogram(&self, name: &str, value: f64, tags: &[&str]) {
        let mut histograms = self.value_histograms.lock().unwrap();
        let histogram = histograms.entry(name.to_string()).or_insert_with(|| {
            let (description, unit) = match name {
                consts::METRIC_TOKENS_PER_SECOND => ("Token generation throughput", "tokens/sec"),
                _ => ("Value distribution", ""),
            };

            let builder = self
                .meter
                .f64_histogram(name.to_string())
                .with_description(description);

            let builder = if !unit.is_empty() {
                builder.with_unit(opentelemetry::metrics::Unit::new(unit))
            } else {
                builder
            };

            builder.init()
        });

        let kv_tags = Self::parse_tags(tags);
        histogram.record(value, &kv_tags);
    }
}

// Helper functions for creating properly formatted tags
/// Create a tag in the "key:value" format
pub fn tag(key: &str, value: impl std::fmt::Display) -> String {
    format!("{key}:{value}")
}

/// Create multiple tags from key-value pairs
pub fn tags(pairs: &[(&str, &str)]) -> Vec<String> {
    pairs.iter().map(|(k, v)| tag(k, v)).collect()
}

// Mock implementation for testing
pub struct MockMetricsService;

#[async_trait]
impl MetricsServiceTrait for MockMetricsService {
    fn record_latency(&self, _name: &str, _duration: Duration, _tags: &[&str]) {}
    fn record_count(&self, _name: &str, _value: i64, _tags: &[&str]) {}
    fn record_histogram(&self, _name: &str, _value: f64, _tags: &[&str]) {}
}
