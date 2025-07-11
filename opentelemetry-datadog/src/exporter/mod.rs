mod intern;
mod model;

pub use model::ApiVersion;
pub use model::Error;
pub use model::FieldMappingFn;

use crate::exporter::model::FieldMapping;
use http::{Method, Request, Uri};
use opentelemetry::{Key, KeyValue};
use opentelemetry_http::{HttpClient, ResponseExt};
use opentelemetry_sdk::{
    error::{OTelSdkError, OTelSdkResult},
    resource::{ResourceDetector, SdkProvidedResourceDetector},
    trace::{Config, SdkTracerProvider, TraceError},
    trace::{SpanData, SpanExporter},
    Resource,
};
use opentelemetry_semantic_conventions as semcov;
use std::borrow::Cow;
use std::fmt::{Debug, Formatter};
use std::sync::Arc;
use url::Url;

use self::model::unified_tags::UnifiedTags;

/// Default Datadog collector endpoint
const DEFAULT_AGENT_ENDPOINT: &str = "http://127.0.0.1:8126";

/// Header name used to inform the Datadog agent of the number of traces in the payload
const DATADOG_TRACE_COUNT_HEADER: &str = "X-Datadog-Trace-Count";

/// Header name use to inform datadog as to what version
const DATADOG_META_LANG_HEADER: &str = "Datadog-Meta-Lang";
const DATADOG_META_TRACER_VERSION_HEADER: &str = "Datadog-Meta-Tracer-Version";

// Struct to hold the mapping between Opentelemetry spans and datadog spans.
pub struct Mapping {
    resource: Option<FieldMapping>,
    name: Option<FieldMapping>,
    service_name: Option<FieldMapping>,
}

impl Mapping {
    pub fn new(
        resource: Option<FieldMapping>,
        name: Option<FieldMapping>,
        service_name: Option<FieldMapping>,
    ) -> Self {
        Mapping {
            resource,
            name,
            service_name,
        }
    }
    pub fn empty() -> Self {
        Self::new(None, None, None)
    }
}

/// Datadog span exporter
pub struct DatadogExporter {
    client: Arc<dyn HttpClient>,
    request_url: Uri,
    model_config: ModelConfig,
    api_version: ApiVersion,
    mapping: Mapping,
    unified_tags: UnifiedTags,
    resource: Option<Resource>,
}

impl DatadogExporter {
    fn new(
        model_config: ModelConfig,
        request_url: Uri,
        api_version: ApiVersion,
        client: Arc<dyn HttpClient>,
        mapping: Mapping,
        unified_tags: UnifiedTags,
    ) -> Self {
        DatadogExporter {
            client,
            request_url,
            model_config,
            api_version,
            mapping,
            unified_tags,
            resource: None,
        }
    }

    fn build_request(
        &self,
        mut batch: Vec<SpanData>,
    ) -> Result<http::Request<Vec<u8>>, OTelSdkError> {
        let traces: Vec<&[SpanData]> = group_into_traces(&mut batch);
        let trace_count = traces.len();
        let data = self
            .api_version
            .encode(
                &self.model_config,
                traces,
                &self.mapping,
                &self.unified_tags,
                self.resource.as_ref(),
            )
            .map_err(|e| OTelSdkError::InternalFailure(format!("{e:?}")))?;
        let req = Request::builder()
            .method(Method::POST)
            .uri(self.request_url.clone())
            .header(http::header::CONTENT_TYPE, self.api_version.content_type())
            .header(DATADOG_TRACE_COUNT_HEADER, trace_count)
            .header(DATADOG_META_LANG_HEADER, "rust")
            .header(
                DATADOG_META_TRACER_VERSION_HEADER,
                env!("CARGO_PKG_VERSION"),
            )
            .body(data)
            .map_err(|e| OTelSdkError::InternalFailure(format!("{e:?}")));
        Ok(req)?
    }
}

impl Debug for DatadogExporter {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatadogExporter")
            .field("model_config", &self.model_config)
            .field("request_url", &self.request_url)
            .field("api_version", &self.api_version)
            .field("client", &self.client)
            .field("resource_mapping", &mapping_debug(&self.mapping.resource))
            .field("name_mapping", &mapping_debug(&self.mapping.name))
            .field(
                "service_name_mapping",
                &mapping_debug(&self.mapping.service_name),
            )
            .finish()
    }
}

