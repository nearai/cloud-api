use config::OtlpConfig;
use opentelemetry::trace::Status;
use opentelemetry::KeyValue;
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{
    BatchConfigBuilder, BatchSpanProcessor, Sampler, SdkTracerProvider, Span, SpanData, SpanEvents,
    SpanLimits, SpanLinks, SpanProcessor,
};
use opentelemetry_sdk::Resource;
use std::time::Duration;

/// Span attribute keys that may leave the process. Any other attribute is
/// removed before export, so a field added to a span later stays local unless
/// it is listed here after review.
pub const SPAN_ATTRIBUTE_ALLOWLIST: &[&str] = &[
    "request_id",
    "org_id",
    "organization_id",
    "workspace_id",
    "api_key_id",
    "model",
    "model_id",
    "inference_type",
    "inference_id",
    "http.request.method",
    "http.route",
    "http.response.status_code",
    "error.kind",
    "input_tokens",
    "output_tokens",
    "ttft_ms",
    "client_disconnect",
    "busy_ns",
    "idle_ns",
];

const MAX_EXPORT_BATCH_SIZE: usize = 512;
const MAX_QUEUE_SIZE: usize = 2048;
const MAX_ATTRIBUTES_PER_SPAN: u32 = 32;

/// Wraps another span processor and strips everything that is not explicitly
/// allowed from each finished span: attributes outside the allowlist, all
/// events and links, and the description of an error status.
#[derive(Debug)]
pub struct AllowlistSpanProcessor<P> {
    inner: P,
}

impl<P> AllowlistSpanProcessor<P> {
    pub fn new(inner: P) -> Self {
        Self { inner }
    }
}

impl<P: SpanProcessor> SpanProcessor for AllowlistSpanProcessor<P> {
    fn on_start(&self, span: &mut Span, cx: &opentelemetry::Context) {
        self.inner.on_start(span, cx);
    }

    fn on_end(&self, mut span: SpanData) {
        span.attributes
            .retain(|attribute| SPAN_ATTRIBUTE_ALLOWLIST.contains(&attribute.key.as_str()));
        span.events = SpanEvents::default();
        span.links = SpanLinks::default();
        if matches!(span.status, Status::Error { .. }) {
            span.status = Status::Error {
                description: "".into(),
            };
        }
        self.inner.on_end(span);
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

fn span_limits() -> SpanLimits {
    SpanLimits {
        max_events_per_span: 0,
        max_attributes_per_span: MAX_ATTRIBUTES_PER_SPAN,
        max_links_per_span: 0,
        max_attributes_per_event: 0,
        max_attributes_per_link: 0,
    }
}

fn sampler(ratio: f64) -> Sampler {
    Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(ratio)))
}

/// Build the tracer provider, or `None` when tracing is disabled.
///
/// Spans go to the configured OTLP gRPC endpoint through a bounded batch
/// processor that drops spans when its queue is full. The W3C trace-context
/// propagator is installed globally for outbound header injection only.
pub fn build_tracer_provider(config: &OtlpConfig, resource: Resource) -> Option<SdkTracerProvider> {
    if !config.traces_enabled {
        return None;
    }
    let exporter = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(&config.endpoint)
        .build()
        .expect("Failed to build OTLP span exporter");
    let batch_config = BatchConfigBuilder::default()
        .with_max_export_batch_size(MAX_EXPORT_BATCH_SIZE)
        .with_max_queue_size(MAX_QUEUE_SIZE)
        .build();
    let batch = BatchSpanProcessor::builder(exporter)
        .with_batch_config(batch_config)
        .build();
    let provider = SdkTracerProvider::builder()
        .with_resource(resource)
        .with_sampler(sampler(config.traces_sample_ratio))
        .with_span_limits(span_limits())
        .with_span_processor(AllowlistSpanProcessor::new(batch))
        .build();
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );
    Some(provider)
}

