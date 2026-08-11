use std::collections::BTreeMap;
use std::time::Duration;

use super::SessionThreadConfig;
use super::ThreadConfigContext;
use super::ThreadConfigLoadError;
use super::ThreadConfigLoadErrorCode;
use super::ThreadConfigLoader;
use super::ThreadConfigLoaderFuture;
use super::ThreadConfigSource;
use super::UserThreadConfig;
use proto::thread_config_loader_client::ThreadConfigLoaderClient;

#[path = "proto/codex.thread_config.v1.rs"]
mod proto;

const REMOTE_THREAD_CONFIG_LOAD_TIMEOUT: Duration = Duration::from_secs(5);

/// gRPC-backed [`ThreadConfigLoader`] implementation.
#[derive(Clone, Debug)]
pub struct RemoteThreadConfigLoader {
    endpoint: String,
}

impl RemoteThreadConfigLoader {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
        }
    }

    async fn client(
        &self,
    ) -> Result<ThreadConfigLoaderClient<tonic::transport::Channel>, ThreadConfigLoadError> {
        ThreadConfigLoaderClient::connect(self.endpoint.clone())
            .await
            .map_err(|err| {
                ThreadConfigLoadError::new(
                    ThreadConfigLoadErrorCode::RequestFailed,
                    /*status_code*/ None,
                    format!("failed to connect to remote thread config loader: {err}"),
                )
            })
    }

    async fn load(
        &self,
        context: ThreadConfigContext,
    ) -> Result<Vec<ThreadConfigSource>, ThreadConfigLoadError> {
        let response = self
            .client()
            .await?
            .load(load_thread_config_request(context))
            .await
            .map_err(remote_status_to_error)?
            .into_inner();

        response
            .sources
            .into_iter()
            .map(thread_config_source_from_proto)
            .collect()
    }
}

impl ThreadConfigLoader for RemoteThreadConfigLoader {
    fn load(
        &self,
        context: ThreadConfigContext,
    ) -> ThreadConfigLoaderFuture<'_, Vec<ThreadConfigSource>> {
        Box::pin(RemoteThreadConfigLoader::load(self, context))
    }
}

fn load_thread_config_request(
    context: ThreadConfigContext,
) -> tonic::Request<proto::LoadThreadConfigRequest> {
    let mut request = tonic::Request::new(proto::LoadThreadConfigRequest {
        thread_id: context.thread_id,
        cwd: context.cwd.map(|cwd| cwd.to_string_lossy().into_owned()),
    });
    request.set_timeout(REMOTE_THREAD_CONFIG_LOAD_TIMEOUT);
    request
}

fn remote_status_to_error(status: tonic::Status) -> ThreadConfigLoadError {
    let code = match status.code() {
        tonic::Code::Unauthenticated | tonic::Code::PermissionDenied => {
            ThreadConfigLoadErrorCode::Auth
        }
        tonic::Code::DeadlineExceeded => ThreadConfigLoadErrorCode::Timeout,
        tonic::Code::Ok
        | tonic::Code::Cancelled
        | tonic::Code::Unknown
        | tonic::Code::InvalidArgument
        | tonic::Code::NotFound
        | tonic::Code::AlreadyExists
        | tonic::Code::ResourceExhausted
        | tonic::Code::FailedPrecondition
        | tonic::Code::Aborted
        | tonic::Code::OutOfRange
        | tonic::Code::Unimplemented
        | tonic::Code::Internal
        | tonic::Code::Unavailable
        | tonic::Code::DataLoss => ThreadConfigLoadErrorCode::RequestFailed,
    };
    ThreadConfigLoadError::new(
        code,
        /*status_code*/ None,
        format!("remote thread config request failed: {status}"),
    )
}

fn thread_config_source_from_proto(
    source: proto::ThreadConfigSource,
) -> Result<ThreadConfigSource, ThreadConfigLoadError> {
    match source.source {
        Some(proto::thread_config_source::Source::Session(config)) => {
            session_thread_config_from_proto(config).map(ThreadConfigSource::Session)
        }
        Some(proto::thread_config_source::Source::User(_)) => {
            Ok(ThreadConfigSource::User(UserThreadConfig::default()))
        }
        None => Err(parse_error("remote thread config omitted source payload")),
    }
}