/// Create a new Datadog exporter pipeline builder.
pub fn new_pipeline() -> DatadogPipelineBuilder {
    DatadogPipelineBuilder::default()
}

/// Builder for `ExporterConfig` struct.
pub struct DatadogPipelineBuilder {
    agent_endpoint: String,
    trace_config: Option<Config>,
    api_version: ApiVersion,
    client: Option<Arc<dyn HttpClient>>,
    mapping: Mapping,
    unified_tags: UnifiedTags,
}

impl Default for DatadogPipelineBuilder {
    fn default() -> Self {
        DatadogPipelineBuilder {
            agent_endpoint: DEFAULT_AGENT_ENDPOINT.to_string(),
            trace_config: None,
            mapping: Mapping::empty(),
            api_version: ApiVersion::Version05,
            unified_tags: UnifiedTags::new(),
            #[cfg(all(
                not(feature = "reqwest-client"),
                not(feature = "reqwest-blocking-client"),
                not(feature = "surf-client"),
            ))]
            client: None,
            #[cfg(all(
                not(feature = "reqwest-client"),
                not(feature = "reqwest-blocking-client"),
                feature = "surf-client"
            ))]
            client: Some(Arc::new(surf::Client::new())),
            #[cfg(all(
                not(feature = "surf-client"),
                not(feature = "reqwest-blocking-client"),
                feature = "reqwest-client"
            ))]
            client: Some(Arc::new(reqwest::Client::new())),
            #[cfg(feature = "reqwest-blocking-client")]
            client: Some(Arc::new(reqwest::blocking::Client::new())),
        }
    }
}

impl Debug for DatadogPipelineBuilder {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatadogExporter")
            .field("agent_endpoint", &self.agent_endpoint)
            .field("trace_config", &self.trace_config)
            .field("client", &self.client)
            .field("resource_mapping", &mapping_debug(&self.mapping.resource))
            .field("name_mapping", &mapping_debug(&self.mapping.name))
            .field(
                "service_name_mapping",
                &mapping_debug(&self.mapping.service_name),
            )
            .finish()
    }
}

impl DatadogPipelineBuilder {
    /// Building a new exporter.
    ///
    /// This is useful if you are manually constructing a pipeline.
    pub fn build_exporter(mut self) -> Result<DatadogExporter, TraceError> {
        let (_, service_name) = self.build_config_and_service_name();
        self.build_exporter_with_service_name(service_name)
    }

    fn build_config_and_service_name(&mut self) -> (Config, String) {
        let service_name = self.unified_tags.service();
        if let Some(service_name) = service_name {
            let config = if let Some(mut cfg) = self.trace_config.take() {
                cfg.resource = Cow::Owned(
                    Resource::builder()
                        .with_attributes(
                            cfg.resource
                                .iter()
                                .filter(|(k, _v)| k.as_str() != semcov::resource::SERVICE_NAME)
                                .map(|(k, v)| KeyValue::new(k.clone(), v.clone())),
                        )
                        .build(),
                );
                cfg
            } else {
                let mut cfg = Config::default();
                cfg.resource = Cow::Owned(Resource::builder_empty().build());
                cfg
            };
            (config, service_name)
        } else {
            let service_name = SdkProvidedResourceDetector
                .detect()
                .get(&Key::new(semcov::resource::SERVICE_NAME))
                .unwrap()
                .to_string();
            let mut cfg = Config::default();
            // use a empty resource to prevent TracerProvider to assign a service name.
            cfg.resource = Cow::Owned(Resource::builder_empty().build());
            (cfg, service_name)
        }
    }

    // parse the endpoint and append the path based on versions.
    // keep the query and host the same.
    fn build_endpoint(agent_endpoint: &str, version: &str) -> Result<Uri, TraceError> {
        // build agent endpoint based on version
        let mut endpoint = agent_endpoint
            .parse::<Url>()
            .map_err::<Error, _>(Into::into)?;
        let mut paths = endpoint
            .path_segments()
            .map(|c| c.filter(|s| !s.is_empty()).collect::<Vec<_>>())
            .unwrap_or_default();
        paths.push(version);

        let path_str = paths.join("/");
        endpoint.set_path(path_str.as_str());

        Ok(endpoint.as_str().parse().map_err::<Error, _>(Into::into)?)
    }

