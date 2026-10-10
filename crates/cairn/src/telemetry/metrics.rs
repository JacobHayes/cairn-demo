//! A small Sentry fan-out, retaining the existing Prometheus recorder and its labels.
use metrics::{
    Counter, CounterFn, Gauge, Histogram, HistogramFn, Key, KeyName, Metadata, Recorder,
    SharedString, Unit,
};
use metrics_exporter_prometheus::PrometheusRecorder;
use std::sync::Arc;

pub(super) struct Fanout(pub PrometheusRecorder);

fn selected(key: &Key) -> bool {
    matches!(
        key.name(),
        "cairn_api_requests_total"
            | "cairn_api_request_duration_seconds"
            | "cairn_api_patches_total"
            | "cairn_write_commits_total"
            | "cairn_assistant_turns_total"
            | "cairn_assistant_tokens_total"
            | "cairn_assistant_provider_call_seconds"
            | "cairn_mcp_tool_calls_total"
            | "cairn_mcp_tool_call_duration_seconds"
            | "cairn_store_commit_duration_seconds"
    )
}

impl Recorder for Fanout {
    fn describe_counter(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.0.describe_counter(key, unit, description);
    }
    fn describe_gauge(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.0.describe_gauge(key, unit, description);
    }
    fn describe_histogram(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.0.describe_histogram(key, unit, description);
    }
    fn register_counter(&self, key: &Key, metadata: &Metadata<'_>) -> Counter {
        let original = self.0.register_counter(key, metadata);
        if selected(key) {
            Counter::from_arc(Arc::new(Count {
                original,
                key: key.clone(),
            }))
        } else {
            original
        }
    }
    fn register_gauge(&self, key: &Key, metadata: &Metadata<'_>) -> Gauge {
        self.0.register_gauge(key, metadata)
    }
    fn register_histogram(&self, key: &Key, metadata: &Metadata<'_>) -> Histogram {
        let original = self.0.register_histogram(key, metadata);
        if selected(key) {
            Histogram::from_arc(Arc::new(Distribution {
                original,
                key: key.clone(),
            }))
        } else {
            original
        }
    }
}

struct Count {
    original: Counter,
    key: Key,
}
impl CounterFn for Count {
    #[expect(
        clippy::cast_precision_loss,
        reason = "Sentry metrics use f64; increments here are request and token counts"
    )]
    fn increment(&self, value: u64) {
        self.original.increment(value);
        let mut metric = sentry::metrics::counter(self.key.name().to_owned(), value as f64);
        for label in self.key.labels() {
            metric = metric.attribute(label.key().to_owned(), label.value().to_owned());
        }
        metric.capture();
    }
    fn absolute(&self, value: u64) {
        self.original.absolute(value);
    }
}
struct Distribution {
    original: Histogram,
    key: Key,
}
impl HistogramFn for Distribution {
    fn record(&self, value: f64) {
        self.original.record(value);
        let mut metric = sentry::metrics::distribution(self.key.name().to_owned(), value)
            .unit(sentry::protocol::Unit::Second);
        for label in self.key.labels() {
            metric = metric.attribute(label.key().to_owned(), label.value().to_owned());
        }
        metric.capture();
    }
}
