//! Content-free distributed tracing shared by native Maestro processes.
//!
//! This module owns W3C propagation, the OTLP exporter, and the small set of
//! lifecycle spans used by the gateway and hosted runner.  It deliberately
//! keeps prompts, completions, tool arguments/results, credentials, paths, and
//! tenant identifiers out of ordinary span attributes.

use std::{
    collections::HashMap,
    env,
    str::FromStr,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use anyhow::Result;
use http::{HeaderMap, HeaderName, HeaderValue};
use opentelemetry::{
    KeyValue, global,
    propagation::TextMapPropagator as _,
    trace::{SpanContext, TraceContextExt as _, TracerProvider as _},
};
use opentelemetry_otlp::{SpanExporter, WithExportConfig as _, WithHttpConfig as _};
use opentelemetry_sdk::{
    Resource,
    propagation::TraceContextPropagator,
    trace::{Sampler, SdkTracerProvider},
};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt as _, util::SubscriberInitExt as _};
use uuid::Uuid;

const TRACEPARENT_HEADER: &str = "traceparent";
const TRACESTATE_HEADER: &str = "tracestate";
const DEFAULT_SAMPLE_RATE: f64 = 0.1;
const EXPORT_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_TRACEPARENT_BYTES: usize = 55;
const MAX_TRACESTATE_BYTES: usize = 512;
const MAX_ATTRIBUTE_BYTES: usize = 160;

/// Configuration for one native Maestro process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TelemetryConfig {
    pub service_name: &'static str,
    pub service_version: &'static str,
    pub default_filter: &'static str,
    pub environment: &'static str,
}

impl TelemetryConfig {
    pub const fn new(
        service_name: &'static str,
        service_version: &'static str,
        default_filter: &'static str,
        environment: &'static str,
    ) -> Self {
        Self {
            service_name,
            service_version,
            default_filter,
            environment,
        }
    }
}

/// Validated W3C context suitable for private durable propagation fields.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TraceHeaders {
    traceparent: String,
    tracestate: String,
}

impl TraceHeaders {
    /// Invalid values become empty so the next boundary starts a safe root.
    #[must_use]
    pub fn from_values(traceparent: Option<&str>, tracestate: Option<&str>) -> Self {
        let Some(traceparent) = normalize_traceparent(traceparent) else {
            return Self::default();
        };
        Self {
            traceparent,
            tracestate: normalize_tracestate(tracestate),
        }
    }

    #[must_use]
    pub fn from_headers(headers: &HeaderMap) -> Self {
        Self::from_values(
            headers
                .get(TRACEPARENT_HEADER)
                .and_then(|value| value.to_str().ok()),
            headers
                .get(TRACESTATE_HEADER)
                .and_then(|value| value.to_str().ok()),
        )
    }

    #[must_use]
    pub fn traceparent(&self) -> Option<&str> {
        (!self.traceparent.is_empty()).then_some(self.traceparent.as_str())
    }

    #[must_use]
    pub fn tracestate(&self) -> Option<&str> {
        (!self.tracestate.is_empty()).then_some(self.tracestate.as_str())
    }

    #[must_use]
    pub fn trace_id(&self) -> Option<&str> {
        self.traceparent.split('-').nth(1)
    }

    #[must_use]
    pub fn span_id(&self) -> Option<&str> {
        self.traceparent.split('-').nth(2)
    }

    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.traceparent().is_some()
    }

    #[must_use]
    pub fn header_map(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(traceparent) = self.traceparent() {
            if let Ok(value) = HeaderValue::from_str(traceparent) {
                headers.insert(HeaderName::from_static(TRACEPARENT_HEADER), value);
            }
        }
        if let Some(tracestate) = self.tracestate() {
            if let Ok(value) = HeaderValue::from_str(tracestate) {
                headers.insert(HeaderName::from_static(TRACESTATE_HEADER), value);
            }
        }
        headers
    }

    /// Return a fresh child context for a durable retry or callback.
    #[must_use]
    pub fn child(&self) -> Self {
        let trace_id = self
            .trace_id()
            .map(ToOwned::to_owned)
            .unwrap_or_else(new_trace_id);
        let flags = self.traceparent.split('-').nth(3).unwrap_or("00");
        Self {
            traceparent: format!("00-{trace_id}-{}-{flags}", new_span_id()),
            tracestate: self.tracestate.clone(),
        }
    }
}