    fn build_exporter_with_service_name(
        self,
        service_name: String,
    ) -> Result<DatadogExporter, TraceError> {
        if let Some(client) = self.client {
            let model_config = ModelConfig { service_name };

            let exporter = DatadogExporter::new(
                model_config,
                Self::build_endpoint(&self.agent_endpoint, self.api_version.path())?,
                self.api_version,
                client,
                self.mapping,
                self.unified_tags,
            );
            Ok(exporter)
        } else {
            Err(Error::NoHttpClient.into())
        }
    }

    /// Install the Datadog trace exporter pipeline using a simple span processor.
    pub fn install_simple(mut self) -> Result<SdkTracerProvider, TraceError> {
        let (config, service_name) = self.build_config_and_service_name();
        let exporter = self.build_exporter_with_service_name(service_name)?;
        Ok(SdkTracerProvider::builder()
            .with_simple_exporter(exporter)
            .with_resource(config.resource.into_owned())
            .build())
    }

    /// Install the Datadog trace exporter pipeline using a batch span processor with the specified
    /// runtime.
    pub fn install_batch(mut self) -> Result<SdkTracerProvider, TraceError> {
        let (config, service_name) = self.build_config_and_service_name();
        let exporter = self.build_exporter_with_service_name(service_name)?;
        Ok(SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(config.resource.into_owned())
            .build())
    }

    /// Assign the service name under which to group traces
    pub fn with_service_name<T: Into<String>>(mut self, service_name: T) -> Self {
        self.unified_tags.set_service(Some(service_name.into()));
        self
    }

    /// Assign the version under which to group traces
    pub fn with_version<T: Into<String>>(mut self, version: T) -> Self {
        self.unified_tags.set_version(Some(version.into()));
        self
    }

    /// Assign the env under which to group traces
    pub fn with_env<T: Into<String>>(mut self, env: T) -> Self {
        self.unified_tags.set_env(Some(env.into()));
        self
    }

    /// Assign the Datadog collector endpoint.
    ///
    /// The endpoint of the datadog agent, by default it is `http://127.0.0.1:8126`.
    pub fn with_agent_endpoint<T: Into<String>>(mut self, endpoint: T) -> Self {
        self.agent_endpoint = endpoint.into();
        self
    }

    /// Choose the http client used by uploader
    pub fn with_http_client<T: HttpClient + 'static>(mut self, client: T) -> Self {
        self.client = Some(Arc::new(client));
        self
    }

    /// Assign the SDK trace configuration
    pub fn with_trace_config(mut self, config: Config) -> Self {
        self.trace_config = Some(config);
        self
    }

    /// Set version of Datadog trace ingestion API
    pub fn with_api_version(mut self, api_version: ApiVersion) -> Self {
        self.api_version = api_version;
        self
    }

    /// Custom the value used for `resource` field in datadog spans.
    /// See [`FieldMappingFn`] for details.
    pub fn with_resource_mapping<F>(mut self, f: F) -> Self
    where
        F: for<'a> Fn(&'a SpanData, &'a ModelConfig) -> &'a str + Send + Sync + 'static,
    {
        self.mapping.resource = Some(Arc::new(f));
        self
    }

    /// Custom the value used for `name` field in datadog spans.
    /// See [`FieldMappingFn`] for details.
    pub fn with_name_mapping<F>(mut self, f: F) -> Self
    where
        F: for<'a> Fn(&'a SpanData, &'a ModelConfig) -> &'a str + Send + Sync + 'static,
    {
        self.mapping.name = Some(Arc::new(f));
        self
    }

    /// Custom the value used for `service_name` field in datadog spans.
    /// See [`FieldMappingFn`] for details.
    pub fn with_service_name_mapping<F>(mut self, f: F) -> Self
    where
        F: for<'a> Fn(&'a SpanData, &'a ModelConfig) -> &'a str + Send + Sync + 'static,
    {
        self.mapping.service_name = Some(Arc::new(f));
        self
    }
}

fn group_into_traces(spans: &mut [SpanData]) -> Vec<&[SpanData]> {
    if spans.is_empty() {
        return vec![];
    }

    spans.sort_unstable_by_key(|x| x.span_context.trace_id().to_bytes());

    let mut traces: Vec<&[SpanData]> = Vec::with_capacity(spans.len());

    let mut start = 0;
    let mut start_trace_id = spans[start].span_context.trace_id();
    for (idx, span) in spans.iter().enumerate() {
        let current_trace_id = span.span_context.trace_id();
        if start_trace_id != current_trace_id {
            traces.push(&spans[start..idx]);
            start = idx;
            start_trace_id = current_trace_id;
        }
    }
    traces.push(&spans[start..]);
    traces
}

