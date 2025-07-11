#![cfg(not(doctest))]
// unfortunately the proto code includes comments from the google proto files
// that are interpreted as "doc tests" and will fail to build.
// When this PR is merged we should be able to remove this attribute:
// https://github.com/danburkert/prost/pull/291
#![allow(
    clippy::doc_lazy_continuation,
    deprecated,
    rustdoc::bare_urls,
    rustdoc::broken_intra_doc_links,
    rustdoc::invalid_rust_codeblocks
)]

use std::{
    borrow::Cow,
    collections::HashMap,
    fmt,
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, RwLock,
    },
    time::{Duration, Instant},
};

use futures_util::stream::StreamExt;
use opentelemetry::{otel_error, trace::SpanId, Key, KeyValue, Value};
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::{
    trace::{SpanData, SpanExporter},
    Resource,
};
use opentelemetry_semantic_conventions as semconv;
use thiserror::Error;
#[cfg(feature = "gcp-authorizer")]
use tonic::metadata::MetadataValue;
#[cfg(any(
    feature = "tls-ring",
    feature = "tls-native-roots",
    feature = "tls-webpki-roots"
))]
use tonic::transport::ClientTlsConfig;
use tonic::{transport::Channel, Code, Request};

#[allow(clippy::derive_partial_eq_without_eq)] // tonic doesn't derive Eq for generated types
#[allow(clippy::doc_overindented_list_items)]
pub mod proto;

#[cfg(feature = "propagator")]
pub mod google_trace_context_propagator;

use proto::devtools::cloudtrace::v2::span::time_event::Annotation;
use proto::devtools::cloudtrace::v2::span::{
    Attributes, Link, Links, SpanKind, TimeEvent, TimeEvents,
};
use proto::devtools::cloudtrace::v2::trace_service_client::TraceServiceClient;
use proto::devtools::cloudtrace::v2::{
    AttributeValue, BatchWriteSpansRequest, Span, TruncatableString,
};
use proto::logging::v2::{
    log_entry::Payload, logging_service_v2_client::LoggingServiceV2Client, LogEntry,
    LogEntrySourceLocation, WriteLogEntriesRequest,
};
use proto::rpc::Status;

/// Exports opentelemetry tracing spans to Google StackDriver.
///
/// As of the time of this writing, the opentelemetry crate exposes no link information
/// so this struct does not send link information.
#[derive(Clone)]
pub struct StackDriverExporter {
    tx: futures_channel::mpsc::Sender<Vec<SpanData>>,
    pending_count: Arc<AtomicUsize>,
    maximum_shutdown_duration: Duration,
    resource: Arc<RwLock<Option<Resource>>>,
}

impl StackDriverExporter {
    pub fn builder() -> Builder {
        Builder::default()
    }

    pub fn pending_count(&self) -> usize {
        self.pending_count.load(Ordering::Relaxed)
    }
}

impl SpanExporter for StackDriverExporter {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        match self.tx.clone().try_send(batch) {
            Err(e) => Err(OTelSdkError::InternalFailure(format!("{e:?}"))),
            Ok(()) => {
                self.pending_count.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        }
    }

    fn shutdown(&mut self) -> OTelSdkResult {
        let start = Instant::now();
        while (Instant::now() - start) < self.maximum_shutdown_duration && self.pending_count() > 0
        {
            std::thread::yield_now();
            // Spin for a bit and give the inner export some time to upload, with a timeout.
        }
        Ok(())
    }

    fn set_resource(&mut self, resource: &Resource) {
        match self.resource.write() {
            Ok(mut guard) => *guard = Some(resource.clone()),
            Err(poisoned) => *poisoned.into_inner() = Some(resource.clone()),
        }
    }
}

impl fmt::Debug for StackDriverExporter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[allow(clippy::unneeded_field_pattern)]
        let Self {
            tx: _,
            pending_count,
            maximum_shutdown_duration,
            resource: _,
        } = self;
        f.debug_struct("StackDriverExporter")
            .field("tx", &"(elided)")
            .field("pending_count", pending_count)
            .field("maximum_shutdown_duration", maximum_shutdown_duration)
            .finish()
    }
}

/// Helper type to build a `StackDriverExporter`.
#[derive(Clone, Default)]
pub struct Builder {
    maximum_shutdown_duration: Option<Duration>,
    num_concurrent_requests: Option<usize>,
    log_context: Option<LogContext>,
}