/// A stable route class that cannot contain query strings or dynamic IDs.
#[must_use]
pub fn route_class(path: &str) -> String {
    let path = path.split('?').next().unwrap_or(path);
    let route = match path {
        "/healthz" | "/readyz" | "/api/status" | "/api/models" | "/api/model" | "/api/config"
        | "/api/metrics" | "/api/telemetry" | "/api/training" => path,
        "/api/chat" | "/api/chat/ws" => path,
        _ if path.starts_with("/api/headless/threads/") && path.ends_with("/turns") => {
            "/api/headless/threads/:id/turns"
        }
        _ if path.starts_with("/api/headless/threads/") && path.ends_with("/events") => {
            "/api/headless/threads/:id/events"
        }
        _ if path.starts_with("/api/headless/threads/") => "/api/headless/threads/:id",
        _ if path.starts_with("/api/headless/sessions/") && path.ends_with("/messages") => {
            "/api/headless/sessions/:id/messages"
        }
        _ if path.starts_with("/api/headless/sessions/") && path.ends_with("/message") => {
            "/api/headless/sessions/:id/message"
        }
        _ if path.starts_with("/api/headless/sessions/") && path.ends_with("/events") => {
            "/api/headless/sessions/:id/events"
        }
        _ if path.starts_with("/api/headless/sessions/") => "/api/headless/sessions/:id",
        _ if path.starts_with("/api/sessions/") => "/api/sessions/:id",
        _ if path.starts_with("/api/pending-requests/") => "/api/pending-requests/:id",
        _ if path.starts_with("/api/a2a/") => "/api/a2a/:operation",
        _ if path.starts_with("/api/") => "/api/:operation",
        _ if path.starts_with("/api") => "/api/:operation",
        _ => "/other",
    };
    route.to_owned()
}

/// Create a semantic HTTP server span with a validated remote parent.
#[must_use]
pub fn server_span(method: &str, route: &str, parent: &TraceHeaders) -> tracing::Span {
    let method = bounded_attribute(method, 32);
    let route = route_class(route);
    let span = tracing::info_span!(
        "http.request",
        otel.name = format!("{method} {route}"),
        otel.kind = "server",
        http.request.method = method.as_str(),
        http.route = route.as_str(),
        http.response.status_code = tracing::field::Empty,
        http.server.request.duration_ms = tracing::field::Empty,
        duration_ms = tracing::field::Empty,
        error.type = tracing::field::Empty,
        otel.status_code = tracing::field::Empty,
        request_id = tracing::field::Empty,
        trace_id = tracing::field::Empty,
        outcome = tracing::field::Empty,
    );
    set_remote_parent(&span, parent);
    span
}

/// Span covering one durable runtime event or replay consumer.
#[must_use]
pub fn consumer_span(operation: &str, parent: &TraceHeaders) -> tracing::Span {
    let operation = bounded_attribute(operation, MAX_ATTRIBUTE_BYTES);
    let span = tracing::info_span!(
        "maestro.runtime.consume",
        otel.name = "maestro.runtime.consume",
        otel.kind = "consumer",
        maestro.operation = operation.as_str(),
        outcome = tracing::field::Empty,
        error.type = tracing::field::Empty,
        duration_ms = tracing::field::Empty,
    );
    set_remote_parent(&span, parent);
    span
}