fn session_thread_config_from_proto(
    config: proto::SessionThreadConfig,
) -> Result<SessionThreadConfig, ThreadConfigLoadError> {
    Ok(SessionThreadConfig {
        features: config.features.into_iter().collect::<BTreeMap<_, _>>(),
    })
}

fn parse_error(message: impl Into<String>) -> ThreadConfigLoadError {
    ThreadConfigLoadError::new(
        ThreadConfigLoadErrorCode::Parse,
        /*status_code*/ None,
        message.into(),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::collections::HashMap;

    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;
    use tonic::Request;
    use tonic::Response;
    use tonic::Status;
    use tonic::transport::Server;

    use super::proto::thread_config_loader_server;
    use super::proto::thread_config_loader_server::ThreadConfigLoaderServer;
    use super::*;
    use crate::SessionThreadConfig;
    use crate::UserThreadConfig;

    struct TestServer {
        sources: Vec<proto::ThreadConfigSource>,
        expected_cwd: String,
    }

    impl TestServer {
        async fn load(
            &self,
            request: Request<proto::LoadThreadConfigRequest>,
        ) -> Result<Response<proto::LoadThreadConfigResponse>, Status> {
            assert_eq!(
                request.into_inner(),
                proto::LoadThreadConfigRequest {
                    thread_id: Some("thread-1".to_string()),
                    cwd: Some(self.expected_cwd.clone()),
                }
            );

            Ok(Response::new(proto::LoadThreadConfigResponse {
                sources: self.sources.clone(),
            }))
        }
    }

    impl thread_config_loader_server::ThreadConfigLoader for TestServer {
        fn load<'a, 'async_trait>(
            &'a self,
            request: Request<proto::LoadThreadConfigRequest>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<Response<proto::LoadThreadConfigResponse>, Status>,
                    > + Send
                    + 'async_trait,
            >,
        >
        where
            'a: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(TestServer::load(self, request))
        }
    }

    #[tokio::test]
    async fn load_thread_config_calls_remote_service() {
        let cwd = workspace_dir().join("project");
        let expected_cwd = cwd.to_string_lossy().into_owned();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("test server addr");
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            Server::builder()
                .add_service(ThreadConfigLoaderServer::new(TestServer {
                    sources: proto_sources(),
                    expected_cwd,
                }))
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::TcpListenerStream::new(listener),
                    async {
                        let _ = shutdown_rx.await;
                    },
                )
                .await
        });

        let loader = RemoteThreadConfigLoader::new(format!("http://{addr}"));
        let loaded = loader
            .load(ThreadConfigContext {
                thread_id: Some("thread-1".to_string()),
                cwd: Some(cwd),
            })
            .await;

        let _ = shutdown_tx.send(());
        server.await.expect("join server").expect("server");

        assert_eq!(loaded.expect("load thread config"), expected_sources());
    }

    #[test]
    fn load_thread_config_request_sets_timeout() {
        let request = load_thread_config_request(ThreadConfigContext::default());

        assert_eq!(
            request
                .metadata()
                .get("grpc-timeout")
                .and_then(|value| value.to_str().ok()),
            Some("5000000u")
        );
    }

    fn proto_sources() -> Vec<proto::ThreadConfigSource> {
        vec![
            proto::ThreadConfigSource {
                source: Some(proto::thread_config_source::Source::Session(
                    proto::SessionThreadConfig {
                        features: HashMap::from([
                            ("plugins".to_string(), false),
                            ("tools".to_string(), true),
                        ]),
                    },
                )),
            },
            proto::ThreadConfigSource {
                source: Some(proto::thread_config_source::Source::User(
                    proto::UserThreadConfig {},
                )),
            },
        ]
    }

    fn expected_sources() -> Vec<ThreadConfigSource> {
        vec![
            ThreadConfigSource::Session(SessionThreadConfig {
                features: BTreeMap::from([
                    ("plugins".to_string(), false),
                    ("tools".to_string(), true),
                ]),
            }),
            ThreadConfigSource::User(UserThreadConfig::default()),
        ]
    }

    fn workspace_dir() -> AbsolutePathBuf {
        AbsolutePathBuf::current_dir()
            .expect("current dir")
            .join("workspace")
    }
}