impl Builder {
    /// Set the maximum shutdown duration to export all the remaining data.
    ///
    /// If not set, defaults to 5 seconds.
    pub fn maximum_shutdown_duration(mut self, duration: Duration) -> Self {
        self.maximum_shutdown_duration = Some(duration);
        self
    }

    /// Set the number of concurrent requests.
    ///
    /// If `num_concurrent_requests` is set to `0` or `None` then no limit is enforced.
    pub fn num_concurrent_requests(mut self, num_concurrent_requests: usize) -> Self {
        self.num_concurrent_requests = Some(num_concurrent_requests);
        self
    }

    /// Enable writing log entries with the given `log_context`.
    pub fn log_context(mut self, log_context: LogContext) -> Self {
        self.log_context = Some(log_context);
        self
    }

    pub async fn build<A: Authorizer>(
        self,
        authenticator: A,
    ) -> Result<(StackDriverExporter, impl Future<Output = ()>), Error>
    where
        Error: From<A::Error>,
    {
        let Self {
            maximum_shutdown_duration,
            num_concurrent_requests,
            log_context,
        } = self;
        let uri = http::uri::Uri::from_static("https://cloudtrace.googleapis.com:443");

        #[cfg(any(
            feature = "tls-ring",
            feature = "tls-native-roots",
            feature = "tls-webpki-roots"
        ))]
        let tls_config = ClientTlsConfig::new().with_enabled_roots();

        let trace_channel_builder = Channel::builder(uri);
        #[cfg(any(
            feature = "tls-ring",
            feature = "tls-native-roots",
            feature = "tls-webpki-roots"
        ))]
        let trace_channel_builder = trace_channel_builder
            .tls_config(tls_config.clone())
            .map_err(|e| Error::Transport(e.into()))?;

        let trace_channel = trace_channel_builder
            .connect()
            .await
            .map_err(|e| Error::Transport(e.into()))?;

        let log_client = match log_context {
            Some(log_context) => {
                let log_channel_builder = Channel::builder(http::uri::Uri::from_static(
                    "https://logging.googleapis.com:443",
                ));
                #[cfg(any(
                    feature = "tls-ring",
                    feature = "tls-native-roots",
                    feature = "tls-webpki-roots"
                ))]
                let log_channel_builder = log_channel_builder
                    .tls_config(tls_config)
                    .map_err(|e| Error::Transport(e.into()))?;

                let log_channel = log_channel_builder
                    .connect()
                    .await
                    .map_err(|e| Error::Transport(e.into()))?;

                Some(LogClient {
                    client: LoggingServiceV2Client::new(log_channel),
                    context: Arc::new(InternalLogContext::from(log_context)),
                })
            }
            None => None,
        };

        let (tx, rx) = futures_channel::mpsc::channel(64);
        let pending_count = Arc::new(AtomicUsize::new(0));
        let scopes = Arc::new(match log_client {
            Some(_) => vec![TRACE_APPEND, LOGGING_WRITE],
            None => vec![TRACE_APPEND],
        });

        let count_clone = pending_count.clone();
        let resource = Arc::new(RwLock::new(None));
        let ctx_resource = resource.clone();
        let future = async move {
            let trace_client = TraceServiceClient::new(trace_channel);
            let authorizer = &authenticator;
            let log_client = log_client.clone();
            rx.for_each_concurrent(num_concurrent_requests, move |batch| {
                let trace_client = trace_client.clone();
                let log_client = log_client.clone();
                let pending_count = count_clone.clone();
                let scopes = scopes.clone();
                let resource = ctx_resource.clone();
                ExporterContext {
                    trace_client,
                    log_client,
                    authorizer,
                    pending_count,
                    scopes,
                    resource,
                }
                .export(batch)
            })
            .await
        };

        let exporter = StackDriverExporter {
            tx,
            pending_count,
            maximum_shutdown_duration: maximum_shutdown_duration
                .unwrap_or_else(|| Duration::from_secs(5)),
            resource,
        };

        Ok((exporter, future))
    }
}

struct ExporterContext<'a, A> {
    trace_client: TraceServiceClient<Channel>,
    log_client: Option<LogClient>,
    authorizer: &'a A,
    pending_count: Arc<AtomicUsize>,
    scopes: Arc<Vec<&'static str>>,
    resource: Arc<RwLock<Option<Resource>>>,
}