/// Span for an allowlisted internal EvalOps callback.  This is the only client
/// constructor used for Platform/Sandboxwich egress; provider clients stay
/// header-free.
#[must_use]
pub fn client_span(method: &str, route: &str, parent: &TraceHeaders) -> tracing::Span {
    let method = bounded_attribute(method, 32);
    let route = route_class(route);
    let span = tracing::info_span!(
        "http.client",
        otel.name = format!("{method} {route}"),
        otel.kind = "client",
        http.request.method = method.as_str(),
        http.route = route.as_str(),
        http.response.status_code = tracing::field::Empty,
        http.client.request.duration_ms = tracing::field::Empty,
        duration_ms = tracing::field::Empty,
        outcome = tracing::field::Empty,
        error.type = tracing::field::Empty,
    );
    set_remote_parent(&span, parent);
    span
}

/// Span covering one governed agent turn.
#[must_use]
pub fn turn_span(parent: Option<&TraceHeaders>) -> tracing::Span {
    let span = tracing::info_span!(
        "gen_ai.agent.run",
        gen_ai.operation.name = "invoke_agent",
        gen_ai.agent.name = "maestro",
        gen_ai.agent.run.id = tracing::field::Empty,
        outcome = tracing::field::Empty,
        error.type = tracing::field::Empty,
        duration_ms = tracing::field::Empty,
    );
    if let Some(parent) = parent {
        set_remote_parent(&span, parent);
    }
    span
}

/// Span covering one model request. Provider headers are intentionally not
/// injected by this constructor; providers are third-party egress.
#[must_use]
pub fn model_span(provider: &str, model: &str) -> tracing::Span {
    tracing::info_span!(
        "gen_ai.client.operation",
        otel.kind = "client",
        gen_ai.operation.name = "chat",
        gen_ai.provider.name = bounded_attribute(provider, MAX_ATTRIBUTE_BYTES),
        gen_ai.request.model = bounded_attribute(model, MAX_ATTRIBUTE_BYTES),
        gen_ai.response.model = tracing::field::Empty,
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        gen_ai.usage.cache_read.input_tokens = tracing::field::Empty,
        gen_ai.usage.cache_write.input_tokens = tracing::field::Empty,
        outcome = tracing::field::Empty,
        error.type = tracing::field::Empty,
        duration_ms = tracing::field::Empty,
    )
}

/// Span for one governed tool execution. Arguments and results stay in the
/// governed payload-reference path rather than ordinary attributes.
#[must_use]
pub fn tool_span(tool_name: &str) -> tracing::Span {
    tool_span_for_call(tool_name, None)
}

/// Span for one governed tool execution with its stable call identity when
/// the caller has one. Arguments and results stay in the governed
/// payload-reference path rather than ordinary attributes.
#[must_use]
pub fn tool_span_for_call(tool_name: &str, call_id: Option<&str>) -> tracing::Span {
    let span = tracing::info_span!(
        "evalops.tool.execute",
        otel.kind = "internal",
        gen_ai.operation.name = "execute_tool",
        gen_ai.tool.type = "function",
        gen_ai.tool.name = bounded_attribute(tool_name, MAX_ATTRIBUTE_BYTES),
        gen_ai.tool.call.id = tracing::field::Empty,
        evalops.tool.name = bounded_attribute(tool_name, MAX_ATTRIBUTE_BYTES),
        outcome = tracing::field::Empty,
        error.type = tracing::field::Empty,
        duration_ms = tracing::field::Empty,
    );
    if let Some(call_id) = call_id {
        span.record(
            "gen_ai.tool.call.id",
            bounded_attribute(call_id, MAX_ATTRIBUTE_BYTES).as_str(),
        );
    }
    span
}

/// Span for a human or policy approval wait.
#[must_use]
pub fn approval_span() -> tracing::Span {
    tracing::info_span!(
        "evalops.approval.wait",
        evalops.approval.kind = "tool",
        outcome = tracing::field::Empty,
        error.type = tracing::field::Empty,
        duration_ms = tracing::field::Empty,
    )
}

/// Span for a terminal turn classification.
#[must_use]
pub fn terminal_span(outcome: &str) -> tracing::Span {
    let span = tracing::info_span!(
        "evalops.turn.terminal",
        evalops.turn.outcome = bounded_attribute(outcome, 48),
        outcome = tracing::field::Empty,
        error.type = tracing::field::Empty,
        duration_ms = tracing::field::Empty,
    );
    span
}