/// A tracer provider for tests: the same sampler, limits and allowlist as
/// production, with a caller-supplied exporter processor.
pub fn tracer_provider_with_processor<P: SpanProcessor + 'static>(
    ratio: f64,
    processor: P,
) -> SdkTracerProvider {
    SdkTracerProvider::builder()
        .with_sampler(sampler(ratio))
        .with_span_limits(span_limits())
        .with_span_processor(AllowlistSpanProcessor::new(processor))
        .build()
}

pub fn build_telemetry_resource(environment: &str, instance_id: Option<&str>) -> Resource {
    let mut attributes = vec![
        KeyValue::new("service.name", "cloud-api"),
        KeyValue::new("environment", environment.to_string()),
    ];
    if let Some(instance_id) = instance_id {
        attributes.push(KeyValue::new(
            "service.instance.id",
            instance_id.to_string(),
        ));
    }

    Resource::builder().with_attributes(attributes).build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{Tracer, TracerProvider as _};
    use opentelemetry::Key;
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SimpleSpanProcessor};

    fn exported_with(
        ratio: f64,
        emit: impl FnOnce(&opentelemetry_sdk::trace::SdkTracer),
    ) -> Vec<SpanData> {
        let exporter = InMemorySpanExporter::default();
        let provider =
            tracer_provider_with_processor(ratio, SimpleSpanProcessor::new(exporter.clone()));
        emit(&provider.tracer("test"));
        provider.force_flush().expect("flush");
        exporter.get_finished_spans().expect("spans")
    }

    #[test]
    fn export_keeps_only_allowlisted_attributes_and_drops_events_and_status_text() {
        use opentelemetry::trace::Span as _;
        let spans = exported_with(1.0, |tracer| {
            let mut span = tracer.start("span");
            span.set_attribute(KeyValue::new("request_id", "r1"));
            span.set_attribute(KeyValue::new("http.route", "/v1/things/{id}"));
            span.set_attribute(KeyValue::new("path", "/v1/things/secret"));
            span.set_attribute(KeyValue::new("client_request_id", "c1"));
            span.set_attribute(KeyValue::new("method", "GET"));
            span.set_attribute(KeyValue::new("code.filepath", "src/x.rs"));
            span.set_attribute(KeyValue::new("thread.id", 1_i64));
            span.set_attribute(KeyValue::new("requested_model", "client-input"));
            span.add_event("message", vec![KeyValue::new("body", "customer text")]);
            span.set_status(Status::error("upstream said customer text"));
            span.end();
        });
        assert_eq!(spans.len(), 1);
        let span = &spans[0];
        let mut keys: Vec<&str> = span.attributes.iter().map(|kv| kv.key.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["http.route", "request_id"]);
        assert!(span.events.is_empty());
        assert!(span.links.is_empty());
        assert_eq!(
            span.status,
            Status::Error {
                description: "".into()
            }
        );
    }

    #[test]
    fn disabled_traces_build_no_provider() {
        let config = OtlpConfig {
            traces_enabled: false,
            ..OtlpConfig::default()
        };
        assert!(build_tracer_provider(&config, build_telemetry_resource("local", None)).is_none());
    }

    #[test]
    fn zero_ratio_samples_no_root_spans() {
        let spans = exported_with(0.0, |tracer| {
            opentelemetry::trace::Span::end(&mut tracer.start("span"));
        });
        assert!(spans.is_empty());
    }

    #[test]
    fn telemetry_resource_sets_configured_instance_id() {
        let resource = build_telemetry_resource("staging", Some("vmm-instance-123"));

        assert_eq!(
            resource
                .get(&Key::new("service.instance.id"))
                .map(|value| value.to_string()),
            Some("vmm-instance-123".to_string())
        );
    }

    #[test]
    fn telemetry_resource_omits_unset_instance_id() {
        let resource = build_telemetry_resource("local", None);

        assert_eq!(
            resource
                .get(&Key::new("service.name"))
                .map(|value| value.to_string()),
            Some("cloud-api".to_string())
        );
        assert_eq!(
            resource
                .get(&Key::new("environment"))
                .map(|value| value.to_string()),
            Some("local".to_string())
        );
        assert!(resource.get(&Key::new("service.instance.id")).is_none());
    }
}