impl<A: Authorizer> ExporterContext<'_, A>
where
    Error: From<A::Error>,
{
    async fn export(mut self, batch: Vec<SpanData>) {
        use proto::devtools::cloudtrace::v2::span::time_event::Value;

        let mut entries = Vec::new();
        let mut spans = Vec::with_capacity(batch.len());
        for span in batch {
            let trace_id = hex::encode(span.span_context.trace_id().to_bytes());
            let span_id = hex::encode(span.span_context.span_id().to_bytes());
            let time_event = match &self.log_client {
                None => span
                    .events
                    .into_iter()
                    .map(|event| TimeEvent {
                        time: Some(event.timestamp.into()),
                        value: Some(Value::Annotation(Annotation {
                            description: Some(to_truncate(event.name.into_owned())),
                            ..Default::default()
                        })),
                    })
                    .collect(),
                Some(client) => {
                    entries.extend(span.events.into_iter().map(|event| {
                        let (mut level, mut target, mut labels) =
                            (LogSeverity::Default, None, HashMap::default());
                        for kv in event.attributes {
                            match kv.key.as_str() {
                                "level" => {
                                    level = match kv.value.as_str().as_ref() {
                                        "DEBUG" | "TRACE" => LogSeverity::Debug,
                                        "INFO" => LogSeverity::Info,
                                        "WARN" => LogSeverity::Warning,
                                        "ERROR" => LogSeverity::Error,
                                        _ => LogSeverity::Default, // tracing::Level is limited to the above 5
                                    }
                                }
                                "target" => target = Some(kv.value.as_str().into_owned()),
                                key => {
                                    labels.insert(key.to_owned(), kv.value.as_str().into_owned());
                                }
                            }
                        }
                        let project_id = self.authorizer.project_id();
                        let log_id = &client.context.log_id;
                        LogEntry {
                            log_name: format!("projects/{project_id}/logs/{log_id}"),
                            resource: Some(client.context.resource.clone()),
                            severity: level as i32,
                            timestamp: Some(event.timestamp.into()),
                            labels,
                            trace: format!("projects/{project_id}/traces/{trace_id}"),
                            span_id: span_id.clone(),
                            source_location: target.map(|target| LogEntrySourceLocation {
                                file: String::new(),
                                line: 0,
                                function: target,
                            }),
                            payload: Some(Payload::TextPayload(event.name.into_owned())),
                            // severity, source_location, text_payload
                            ..Default::default()
                        }
                    }));

                    vec![]
                }
            };

            let resource = self.resource.read().ok();
            let attributes = match resource {
                Some(resource) => Attributes::new(span.attributes, resource.as_ref()),
                None => Attributes::new(span.attributes, None),
            };

            spans.push(Span {
                name: format!(
                    "projects/{}/traces/{}/spans/{}",
                    self.authorizer.project_id(),
                    hex::encode(span.span_context.trace_id().to_bytes()),
                    hex::encode(span.span_context.span_id().to_bytes())
                ),
                display_name: Some(to_truncate(span.name.into_owned())),
                span_id: hex::encode(span.span_context.span_id().to_bytes()),
                // From the API docs: If this is a root span,
                // then this field must be empty.
                parent_span_id: match span.parent_span_id {
                    SpanId::INVALID => "".to_owned(),
                    _ => hex::encode(span.parent_span_id.to_bytes()),
                },
                start_time: Some(span.start_time.into()),
                end_time: Some(span.end_time.into()),
                attributes: Some(attributes),
                time_events: Some(TimeEvents {
                    time_event,
                    ..Default::default()
                }),
                links: transform_links(&span.links),
                status: status(span.status),
                span_kind: SpanKind::from(span.span_kind) as i32,
                ..Default::default()
            });
        }

        let mut req = Request::new(BatchWriteSpansRequest {
            name: format!("projects/{}", self.authorizer.project_id()),
            spans,
        });

        self.pending_count.fetch_sub(1, Ordering::Relaxed);
        if let Err(e) = self.authorizer.authorize(&mut req, &self.scopes).await {
            otel_error!(name: "ExportAuthorizeError", error = format!("{e:?}"));
        } else if let Err(e) = self.trace_client.batch_write_spans(req).await {
            otel_error!(name: "ExportTransportError", error = format!("{e:?}"));
        }

        let client = match &mut self.log_client {
            Some(client) => client,
            None => return,
        };

        let mut req = Request::new(WriteLogEntriesRequest {
            log_name: format!(
                "projects/{}/logs/{}",
                self.authorizer.project_id(),
                client.context.log_id,
            ),
            entries,
            dry_run: false,
            labels: HashMap::default(),
            partial_success: true,
            resource: None,
        });

        if let Err(e) = self.authorizer.authorize(&mut req, &self.scopes).await {
            otel_error!(name: "ExportAuthorizeError", error = format!("{e:?}"));
        } else if let Err(e) = client.client.write_log_entries(req).await {
            otel_error!(name: "ExportTransportError", error =  format!("{e:?}"));
        }
    }
}