pub fn record_http_response(span: &tracing::Span, status: u16, duration: Duration) {
    span.record("http.response.status_code", i64::from(status));
    span.record(
        "http.server.request.duration_ms",
        duration.as_secs_f64() * 1_000.0,
    );
    span.record("outcome", if status < 400 { "success" } else { "error" });
    if status >= 500 {
        let error_type = status.to_string();
        span.record("error.type", error_type.as_str());
        span.record("otel.status_code", "ERROR");
    }
}

pub fn record_outcome(
    span: &tracing::Span,
    outcome: &str,
    duration: Duration,
    error: Option<&str>,
) {
    span.record("outcome", bounded_attribute(outcome, 48).as_str());
    let duration_ms = duration.as_secs_f64() * 1_000.0;
    span.record("duration_ms", duration_ms);
    span.record("http.server.request.duration_ms", duration_ms);
    span.record("http.client.request.duration_ms", duration_ms);
    if let Some(error) = error {
        span.record("error.type", bounded_attribute(error, 96).as_str());
        span.record("otel.status_code", "ERROR");
    }
}

pub fn record_model_usage(
    span: &tracing::Span,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
) {
    span.record("gen_ai.usage.input_tokens", input_tokens);
    span.record("gen_ai.usage.output_tokens", output_tokens);
    span.record("gen_ai.usage.cache_read.input_tokens", cache_read_tokens);
    span.record("gen_ai.usage.cache_write.input_tokens", cache_write_tokens);
}

#[must_use]
pub fn trace_headers_for_span(span: &tracing::Span, fallback: &TraceHeaders) -> TraceHeaders {
    let context = span.context();
    let span = context.span();
    let span_context = span.span_context();
    if span_context.is_valid() {
        headers_from_span_context(span_context)
    } else if fallback.is_valid() {
        fallback.child()
    } else {
        TraceHeaders::default()
    }
}

#[must_use]
pub fn current_trace_headers() -> TraceHeaders {
    trace_headers_for_span(&tracing::Span::current(), &TraceHeaders::default())
}

/// Replace stale propagation headers with the currently active span.
pub fn inject_evalops_context(headers: &mut HeaderMap) {
    headers.remove(TRACEPARENT_HEADER);
    headers.remove(TRACESTATE_HEADER);
    let context = tracing::Span::current().context();
    if context.span().span_context().is_valid() {
        TraceContextPropagator::new().inject_context(&context, &mut HeaderInjector(headers));
    }
}

/// Inject an explicit span context into an internal request.
pub fn inject_span_context(span: &tracing::Span, headers: &mut HeaderMap) {
    headers.remove(TRACEPARENT_HEADER);
    headers.remove(TRACESTATE_HEADER);
    let context = span.context();
    if context.span().span_context().is_valid() {
        TraceContextPropagator::new().inject_context(&context, &mut HeaderInjector(headers));
    }
}

#[must_use]
pub fn telemetry_resource(
    service_name: &'static str,
    version: &'static str,
    environment: &'static str,
) -> Resource {
    Resource::builder_empty()
        .with_attributes([
            KeyValue::new("service.name", service_name),
            KeyValue::new("service.version", version),
            KeyValue::new("deployment.environment.name", environment),
        ])
        .build()
}

/// Process-wide logs plus optional OTLP trace export.
pub struct TelemetryGuard {
    provider: SdkTracerProvider,
    shutdown: AtomicBool,
}