async fn send_request(
    client: Arc<dyn HttpClient>,
    request: http::Request<Vec<u8>>,
) -> OTelSdkResult {
    #[allow(deprecated)]
    let response = client
        .send(request)
        .await
        .map_err(|e| OTelSdkError::InternalFailure(format!("HTTP request failed: {e}")))?;

    response
        .error_for_status()
        .map_err(|e| OTelSdkError::InternalFailure(format!("HTTP response error: {e}")))?;

    Ok(())
}

impl SpanExporter for DatadogExporter {
    /// Export spans to datadog-agent
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        let request = match self.build_request(batch) {
            Ok(req) => req,
            Err(err) => return Err(err),
        };

        let client = self.client.clone();
        send_request(client, request).await
    }
    fn set_resource(&mut self, resource: &Resource) {
        self.resource = Some(resource.clone());
    }
}

/// Helper struct to custom the mapping between Opentelemetry spans and datadog spans.
///
/// This struct will be passed to [`FieldMappingFn`]
#[derive(Default, Debug)]
#[non_exhaustive]
pub struct ModelConfig {
    pub service_name: String,
}

fn mapping_debug(f: &Option<FieldMapping>) -> String {
    if f.is_some() {
        "custom mapping"
    } else {
        "default mapping"
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ApiVersion::Version05;

    use crate::exporter::model::tests::get_span;
    use bytes::Bytes;

    #[test]
    fn test_out_of_order_group() {
        let mut batch = vec![get_span(1, 1, 1), get_span(2, 2, 2), get_span(1, 1, 3)];
        let expected = vec![
            vec![get_span(1, 1, 1), get_span(1, 1, 3)],
            vec![get_span(2, 2, 2)],
        ];

        let mut traces = group_into_traces(&mut batch);
        // We need to sort the output in order to compare, but this is not required by the Datadog agent
        traces.sort_by_key(|t| u128::from_be_bytes(t[0].span_context.trace_id().to_bytes()));

        assert_eq!(traces, expected);
    }

    #[test]
    fn test_agent_endpoint_with_api_version() {
        let with_tail_slash =
            DatadogPipelineBuilder::build_endpoint("http://localhost:8126/", Version05.path());
        let without_tail_slash =
            DatadogPipelineBuilder::build_endpoint("http://localhost:8126", Version05.path());
        let with_query = DatadogPipelineBuilder::build_endpoint(
            "http://localhost:8126?api_key=123",
            Version05.path(),
        );
        let invalid = DatadogPipelineBuilder::build_endpoint(
            "http://localhost:klsajfjksfh",
            Version05.path(),
        );

        assert_eq!(
            with_tail_slash.unwrap().to_string(),
            "http://localhost:8126/v0.5/traces"
        );
        assert_eq!(
            without_tail_slash.unwrap().to_string(),
            "http://localhost:8126/v0.5/traces"
        );
        assert_eq!(
            with_query.unwrap().to_string(),
            "http://localhost:8126/v0.5/traces?api_key=123"
        );
        assert!(invalid.is_err())
    }

    #[derive(Debug)]
    struct DummyClient;

    #[async_trait::async_trait]
    impl HttpClient for DummyClient {
        async fn send(
            &self,
            _request: Request<Vec<u8>>,
        ) -> Result<http::Response<bytes::Bytes>, opentelemetry_http::HttpError> {
            Ok(http::Response::new("dummy response".into()))
        }
        async fn send_bytes(
            &self,
            request: Request<Bytes>,
        ) -> Result<http::Response<Bytes>, opentelemetry_http::HttpError> {
            Ok(http::Response::builder()
                .status(200)
                .body(request.into_body())
                .unwrap())
        }
    }

    #[test]
    fn test_custom_http_client() {
        new_pipeline()
            .with_http_client(DummyClient)
            .build_exporter()
            .unwrap();
    }

    #[test]
    fn test_install_simple() {
        new_pipeline()
            .with_service_name("test_service")
            .with_http_client(DummyClient)
            .install_simple()
            .unwrap();
    }

    #[test]
    fn test_install_batch() {
        new_pipeline()
            .with_service_name("test_service")
            .with_http_client(DummyClient)
            .install_batch()
            .unwrap();
    }
}