#[cfg(feature = "gcp-authorizer")]
pub struct GcpAuthorizer {
    provider: Arc<dyn gcp_auth::TokenProvider>,
    project_id: Arc<str>,
}

#[cfg(feature = "gcp-authorizer")]
impl GcpAuthorizer {
    pub async fn new() -> Result<Self, Error> {
        let provider = gcp_auth::provider()
            .await
            .map_err(|e| Error::Authorizer(e.into()))?;

        let project_id = provider
            .project_id()
            .await
            .map_err(|e| Error::Authorizer(e.into()))?;

        Ok(Self {
            provider,
            project_id,
        })
    }
    pub fn from_gcp_auth(provider: Arc<dyn gcp_auth::TokenProvider>, project_id: Arc<str>) -> Self {
        Self {
            provider,
            project_id,
        }
    }
}

#[cfg(feature = "gcp-authorizer")]
impl Authorizer for GcpAuthorizer {
    type Error = Error;

    fn project_id(&self) -> &str {
        &self.project_id
    }

    async fn authorize<T: Send + Sync>(
        &self,
        req: &mut Request<T>,
        scopes: &[&str],
    ) -> Result<(), Self::Error> {
        let token = self
            .provider
            .token(scopes)
            .await
            .map_err(|e| Error::Authorizer(e.into()))?;

        req.metadata_mut().insert(
            "authorization",
            MetadataValue::try_from(format!("Bearer {}", token.as_str())).unwrap(),
        );

        Ok(())
    }
}