impl TelemetryGuard {
    /// Exporter configuration is fail-open: malformed exporter settings never
    /// prevent a native runtime from starting.
    #[must_use]
    pub fn init(config: TelemetryConfig) -> Self {
        let environment = first_nonempty_env(&["OTEL_DEPLOYMENT_ENVIRONMENT_NAME", "ENVIRONMENT"])
            .unwrap_or_else(|| config.environment.to_owned());
        let service_version = first_nonempty_env(&["OTEL_SERVICE_VERSION"])
            .unwrap_or_else(|| config.service_version.to_owned());
        let instance_id = first_nonempty_env(&["OTEL_SERVICE_INSTANCE_ID", "HOSTNAME"])
            .unwrap_or_else(|| format!("{}-{}", config.service_name, Uuid::now_v7()));
        let sampler = resolve_sampler().unwrap_or_else(|error| {
            eprintln!(
                "{} OTLP sampler disabled after configuration error: {error}",
                config.service_name
            );
            Sampler::AlwaysOff
        });
        let resource = Resource::builder_empty()
            .with_attributes([
                KeyValue::new("service.name", config.service_name),
                KeyValue::new("service.version", service_version),
                KeyValue::new("service.instance.id", instance_id),
                KeyValue::new("deployment.environment.name", environment),
            ])
            .build();
        let mut builder = SdkTracerProvider::builder()
            .with_sampler(sampler)
            .with_resource(resource);
        if let Some(endpoint) = trace_export_endpoint() {
            let headers = first_nonempty_env(&["OTEL_EXPORTER_OTLP_HEADERS"])
                .map(|value| parse_headers(&value))
                .unwrap_or_default();
            match SpanExporter::builder()
                .with_http()
                .with_endpoint(endpoint)
                .with_timeout(EXPORT_TIMEOUT)
                .with_headers(headers)
                .build()
            {
                Ok(exporter) => builder = builder.with_batch_exporter(exporter),
                Err(error) => eprintln!(
                    "{} OTLP exporter disabled after configuration error: {error}",
                    config.service_name
                ),
            }
        }
        let provider = builder.build();
        global::set_tracer_provider(provider.clone());
        global::set_text_map_propagator(TraceContextPropagator::new());
        let tracer = provider.tracer(config.service_name);
        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(config.default_filter));
        if let Err(error) = tracing_subscriber::registry()
            .with(filter)
            // Native hosted-runner stdout is a machine-readable protocol
            // stream; diagnostics must stay on stderr so they cannot corrupt
            // startup or shutdown JSON frames.
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .with(tracing_opentelemetry::layer().with_tracer(tracer))
            .try_init()
        {
            eprintln!(
                "{} tracing was already initialized: {error}",
                config.service_name
            );
        }
        Self {
            provider,
            shutdown: AtomicBool::new(false),
        }
    }

    fn shutdown(&self) -> Result<()> {
        if self.shutdown.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let flush_error = self.provider.force_flush().err();
        let shutdown_error = self.provider.shutdown_with_timeout(SHUTDOWN_TIMEOUT).err();
        if let Some(error) = flush_error {
            anyhow::bail!("OTLP trace flush failed: {error}");
        }
        if let Some(error) = shutdown_error {
            anyhow::bail!("OTLP trace shutdown failed: {error}");
        }
        Ok(())
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            eprintln!("{error}");
        }
    }
}

/// Alias retained for callers that prefer the shorter name.
pub type Telemetry = TelemetryGuard;

fn set_remote_parent(span: &tracing::Span, parent: &TraceHeaders) {
    let headers = parent.header_map();
    let context = TraceContextPropagator::new().extract(&HeaderExtractor(&headers));
    if context.span().span_context().is_valid() {
        let _ = span.set_parent(context);
    }
}

fn headers_from_span_context(context: &SpanContext) -> TraceHeaders {
    TraceHeaders {
        traceparent: format!(
            "00-{}-{}-{:02x}",
            context.trace_id(),
            context.span_id(),
            context.trace_flags().to_u8()
        ),
        tracestate: context.trace_state().header(),
    }
}

fn normalize_traceparent(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.len() > MAX_TRACEPARENT_BYTES {
        return None;
    }
    let mut parts = value.split('-');
    let version = parts.next()?;
    let trace_id = parts.next()?;
    let span_id = parts.next()?;
    let flags = parts.next()?;
    if parts.next().is_some()
        || version.len() != 2
        || version.eq_ignore_ascii_case("ff")
        || trace_id.len() != 32
        || span_id.len() != 16
        || flags.len() != 2
        || !is_hex(version)
        || !is_hex(trace_id)
        || !is_hex(span_id)
        || !is_hex(flags)
        || trace_id.bytes().all(|byte| byte == b'0')
        || span_id.bytes().all(|byte| byte == b'0')
    {
        return None;
    }
    Some(format!(
        "{}-{}-{}-{}",
        version.to_ascii_lowercase(),
        trace_id.to_ascii_lowercase(),
        span_id.to_ascii_lowercase(),
        flags.to_ascii_lowercase()
    ))
}

fn normalize_tracestate(value: Option<&str>) -> String {
    let value = value.map(str::trim).unwrap_or_default();
    if value.is_empty()
        || value.len() > MAX_TRACESTATE_BYTES
        || opentelemetry::trace::TraceState::from_str(value).is_err()
    {
        String::new()
    } else {
        value.to_owned()
    }
}

fn resolve_sampler() -> Result<Sampler> {
    let Some(name) = first_nonempty_env(&["OTEL_TRACES_SAMPLER"]) else {
        return Ok(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
            DEFAULT_SAMPLE_RATE,
        ))));
    };
    let ratio = || -> Result<f64> {
        let value = first_nonempty_env(&["OTEL_TRACES_SAMPLER_ARG"])
            .unwrap_or_else(|| DEFAULT_SAMPLE_RATE.to_string());
        let rate = value
            .parse::<f64>()
            .map_err(|_| anyhow::anyhow!("OTEL_TRACES_SAMPLER_ARG must be a number from 0 to 1"))?;
        if !(0.0..=1.0).contains(&rate) {
            anyhow::bail!("OTEL_TRACES_SAMPLER_ARG must be a number from 0 to 1");
        }
        Ok(rate)
    };
    match name.to_ascii_lowercase().as_str() {
        "always_on" => Ok(Sampler::AlwaysOn),
        "always_off" => Ok(Sampler::AlwaysOff),
        "traceidratio" => Ok(Sampler::TraceIdRatioBased(ratio()?)),
        "parentbased_always_on" => Ok(Sampler::ParentBased(Box::new(Sampler::AlwaysOn))),
        "parentbased_always_off" => Ok(Sampler::ParentBased(Box::new(Sampler::AlwaysOff))),
        "parentbased_traceidratio" => Ok(Sampler::ParentBased(Box::new(
            Sampler::TraceIdRatioBased(ratio()?),
        ))),
        _ => anyhow::bail!(
            "OTEL_TRACES_SAMPLER must be always_on, always_off, traceidratio, parentbased_always_on, parentbased_always_off, or parentbased_traceidratio"
        ),
    }
}

fn parse_headers(value: &str) -> HashMap<String, String> {
    value
        .split(',')
        .filter_map(|item| item.split_once('='))
        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
        .filter(|(key, value)| !key.is_empty() && !value.is_empty())
        .collect()
}

fn trace_export_endpoint() -> Option<String> {
    if let Some(endpoint) = first_nonempty_env(&["OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"]) {
        return Some(endpoint);
    }
    first_nonempty_env(&["OTEL_EXPORTER_OTLP_ENDPOINT"]).map(|endpoint| {
        if endpoint.ends_with("/v1/traces") {
            endpoint
        } else {
            format!("{}/v1/traces", endpoint.trim_end_matches('/'))
        }
    })
}

fn first_nonempty_env(keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| env::var(key).ok().map(|value| value.trim().to_owned()))
        .filter(|value| !value.is_empty())
}

fn bounded_attribute(value: &str, max: usize) -> String {
    value
        .trim()
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '_' | '-' | '.' | '/' | ':' | ' ')
        })
        .take(max)
        .collect()
}

fn is_hex(value: &str) -> bool {
    value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn new_trace_id() -> String {
    hex_bytes(&Uuid::now_v7().into_bytes())
}

fn new_span_id() -> String {
    hex_bytes(&Uuid::now_v7().into_bytes()[8..])
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(result, "{byte:02x}");
    }
    result
}

struct HeaderExtractor<'a>(&'a HeaderMap);

impl opentelemetry::propagation::Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|key| key.as_str()).collect()
    }
}