pub trait Authorizer: Sync + Send + 'static {
    type Error: std::error::Error + fmt::Debug + Send + Sync;

    fn project_id(&self) -> &str;
    fn authorize<T: Send + Sync>(
        &self,
        request: &mut Request<T>,
        scopes: &[&str],
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

impl From<Value> for AttributeValue {
    fn from(v: Value) -> AttributeValue {
        use proto::devtools::cloudtrace::v2::attribute_value;
        let new_value = match v {
            Value::Bool(v) => attribute_value::Value::BoolValue(v),
            Value::F64(v) => attribute_value::Value::StringValue(to_truncate(v.to_string())),
            Value::I64(v) => attribute_value::Value::IntValue(v),
            Value::String(v) => attribute_value::Value::StringValue(to_truncate(v.to_string())),
            Value::Array(_) => attribute_value::Value::StringValue(to_truncate(v.to_string())),
            _ => attribute_value::Value::StringValue(to_truncate("".to_string())),
        };
        AttributeValue {
            value: Some(new_value),
        }
    }
}

fn to_truncate(s: String) -> TruncatableString {
    TruncatableString {
        value: s,
        ..Default::default()
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("authorizer error: {0}")]
    Authorizer(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(#[from] Box<dyn std::error::Error + Send + Sync>),
    #[error("tonic error: {0}")]
    Transport(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl opentelemetry_sdk::ExportError for Error {
    fn exporter_name(&self) -> &'static str {
        "stackdriver"
    }
}

/// As defined in https://cloud.google.com/logging/docs/reference/v2/rpc/google.logging.type#google.logging.type.LogSeverity.
enum LogSeverity {
    Default = 0,
    Debug = 100,
    Info = 200,
    Warning = 400,
    Error = 500,
}

#[derive(Clone)]
struct LogClient {
    client: LoggingServiceV2Client<Channel>,
    context: Arc<InternalLogContext>,
}

struct InternalLogContext {
    log_id: String,
    resource: proto::api::MonitoredResource,
}

#[derive(Clone)]
pub struct LogContext {
    pub log_id: String,
    pub resource: MonitoredResource,
}

impl From<LogContext> for InternalLogContext {
    fn from(cx: LogContext) -> Self {
        let mut labels = HashMap::default();
        let resource = match cx.resource {
            MonitoredResource::AppEngine {
                project_id,
                module_id,
                version_id,
                zone,
            } => {
                labels.insert("project_id".to_string(), project_id);
                if let Some(module_id) = module_id {
                    labels.insert("module_id".to_string(), module_id);
                }
                if let Some(version_id) = version_id {
                    labels.insert("version_id".to_string(), version_id);
                }
                if let Some(zone) = zone {
                    labels.insert("zone".to_string(), zone);
                }

                proto::api::MonitoredResource {
                    r#type: "gae_app".to_owned(),
                    labels,
                }
            }
            MonitoredResource::CloudFunction {
                project_id,
                function_name,
                region,
            } => {
                labels.insert("project_id".to_string(), project_id);
                if let Some(function_name) = function_name {
                    labels.insert("function_name".to_string(), function_name);
                }
                if let Some(region) = region {
                    labels.insert("region".to_string(), region);
                }

                proto::api::MonitoredResource {
                    r#type: "cloud_function".to_owned(),
                    labels,
                }
            }
            MonitoredResource::CloudRunJob {
                project_id,
                job_name,
                location,
            } => {
                labels.insert("project_id".to_string(), project_id);
                if let Some(job_name) = job_name {
                    labels.insert("job_name".to_string(), job_name);
                }
                if let Some(location) = location {
                    labels.insert("location".to_string(), location);
                }

                proto::api::MonitoredResource {
                    r#type: "cloud_run_job".to_owned(),
                    labels,
                }
            }
            MonitoredResource::CloudRunRevision {
                project_id,
                service_name,
                revision_name,
                location,
                configuration_name,
            } => {
                labels.insert("project_id".to_string(), project_id);
                if let Some(service_name) = service_name {
                    labels.insert("service_name".to_string(), service_name);
                }
                if let Some(revision_name) = revision_name {
                    labels.insert("revision_name".to_string(), revision_name);
                }
                if let Some(location) = location {
                    labels.insert("location".to_string(), location);
                }
                if let Some(configuration_name) = configuration_name {
                    labels.insert("configuration_name".to_string(), configuration_name);
                }

                proto::api::MonitoredResource {
                    r#type: "cloud_run_revision".to_owned(),
                    labels,
                }
            }

            MonitoredResource::ComputeEngine {
                project_id,
                instance_id,
                zone,
            } => {
                labels.insert("project_id".to_string(), project_id);
                if let Some(instance_id) = instance_id {
                    labels.insert("instance_id".to_string(), instance_id);
                }
                if let Some(zone) = zone {
                    labels.insert("zone".to_string(), zone);
                }

                proto::api::MonitoredResource {
                    r#type: "gce_instance".to_owned(),
                    labels,
                }
            }

            MonitoredResource::GenericNode {
                project_id,
                location,
                namespace,
                node_id,
            } => {
                labels.insert("project_id".to_string(), project_id);
                if let Some(location) = location {
                    labels.insert("location".to_string(), location);
                }
                if let Some(namespace) = namespace {
                    labels.insert("namespace".to_string(), namespace);
                }
                if let Some(node_id) = node_id {
                    labels.insert("node_id".to_string(), node_id);
                }

                proto::api::MonitoredResource {
                    r#type: "generic_node".to_owned(),
                    labels,
                }
            }
            MonitoredResource::GenericTask {
                project_id,
                location,
                namespace,
                job,
                task_id,
            } => {
                labels.insert("project_id".to_owned(), project_id);
                if let Some(location) = location {
                    labels.insert("location".to_owned(), location);
                }
                if let Some(namespace) = namespace {
                    labels.insert("namespace".to_owned(), namespace);
                }
                if let Some(job) = job {
                    labels.insert("job".to_owned(), job);
                }
                if let Some(task_id) = task_id {
                    labels.insert("task_id".to_owned(), task_id);
                }

                proto::api::MonitoredResource {
                    r#type: "generic_task".to_owned(),
                    labels,
                }
            }
            MonitoredResource::Global { project_id } => {
                labels.insert("project_id".to_owned(), project_id);
                proto::api::MonitoredResource {
                    r#type: "global".to_owned(),
                    labels,
                }
            }
            MonitoredResource::KubernetesEngine {
                project_id,
                cluster_name,
                location,
                pod_name,
                namespace_name,
                container_name,
            } => {
                labels.insert("project_id".to_string(), project_id);
                if let Some(cluster_name) = cluster_name {
                    labels.insert("cluster_name".to_string(), cluster_name);
                }
                if let Some(location) = location {
                    labels.insert("location".to_string(), location);
                }
                if let Some(pod_name) = pod_name {
                    labels.insert("pod_name".to_string(), pod_name);
                }
                if let Some(namespace_name) = namespace_name {
                    labels.insert("namespace_name".to_string(), namespace_name);
                }
                if let Some(container_name) = container_name {
                    labels.insert("container_name".to_string(), container_name);
                }

                proto::api::MonitoredResource {
                    r#type: "k8s_container".to_owned(),
                    labels,
                }
            }
        };

        Self {
            log_id: cx.log_id,
            resource,
        }
    }
}

/// A description of a `MonitoredResource`.
///
/// Possible values are listed in the [API documentation](https://cloud.google.com/logging/docs/api/v2/resource-list).
/// Please submit an issue or pull request if you want to use a resource type not listed here.
#[derive(Clone)]
pub enum MonitoredResource {
    AppEngine {
        project_id: String,
        module_id: Option<String>,
        version_id: Option<String>,
        zone: Option<String>,
    },
    CloudFunction {
        project_id: String,
        function_name: Option<String>,
        region: Option<String>,
    },
    CloudRunJob {
        project_id: String,
        job_name: Option<String>,
        location: Option<String>,
    },
    CloudRunRevision {
        project_id: String,
        service_name: Option<String>,
        revision_name: Option<String>,
        location: Option<String>,
        configuration_name: Option<String>,
    },
    ComputeEngine {
        project_id: String,
        instance_id: Option<String>,
        zone: Option<String>,
    },
    KubernetesEngine {
        project_id: String,
        location: Option<String>,
        cluster_name: Option<String>,
        namespace_name: Option<String>,
        pod_name: Option<String>,
        container_name: Option<String>,
    },
    GenericNode {
        project_id: String,
        location: Option<String>,
        namespace: Option<String>,
        node_id: Option<String>,
    },
    GenericTask {
        project_id: String,
        location: Option<String>,
        namespace: Option<String>,
        job: Option<String>,
        task_id: Option<String>,
    },
    Global {
        project_id: String,
    },
}

impl Attributes {
    /// Combines `EvictedHashMap` and `Resource` attributes into a maximum of 32.
    ///
    /// The `Resource` takes precedence over the `EvictedHashMap` attributes.
    fn new(attributes: Vec<KeyValue>, resource: Option<&Resource>) -> Self {
        let mut new = Self {
            dropped_attributes_count: 0,
            attribute_map: HashMap::with_capacity(Ord::min(
                MAX_ATTRIBUTES_PER_SPAN,
                attributes.len() + resource.map_or(0, |r| r.len()),
            )),
        };

        if let Some(resource) = resource {
            for (k, v) in resource.iter() {
                new.push(Cow::Borrowed(k), Cow::Borrowed(v));
            }
        }

        for kv in attributes {
            new.push(Cow::Owned(kv.key), Cow::Owned(kv.value));
        }

        new
    }

    fn push(&mut self, key: Cow<'_, Key>, value: Cow<'_, Value>) {
        if self.attribute_map.len() >= MAX_ATTRIBUTES_PER_SPAN {
            self.dropped_attributes_count += 1;
            return;
        }

        let key_str = key.as_str();
        if key_str.len() > 128 {
            self.dropped_attributes_count += 1;
            return;
        }

        for (otel_key, gcp_key) in KEY_MAP {
            if otel_key == key_str {
                self.attribute_map
                    .insert(gcp_key.to_owned(), value.into_owned().into());
                return;
            }
        }

        self.attribute_map.insert(
            match key {
                Cow::Owned(k) => k.to_string(),
                Cow::Borrowed(k) => k.to_string(),
            },
            value.into_owned().into(),
        );
    }
}

fn transform_links(links: &opentelemetry_sdk::trace::SpanLinks) -> Option<Links> {
    if links.is_empty() {
        return None;
    }

    Some(Links {
        dropped_links_count: links.dropped_count as i32,
        link: links
            .iter()
            .map(|link| Link {
                trace_id: hex::encode(link.span_context.trace_id().to_bytes()),
                span_id: hex::encode(link.span_context.span_id().to_bytes()),
                ..Default::default()
            })
            .collect(),
    })
}

// Map conventional OpenTelemetry keys to their GCP counterparts.
//
// https://cloud.google.com/trace/docs/trace-labels
const KEY_MAP: [(&str, &str); 19] = [
    (HTTP_PATH, GCP_HTTP_PATH),
    (semconv::attribute::HTTP_HOST, "/http/host"),
    ("http.request.header.host", "/http/host"),
    (semconv::attribute::HTTP_METHOD, "/http/method"),
    (semconv::attribute::HTTP_REQUEST_METHOD, "/http/method"),
    (semconv::attribute::HTTP_TARGET, "/http/path"),
    (semconv::attribute::URL_PATH, "/http/path"),
    (semconv::attribute::HTTP_URL, "/http/url"),
    (semconv::attribute::URL_FULL, "/http/url"),
    (semconv::attribute::HTTP_USER_AGENT, "/http/user_agent"),
    (semconv::attribute::USER_AGENT_ORIGINAL, "/http/user_agent"),
    (semconv::attribute::HTTP_STATUS_CODE, "/http/status_code"),
    // https://cloud.google.com/trace/docs/trace-labels#canonical-gke
    (
        semconv::attribute::HTTP_RESPONSE_STATUS_CODE,
        "/http/status_code",
    ),
    (
        semconv::attribute::K8S_CLUSTER_NAME,
        "g.co/r/k8s_container/cluster_name",
    ),
    (
        semconv::attribute::K8S_NAMESPACE_NAME,
        "g.co/r/k8s_container/namespace",
    ),
    (
        semconv::attribute::K8S_POD_NAME,
        "g.co/r/k8s_container/pod_name",
    ),
    (
        semconv::attribute::K8S_CONTAINER_NAME,
        "g.co/r/k8s_container/container_name",
    ),
    (semconv::trace::HTTP_ROUTE, "/http/route"),
    (HTTP_PATH, GCP_HTTP_PATH),
];

const HTTP_PATH: &str = "http.path";
const GCP_HTTP_PATH: &str = "/http/path";

impl From<opentelemetry::trace::SpanKind> for SpanKind {
    fn from(span_kind: opentelemetry::trace::SpanKind) -> Self {
        match span_kind {
            opentelemetry::trace::SpanKind::Client => SpanKind::Client,
            opentelemetry::trace::SpanKind::Server => SpanKind::Server,
            opentelemetry::trace::SpanKind::Producer => SpanKind::Producer,
            opentelemetry::trace::SpanKind::Consumer => SpanKind::Consumer,
            opentelemetry::trace::SpanKind::Internal => SpanKind::Internal,
        }
    }
}

fn status(value: opentelemetry::trace::Status) -> Option<Status> {
    match value {
        opentelemetry::trace::Status::Ok => Some(Status {
            code: Code::Ok as i32,
            message: "".to_owned(),
            details: vec![],
        }),
        opentelemetry::trace::Status::Unset => None,
        opentelemetry::trace::Status::Error { description } => Some(Status {
            code: Code::Unknown as i32,
            message: description.into(),
            details: vec![],
        }),
    }
}
const TRACE_APPEND: &str = "https://www.googleapis.com/auth/trace.append";
const LOGGING_WRITE: &str = "https://www.googleapis.com/auth/logging.write";
const MAX_ATTRIBUTES_PER_SPAN: usize = 32;

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::{KeyValue, Value};
    use opentelemetry_semantic_conventions as semcov;

    #[test]
    fn test_attributes_mapping() {
        let capacity = 10;
        let mut attributes = Vec::with_capacity(capacity);

        //	hostAttribute       = "http.host"
        attributes.push(KeyValue::new(
            semconv::attribute::HTTP_HOST,
            "example.com:8080",
        ));

        // 	methodAttribute     = "http.method"
        attributes.push(KeyValue::new(semcov::attribute::HTTP_METHOD, "POST"));

        // 	pathAttribute       = "http.path"
        attributes.push(KeyValue::new(HTTP_PATH, "/path/12314/?q=ddds#123"));

        // 	urlAttribute        = "http.url"
        attributes.push(KeyValue::new(
            semcov::attribute::HTTP_URL,
            "https://example.com:8080/webshop/articles/4?s=1",
        ));

        // 	userAgentAttribute  = "http.user_agent"
        attributes.push(KeyValue::new(
            semconv::attribute::HTTP_USER_AGENT,
            "CERN-LineMode/2.15 libwww/2.17b3",
        ));

        // 	statusCodeAttribute = "http.status_code"
        attributes.push(KeyValue::new(semcov::attribute::HTTP_STATUS_CODE, 200i64));

        // 	statusCodeAttribute = "http.route"
        attributes.push(KeyValue::new(
            semcov::trace::HTTP_ROUTE,
            "/webshop/articles/:article_id",
        ));

        // 	serviceAttribute    = "service.name"
        let resources = Resource::builder_empty()
            .with_attributes([KeyValue::new(
                semcov::resource::SERVICE_NAME,
                "Test Service Name",
            )])
            .build();

        let actual = Attributes::new(attributes, Some(&resources));
        assert_eq!(actual.attribute_map.len(), 8);
        assert_eq!(actual.dropped_attributes_count, 0);
        assert_eq!(
            actual.attribute_map.get("/http/host"),
            Some(&AttributeValue::from(Value::String(
                "example.com:8080".into()
            )))
        );
        assert_eq!(
            actual.attribute_map.get("/http/method"),
            Some(&AttributeValue::from(Value::String("POST".into()))),
        );
        assert_eq!(
            actual.attribute_map.get("/http/path"),
            Some(&AttributeValue::from(Value::String(
                "/path/12314/?q=ddds#123".into()
            ))),
        );
        assert_eq!(
            actual.attribute_map.get("/http/route"),
            Some(&AttributeValue::from(Value::String(
                "/webshop/articles/:article_id".into()
            ))),
        );
        assert_eq!(
            actual.attribute_map.get("/http/url"),
            Some(&AttributeValue::from(Value::String(
                "https://example.com:8080/webshop/articles/4?s=1".into(),
            ))),
        );
        assert_eq!(
            actual.attribute_map.get("/http/user_agent"),
            Some(&AttributeValue::from(Value::String(
                "CERN-LineMode/2.15 libwww/2.17b3".into()
            ))),
        );
        assert_eq!(
            actual.attribute_map.get("/http/status_code"),
            Some(&AttributeValue::from(Value::I64(200))),
        );
    }

    #[test]
    fn test_too_many() {
        let resources = Resource::builder_empty()
            .with_attributes([KeyValue::new(
                semconv::attribute::USER_AGENT_ORIGINAL,
                "Test Service Name UA",
            )])
            .build();
        let mut attributes = Vec::with_capacity(32);
        for i in 0..32 {
            attributes.push(KeyValue::new(
                format!("key{i}"),
                Value::String(format!("value{i}").into()),
            ));
        }

        let actual = Attributes::new(attributes, Some(&resources));
        assert_eq!(actual.attribute_map.len(), 32);
        assert_eq!(actual.dropped_attributes_count, 1);
        assert_eq!(
            actual.attribute_map.get("/http/user_agent"),
            Some(&AttributeValue::from(Value::String(
                "Test Service Name UA".into()
            ))),
        );
    }

    #[test]
    fn test_attributes_mapping_http_target() {
        let attributes = vec![KeyValue::new(
            semcov::attribute::HTTP_TARGET,
            "/path/12314/?q=ddds#123",
        )];

        //	hostAttribute       = "http.target"

        let resources = Resource::builder_empty().with_attributes([]).build();
        let actual = Attributes::new(attributes, Some(&resources));
        assert_eq!(actual.attribute_map.len(), 1);
        assert_eq!(actual.dropped_attributes_count, 0);
        assert_eq!(
            actual.attribute_map.get("/http/path"),
            Some(&AttributeValue::from(Value::String(
                "/path/12314/?q=ddds#123".into()
            ))),
        );
    }

    #[test]
    fn test_attributes_mapping_dropped_attributes_count() {
        let attributes = vec![KeyValue::new("answer", Value::I64(42)),KeyValue::new("long_attribute_key_dvwmacxpeefbuemoxljmqvldjxmvvihoeqnuqdsyovwgljtnemouidabhkmvsnauwfnaihekcfwhugejboiyfthyhmkpsaxtidlsbwsmirebax", Value::String("Some value".into()))];

        let resources = Resource::builder_empty().with_attributes([]).build();
        let actual = Attributes::new(attributes, Some(&resources));
        assert_eq!(
            actual,
            Attributes {
                attribute_map: HashMap::from([(
                    "answer".into(),
                    AttributeValue::from(Value::I64(42))
                ),]),
                dropped_attributes_count: 1,
            }
        );
        assert_eq!(actual.attribute_map.len(), 1);
        assert_eq!(actual.dropped_attributes_count, 1);
    }
}