struct HeaderInjector<'a>(&'a mut HeaderMap);

impl opentelemetry::propagation::Injector for HeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let (Ok(key), Ok(value)) = (
            HeaderName::from_bytes(key.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            self.0.insert(key, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{SpanId, SpanKind};
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
    use std::time::Instant;

    const PARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[test]
    fn batch_otlp_exporter_delivers_a_span_over_http() {
        use opentelemetry::trace::{Span as _, Tracer as _};
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("collector listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let endpoint = format!("http://{}/v1/traces", listener.local_addr().unwrap());
        let collector = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "no OTLP request arrived");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("collector accept failed: {error}"),
                }
            };
            // On macOS an accepted socket can inherit O_NONBLOCK from the
            // listener. The collector wants a bounded blocking read; leaving
            // the inherited flag set turns an ordinary scheduling gap into a
            // spurious WouldBlock failure before the exporter writes headers.
            stream
                .set_nonblocking(false)
                .expect("blocking collector stream");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 4096];
            loop {
                let count = stream.read(&mut chunk).expect("OTLP request bytes");
                assert!(count > 0, "incomplete OTLP request");
                request.extend_from_slice(&chunk[..count]);
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .expect("OTLP content length")
                        .parse()
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        assert!(headers.starts_with("post /v1/traces http/1.1"));
                        assert!(headers.contains("content-type: application/x-protobuf"));
                        assert!(length > 0, "exported span payload must not be empty");
                        break;
                    }
                }
                assert!(request.len() < 65536, "unexpectedly large test trace");
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
        });
        let exporter = SpanExporter::builder()
            .with_http()
            .with_endpoint(endpoint)
            .with_timeout(Duration::from_secs(2))
            .build()
            .expect("configured HTTP exporter");
        let provider = SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .build();
        provider
            .tracer("collector-regression")
            .start("test-span")
            .end();
        provider.force_flush().expect("batch export completed");
        provider.shutdown().expect("exporter shutdown");
        collector.join().expect("collector accepted OTLP trace");
    }

    fn harness(
        service_name: &'static str,
        version: &'static str,
        environment: &'static str,
    ) -> (
        InMemorySpanExporter,
        SdkTracerProvider,
        impl tracing::Subscriber + Send + Sync,
    ) {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .with_resource(telemetry_resource(service_name, version, environment))
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("maestro-test")));
        (exporter, provider, subscriber)
    }

    #[test]
    fn platform_parent_creates_maestro_child() {
        let (exporter, provider, subscriber) = harness("maestro-runtime", "1.2.3", "test");
        let parent = TraceHeaders::from_values(Some(PARENT), Some("evalops=maestro"));
        tracing::subscriber::with_default(subscriber, || {
            let span = server_span("POST", "/api/chat?tenant=secret", &parent);
            let _entered = span.enter();
        });
        provider.force_flush().expect("flush");
        let spans = exporter.get_finished_spans().expect("spans");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].parent_span_id.to_string(), "00f067aa0ba902b7");
        assert!(spans[0].parent_span_is_remote);
        assert_eq!(spans[0].span_kind, SpanKind::Server);
        assert_eq!(spans[0].name.as_ref(), "POST /api/chat");
    }

    #[test]
    fn invalid_context_creates_safe_root() {
        let (exporter, provider, subscriber) = harness("maestro-runtime", "1.2.3", "test");
        let parent = TraceHeaders::from_values(Some("prompt=never-export"), Some("secret=value"));
        tracing::subscriber::with_default(subscriber, || {
            let span = server_span("GET", "/healthz", &parent);
            let _entered = span.enter();
        });
        provider.force_flush().expect("flush");
        let spans = exporter.get_finished_spans().expect("spans");
        assert_eq!(spans[0].parent_span_id, SpanId::INVALID);
        let attrs = spans[0]
            .attributes
            .iter()
            .map(|attribute| format!("{}={}", attribute.key, attribute.value))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!attrs.contains("prompt=never-export"));
        assert!(!attrs.contains("secret=value"));
    }

    #[test]
    fn otlp_resource_identifies_maestro() {
        let resource = telemetry_resource("maestro-runtime", "1.2.3", "staging");
        for (key, expected) in [
            ("service.name", "maestro-runtime"),
            ("service.version", "1.2.3"),
            ("deployment.environment.name", "staging"),
        ] {
            assert_eq!(
                resource
                    .get(&opentelemetry::Key::new(key))
                    .expect("resource attribute")
                    .to_string(),
                expected
            );
        }
    }

    #[test]
    fn shutdown_flush_is_bounded() {
        let provider = SdkTracerProvider::builder().build();
        let started = Instant::now();
        let _ = provider.shutdown_with_timeout(Duration::from_millis(50));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn injected_context_uses_active_child() {
        let (_exporter, _provider, subscriber) = harness("maestro-runtime", "1.2.3", "test");
        let parent = TraceHeaders::from_values(Some(PARENT), None);
        tracing::subscriber::with_default(subscriber, || {
            let span = server_span("POST", "/api/chat", &parent);
            let child = trace_headers_for_span(&span, &parent);
            assert_eq!(child.trace_id(), Some("4bf92f3577b34da6a3ce929d0e0e4736"));
            assert_ne!(child.span_id(), Some("00f067aa0ba902b7"));
        });
    }

    #[test]
    fn operation_spans_have_stable_names_and_no_content_fields() {
        let (exporter, provider, subscriber) = harness("maestro-runtime", "1.2.3", "test");
        tracing::subscriber::with_default(subscriber, || {
            let turn = turn_span(None);
            let _turn = turn.enter();
            let model = model_span("openai", "gpt-5");
            let _model = model.enter();
            let tool = tool_span_for_call("shell", Some("call-123"));
            let _tool = tool.enter();
            let approval = approval_span();
            let _approval = approval.enter();
            let terminal = terminal_span("success");
            let _terminal = terminal.enter();
        });
        provider.force_flush().expect("flush");
        let spans = exporter.get_finished_spans().expect("spans");
        let names = spans
            .iter()
            .map(|span| span.name.as_ref())
            .collect::<Vec<_>>();
        assert!(names.contains(&"gen_ai.agent.run"));
        assert!(names.contains(&"gen_ai.client.operation"));
        assert!(names.contains(&"evalops.tool.execute"));
        assert!(names.contains(&"evalops.approval.wait"));
        assert!(names.contains(&"evalops.turn.terminal"));
        assert!(
            names
                .iter()
                .all(|name| !name.contains("prompt") && !name.contains("secret"))
        );
        let model = spans
            .iter()
            .find(|span| span.name.as_ref() == "gen_ai.client.operation")
            .expect("model span");
        let model_attributes = model
            .attributes
            .iter()
            .map(|attribute| (attribute.key.as_str(), attribute.value.to_string()))
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(model.span_kind, SpanKind::Client);
        assert_eq!(
            model_attributes
                .get("gen_ai.operation.name")
                .map(String::as_str),
            Some("chat")
        );
        assert_eq!(
            model_attributes
                .get("gen_ai.provider.name")
                .map(String::as_str),
            Some("openai")
        );
        assert_eq!(
            model_attributes
                .get("gen_ai.request.model")
                .map(String::as_str),
            Some("gpt-5")
        );
        let tool = spans
            .iter()
            .find(|span| span.name.as_ref() == "evalops.tool.execute")
            .expect("tool span");
        let tool_attributes = tool
            .attributes
            .iter()
            .map(|attribute| (attribute.key.as_str(), attribute.value.to_string()))
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(tool.span_kind, SpanKind::Internal);
        assert_eq!(
            tool_attributes
                .get("gen_ai.operation.name")
                .map(String::as_str),
            Some("execute_tool")
        );
        assert_eq!(
            tool_attributes.get("gen_ai.tool.name").map(String::as_str),
            Some("shell")
        );
        assert_eq!(
            tool_attributes
                .get("gen_ai.tool.call.id")
                .map(String::as_str),
            Some("call-123")
        );
    }
}
