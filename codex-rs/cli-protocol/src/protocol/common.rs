use std::path::PathBuf;

use crate::JSONRPCNotification;
use crate::JSONRPCRequest;
use crate::JsonSchema;
use crate::RequestId;
use crate::TS;
use crate::protocol::v1;
use crate::protocol::v2;
use codex_experimental_api_macros::ExperimentalApi;
use serde::Deserialize;
use serde::Serialize;
use strum_macros::Display;

/// Authentication mode for OpenAI-backed providers.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    /// OpenAI API key provided by the caller and stored by Codex.
    ApiKey,
    /// ChatGPT OAuth managed by Codex (tokens persisted and refreshed by Codex).
    Chatgpt,
    /// [UNSTABLE] FOR OPENAI INTERNAL USE ONLY - DO NOT USE.
    ///
    /// ChatGPT auth tokens are supplied by an external host app and are only
    /// stored in memory. Token refresh must be handled by the external host app.
    #[serde(rename = "chatgptAuthTokens")]
    #[ts(rename = "chatgptAuthTokens")]
    #[strum(serialize = "chatgptAuthTokens")]
    ChatgptAuthTokens,
    /// Backend auth supplied as request headers.
    #[serde(rename = "headers")]
    #[ts(rename = "headers")]
    #[strum(serialize = "headers")]
    Headers,
    /// Programmatic Codex auth backed by a registered Agent Identity.
    #[serde(rename = "agentIdentity")]
    #[ts(rename = "agentIdentity")]
    #[strum(serialize = "agentIdentity")]
    AgentIdentity,
    /// Programmatic Codex auth backed by a personal access token.
    #[serde(rename = "personalAccessToken")]
    #[ts(rename = "personalAccessToken")]
    #[strum(serialize = "personalAccessToken")]
    PersonalAccessToken,
}

impl AuthMode {
    /// Returns whether this mode represents an authenticated human ChatGPT account.
    pub fn has_chatgpt_account(self) -> bool {
        match self {
            Self::Chatgpt | Self::ChatgptAuthTokens | Self::PersonalAccessToken => true,
            Self::ApiKey | Self::Headers | Self::AgentIdentity => false,
        }
    }

    /// Returns whether this mode is backed by Codex services rather than a direct model API.
    pub fn uses_codex_backend(self) -> bool {
        match self {
            Self::Chatgpt
            | Self::ChatgptAuthTokens
            | Self::Headers
            | Self::AgentIdentity
            | Self::PersonalAccessToken => true,
            Self::ApiKey => false,
        }
    }
}

macro_rules! experimental_reason_expr {
    // If a request variant is explicitly marked experimental, that reason wins.
    (variant $variant:ident, #[experimental($reason:expr)] $params:ident $(, $inspect_params:tt)?) => {
        Some($reason)
    };
    // `inspect_params: true` is used when a method is mostly stable but needs
    // field-level gating from its params type (for example, ThreadStart).
    (variant $variant:ident, $params:ident, true) => {
        crate::experimental_api::ExperimentalApi::experimental_reason($params)
    };
    (variant $variant:ident, $params:ident $(, $inspect_params:tt)?) => {
        None
    };
}

#[cfg(test)]
macro_rules! experimental_method_entry {
    (#[experimental($reason:expr)] => $wire:literal) => {
        $wire
    };
    (#[experimental($reason:expr)]) => {
        $reason
    };
    ($($tt:tt)*) => {
        ""
    };
}

#[cfg(test)]
macro_rules! experimental_type_entry {
    (#[experimental($reason:expr)] $ty:ty) => {
        stringify!($ty)
    };
    ($ty:ty) => {
        ""
    };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientRequestSerializationScope {
    Global(&'static str),
    GlobalSharedRead(&'static str),
    Thread { thread_id: String },
    ThreadPath { path: PathBuf },
    CommandExecProcess { process_id: String },
    Process { process_handle: String },
    FuzzyFileSearchSession { session_id: String },
    FsWatch { watch_id: String },
}

macro_rules! serialization_scope_expr {
    ($actual_params:ident, None) => {
        None
    };
    ($actual_params:ident, global($key:literal)) => {
        Some(ClientRequestSerializationScope::Global($key))
    };
    ($actual_params:ident, global_shared_read($key:literal)) => {
        Some(ClientRequestSerializationScope::GlobalSharedRead($key))
    };
    ($actual_params:ident, thread_id($params:ident . $field:ident)) => {
        Some(ClientRequestSerializationScope::Thread {
            thread_id: $actual_params.$field.clone(),
        })
    };
    ($actual_params:ident, optional_thread_id($params:ident . $field:ident)) => {
        $actual_params
            .$field
            .clone()
            .map(|thread_id| ClientRequestSerializationScope::Thread { thread_id })
    };
    ($actual_params:ident, thread_or_path($params:ident . $thread_field:ident, $params2:ident . $path_field:ident)) => {
        if !$actual_params.$thread_field.is_empty() {
            Some(ClientRequestSerializationScope::Thread {
                thread_id: $actual_params.$thread_field.clone(),
            })
        } else if let Some(path) = $actual_params.$path_field.clone() {
            Some(ClientRequestSerializationScope::ThreadPath { path })
        } else {
            Some(ClientRequestSerializationScope::Thread {
                thread_id: $actual_params.$thread_field.clone(),
            })
        }
    };
    ($actual_params:ident, optional_command_process_id($params:ident . $field:ident)) => {
        $actual_params
            .$field
            .clone()
            .map(|process_id| ClientRequestSerializationScope::CommandExecProcess { process_id })
    };
    ($actual_params:ident, command_process_id($params:ident . $field:ident)) => {
        Some(ClientRequestSerializationScope::CommandExecProcess {
            process_id: $actual_params.$field.clone(),
        })
    };
    ($actual_params:ident, process_handle($params:ident . $field:ident)) => {
        Some(ClientRequestSerializationScope::Process {
            process_handle: $actual_params.$field.clone(),
        })
    };
    ($actual_params:ident, fuzzy_session_id($params:ident . $field:ident)) => {
        Some(ClientRequestSerializationScope::FuzzyFileSearchSession {
            session_id: $actual_params.$field.clone(),
        })
    };
    ($actual_params:ident, fs_watch_id($params:ident . $field:ident)) => {
        Some(ClientRequestSerializationScope::FsWatch {
            watch_id: $actual_params.$field.clone(),
        })
    };
}

/// Generates an `enum ClientRequest` where each variant is a request that the
/// client can send to the server. Each variant has associated `params` and
/// `response` types. Also generates a `export_client_responses()` function to
/// export all response types to TypeScript.
macro_rules! client_request_definitions {
    (
        $(
            $(#[experimental($reason:expr)])?
            $(#[doc = $variant_doc:literal])*
            $variant:ident => $wire:literal {
                params: $(#[$params_meta:meta])* $params:ty,
                $(inspect_params: $inspect_params:tt,)?
                serialization: $serialization:ident $( ( $($serialization_args:tt)* ) )?,
                $(manual_payload_conversion: $manual_payload_conversion:ident,)?
                response: $response:ty,
            }
        ),* $(,)?
    ) => {
        /// Request from the client to the server.
        #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
        #[serde(tag = "method", rename_all = "camelCase")]
        pub enum ClientRequest {
            $(
                $(#[doc = $variant_doc])*
                #[serde(rename = $wire)]
                #[ts(rename = $wire)]
                $variant {
                    #[serde(rename = "id")]
                    request_id: RequestId,
                    $(#[$params_meta])*
                    params: $params,
                },
            )*
        }

        impl ClientRequest {
            pub fn id(&self) -> &RequestId {
                match self {
                    $(Self::$variant { request_id, .. } => request_id,)*
                }
            }

            pub const fn method_name(&self) -> &'static str {
                match self {
                    $(Self::$variant { .. } => $wire,)*
                }
            }

            pub fn serialization_scope(&self) -> Option<ClientRequestSerializationScope> {
                match self {
                    $(
                        Self::$variant { params, .. } => {
                            let _ = params;
                            serialization_scope_expr!(
                                params, $serialization $( ( $($serialization_args)* ) )?
                            )
                        }
                    )*
                }
            }
        }

        impl TryFrom<JSONRPCRequest> for ClientRequest {
            type Error = serde_json::Error;

            fn try_from(request: JSONRPCRequest) -> Result<Self, Self::Error> {
                let JSONRPCRequest {
                    id: request_id,
                    method,
                    params,
                    trace: _,
                } = request;
                let mut request = serde_json::Map::new();
                request.insert("id".to_string(), serde_json::to_value(request_id)?);
                request.insert("method".to_string(), serde_json::Value::String(method));
                if let Some(params) = params {
                    request.insert("params".to_string(), params);
                }
                serde_json::from_value(serde_json::Value::Object(request))
            }
        }

        /// Typed response from the server to the client.
        #[derive(Serialize, Deserialize, Debug, Clone)]
        #[allow(clippy::large_enum_variant)]
        #[serde(tag = "method", rename_all = "camelCase")]
        pub enum ClientResponse {
            $(
                $(#[doc = $variant_doc])*
                #[serde(rename = $wire)]
                $variant {
                    #[serde(rename = "id")]
                    request_id: RequestId,
                    response: $response,
                },
            )*
        }

        impl ClientResponse {
            pub fn id(&self) -> &RequestId {
                match self {
                    $(Self::$variant { request_id, .. } => request_id,)*
                }
            }

            pub fn method(&self) -> String {
                match self {
                    $(Self::$variant { .. } => $wire.to_string(),)*
                }
            }

            pub fn into_jsonrpc_parts(
                self,
            ) -> std::result::Result<(RequestId, crate::Result), serde_json::Error> {
                match self {
                    $(
                        Self::$variant { request_id, response } => {
                            serde_json::to_value(response).map(|result| (request_id, result))
                        }
                    )*
                }
            }
        }

        #[derive(Debug, Clone, Serialize)]
        #[serde(untagged)]
        #[allow(clippy::large_enum_variant)]
        pub enum ClientResponsePayload {
            $( $variant($response), )*
            InterruptConversation(v1::InterruptConversationResponse),
        }

        impl ClientResponsePayload {
            pub fn into_client_response(self, request_id: RequestId) -> Option<ClientResponse> {
                match self {
                    $(
                        Self::$variant(response) => {
                            Some(ClientResponse::$variant {
                                request_id,
                                response,
                            })
                        }
                    )*
                    Self::InterruptConversation(_) => None,
                }
            }

            pub fn into_jsonrpc_parts(
                self,
                request_id: RequestId,
            ) -> std::result::Result<(RequestId, crate::Result), serde_json::Error> {
                self.to_jsonrpc_parts(request_id)
            }

            pub fn to_jsonrpc_parts(
                &self,
                request_id: RequestId,
            ) -> std::result::Result<(RequestId, crate::Result), serde_json::Error> {
                match self {
                    $(
                        Self::$variant(response) => {
                            serde_json::to_value(response).map(|result| (request_id, result))
                        }
                    )*
                    Self::InterruptConversation(response) => {
                        serde_json::to_value(response).map(|result| (request_id, result))
                    }
                }
            }
        }

        impl From<v1::InterruptConversationResponse> for ClientResponsePayload {
            fn from(response: v1::InterruptConversationResponse) -> Self {
                Self::InterruptConversation(response)
            }
        }

        $(
            client_response_payload_from_impl!(
                $variant,
                $response
                $(, $manual_payload_conversion)?
            );
        )*

        impl crate::experimental_api::ExperimentalApi for ClientRequest {
            fn experimental_reason(&self) -> Option<&'static str> {
                match self {
                    $(
                        Self::$variant { params: _params, .. } => {
                            experimental_reason_expr!(
                                variant $variant,
                                $(#[experimental($reason)])?
                                _params
                                $(, $inspect_params)?
                            )
                        }
                    )*
                }
            }
        }

        #[cfg(test)]
        pub(crate) const EXPERIMENTAL_CLIENT_METHODS: &[&str] = &[
            $(
                experimental_method_entry!($(#[experimental($reason)])? => $wire),
            )*
        ];
        #[cfg(test)]
        pub(crate) const EXPERIMENTAL_CLIENT_METHOD_PARAM_TYPES: &[&str] = &[
            $(
                experimental_type_entry!($(#[experimental($reason)])? $params),
            )*
        ];
        #[cfg(test)]
        pub(crate) const EXPERIMENTAL_CLIENT_METHOD_RESPONSE_TYPES: &[&str] = &[
            $(
                experimental_type_entry!($(#[experimental($reason)])? $response),
            )*
        ];

    };
}

macro_rules! client_response_payload_from_impl {
    ($variant:ident, $response:ty) => {
        impl From<$response> for ClientResponsePayload {
            fn from(response: $response) -> Self {
                Self::$variant(response)
            }
        }
    };
    ($variant:ident, $response:ty, manual) => {};
}

client_request_definitions! {
    Initialize => "initialize" {
        params: v1::InitializeParams,
        serialization: None,
        response: v1::InitializeResponse,
    },

    /// NEW APIs
    // Thread lifecycle
    // Uses `inspect_params` because only some fields are experimental.
    ThreadStart => "thread/start" {
        params: v2::ThreadStartParams,
        inspect_params: true,
        serialization: None,
        response: v2::ThreadStartResponse,
    },
    ThreadResume => "thread/resume" {
        params: v2::ThreadResumeParams,
        inspect_params: true,
        serialization: thread_or_path(params.thread_id, params.path),
        response: v2::ThreadResumeResponse,
    },
    ThreadFork => "thread/fork" {
        params: v2::ThreadForkParams,
        inspect_params: true,
        serialization: thread_or_path(params.thread_id, params.path),
        response: v2::ThreadForkResponse,
    },
    ThreadArchive => "thread/archive" {
        params: v2::ThreadArchiveParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadArchiveResponse,
    },
    ThreadDelete => "thread/delete" {
        params: v2::ThreadDeleteParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadDeleteResponse,
    },
    ThreadUnsubscribe => "thread/unsubscribe" {
        params: v2::ThreadUnsubscribeParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadUnsubscribeResponse,
    },
    #[experimental("thread/increment_elicitation")]
    /// Increment the thread-local out-of-band elicitation counter.
    ///
    /// This is used by external helpers to pause timeout accounting while a user
    /// approval or other elicitation is pending outside the app-server request flow.
    ThreadIncrementElicitation => "thread/increment_elicitation" {
        params: v2::ThreadIncrementElicitationParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadIncrementElicitationResponse,
    },
    #[experimental("thread/decrement_elicitation")]
    /// Decrement the thread-local out-of-band elicitation counter.
    ///
    /// When the count reaches zero, timeout accounting resumes for the thread.
    ThreadDecrementElicitation => "thread/decrement_elicitation" {
        params: v2::ThreadDecrementElicitationParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadDecrementElicitationResponse,
    },
    ThreadSetName => "thread/name/set" {
        params: v2::ThreadSetNameParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadSetNameResponse,
    },
    ThreadGoalSet => "thread/goal/set" {
        params: v2::ThreadGoalSetParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadGoalSetResponse,
    },
    ThreadGoalGet => "thread/goal/get" {
        params: v2::ThreadGoalGetParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadGoalGetResponse,
    },
    ThreadGoalClear => "thread/goal/clear" {
        params: v2::ThreadGoalClearParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadGoalClearResponse,
    },
    ThreadMetadataUpdate => "thread/metadata/update" {
        params: v2::ThreadMetadataUpdateParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadMetadataUpdateResponse,
    },
    ThreadSectionMove => "thread/section/move" {
        params: v2::ThreadSectionMoveParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadSectionMoveResponse,
    },
    #[experimental("thread/settings/update")]
    ThreadSettingsUpdate => "thread/settings/update" {
        params: v2::ThreadSettingsUpdateParams,
        inspect_params: true,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadSettingsUpdateResponse,
    },
    #[experimental("thread/memoryMode/set")]
    ThreadMemoryModeSet => "thread/memoryMode/set" {
        params: v2::ThreadMemoryModeSetParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadMemoryModeSetResponse,
    },
    #[experimental("memory/reset")]
    MemoryReset => "memory/reset" {
        params: #[ts(type = "undefined")] #[serde(skip_serializing_if = "Option::is_none")] Option<()>,
        serialization: global("memory"),
        response: v2::MemoryResetResponse,
    },
    ThreadUnarchive => "thread/unarchive" {
        params: v2::ThreadUnarchiveParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadUnarchiveResponse,
    },
    ThreadCompactStart => "thread/compact/start" {
        params: v2::ThreadCompactStartParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadCompactStartResponse,
    },
    ThreadShellCommand => "thread/shellCommand" {
        params: v2::ThreadShellCommandParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadShellCommandResponse,
    },
    #[experimental("thread/backgroundTerminals/clean")]
    ThreadBackgroundTerminalsClean => "thread/backgroundTerminals/clean" {
        params: v2::ThreadBackgroundTerminalsCleanParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadBackgroundTerminalsCleanResponse,
    },
    #[experimental("thread/backgroundTerminals/list")]
    ThreadBackgroundTerminalsList => "thread/backgroundTerminals/list" {
        params: v2::ThreadBackgroundTerminalsListParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadBackgroundTerminalsListResponse,
    },
    #[experimental("thread/backgroundTerminals/terminate")]
    ThreadBackgroundTerminalsTerminate => "thread/backgroundTerminals/terminate" {
        params: v2::ThreadBackgroundTerminalsTerminateParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadBackgroundTerminalsTerminateResponse,
    },
    ThreadRollback => "thread/rollback" {
        params: v2::ThreadRollbackParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadRollbackResponse,
    },
    ThreadList => "thread/list" {
        params: v2::ThreadListParams,
        inspect_params: true,
        serialization: None,
        response: v2::ThreadListResponse,
    },
    ThreadSectionList => "threadSection/list" {
        params: v2::ThreadSectionListParams,
        serialization: global_shared_read("thread-sections"),
        response: v2::ThreadSectionListResponse,
    },
    ThreadSectionCreate => "threadSection/create" {
        params: v2::ThreadSectionCreateParams,
        serialization: global("thread-sections"),
        response: v2::ThreadSectionCreateResponse,
    },
    ThreadSectionUpdate => "threadSection/update" {
        params: v2::ThreadSectionUpdateParams,
        serialization: global("thread-sections"),
        response: v2::ThreadSectionUpdateResponse,
    },
    ThreadSectionDelete => "threadSection/delete" {
        params: v2::ThreadSectionDeleteParams,
        serialization: global("thread-sections"),
        response: v2::ThreadSectionDeleteResponse,
    },
    #[experimental("thread/search")]
    ThreadSearch => "thread/search" {
        params: v2::ThreadSearchParams,
        serialization: None,
        response: v2::ThreadSearchResponse,
    },
    #[experimental("thread/searchOccurrences")]
    ThreadSearchOccurrences => "thread/searchOccurrences" {
        params: v2::ThreadSearchOccurrencesParams,
        // Explicitly concurrent: this reads persisted paginated history.
        serialization: None,
        response: v2::ThreadSearchOccurrencesResponse,
    },
    ThreadLoadedList => "thread/loaded/list" {
        params: v2::ThreadLoadedListParams,
        serialization: None,
        response: v2::ThreadLoadedListResponse,
    },
    ThreadRead => "thread/read" {
        params: v2::ThreadReadParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadReadResponse,
    },
    #[experimental("thread/turns/list")]
    ThreadTurnsList => "thread/turns/list" {
        params: v2::ThreadTurnsListParams,
        // Explicitly concurrent: this primarily reads append-only rollout storage.
        serialization: None,
        response: v2::ThreadTurnsListResponse,
    },
    #[experimental("thread/items/list")]
    ThreadItemsList => "thread/items/list" {
        params: v2::ThreadItemsListParams,
        // Explicitly concurrent: this primarily reads append-only rollout storage.
        serialization: None,
        response: v2::ThreadItemsListResponse,
    },
    /// Append raw Responses API items to the thread history without starting a user turn.
    ThreadInjectItems => "thread/inject_items" {
        params: v2::ThreadInjectItemsParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadInjectItemsResponse,
    },
    SkillsList => "skills/list" {
        params: v2::SkillsListParams,
        serialization: global_shared_read("config"),
        response: v2::SkillsListResponse,
    },
    SkillsExtraRootsSet => "skills/extraRoots/set" {
        params: v2::SkillsExtraRootsSetParams,
        serialization: global("config"),
        response: v2::SkillsExtraRootsSetResponse,
    },
    // File system requests are intentionally concurrent. Desktop already treats local
    // file system operations as concurrent, and app-server remote fs mirrors that model.
    FsReadFile => "fs/readFile" {
        params: v2::FsReadFileParams,
        serialization: None,
        response: v2::FsReadFileResponse,
    },
    FsWriteFile => "fs/writeFile" {
        params: v2::FsWriteFileParams,
        serialization: None,
        response: v2::FsWriteFileResponse,
    },
    FsCreateDirectory => "fs/createDirectory" {
        params: v2::FsCreateDirectoryParams,
        serialization: None,
        response: v2::FsCreateDirectoryResponse,
    },
    FsGetMetadata => "fs/getMetadata" {
        params: v2::FsGetMetadataParams,
        serialization: None,
        response: v2::FsGetMetadataResponse,
    },
    FsReadDirectory => "fs/readDirectory" {
        params: v2::FsReadDirectoryParams,
        serialization: None,
        response: v2::FsReadDirectoryResponse,
    },
    FsRemove => "fs/remove" {
        params: v2::FsRemoveParams,
        serialization: None,
        response: v2::FsRemoveResponse,
    },
    FsCopy => "fs/copy" {
        params: v2::FsCopyParams,
        serialization: None,
        response: v2::FsCopyResponse,
    },
    FsWatch => "fs/watch" {
        params: v2::FsWatchParams,
        serialization: fs_watch_id(params.watch_id),
        response: v2::FsWatchResponse,
    },
    FsUnwatch => "fs/unwatch" {
        params: v2::FsUnwatchParams,
        serialization: fs_watch_id(params.watch_id),
        response: v2::FsUnwatchResponse,
    },
    SkillsConfigWrite => "skills/config/write" {
        params: v2::SkillsConfigWriteParams,
        serialization: global("config"),
        response: v2::SkillsConfigWriteResponse,
    },
    TurnStart => "turn/start" {
        params: v2::TurnStartParams,
        inspect_params: true,
        serialization: thread_id(params.thread_id),
        response: v2::TurnStartResponse,
    },
    TurnSteer => "turn/steer" {
        params: v2::TurnSteerParams,
        inspect_params: true,
        serialization: thread_id(params.thread_id),
        response: v2::TurnSteerResponse,
    },
    TurnInterrupt => "turn/interrupt" {
        params: v2::TurnInterruptParams,
        serialization: thread_id(params.thread_id),
        response: v2::TurnInterruptResponse,
    },
    #[experimental("thread/realtime/start")]
    ThreadRealtimeStart => "thread/realtime/start" {
        params: v2::ThreadRealtimeStartParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadRealtimeStartResponse,
    },
    #[experimental("thread/realtime/appendAudio")]
    ThreadRealtimeAppendAudio => "thread/realtime/appendAudio" {
        params: v2::ThreadRealtimeAppendAudioParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadRealtimeAppendAudioResponse,
    },
    #[experimental("thread/realtime/appendText")]
    ThreadRealtimeAppendText => "thread/realtime/appendText" {
        params: v2::ThreadRealtimeAppendTextParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadRealtimeAppendTextResponse,
    },
    #[experimental("thread/realtime/appendSpeech")]
    ThreadRealtimeAppendSpeech => "thread/realtime/appendSpeech" {
        params: v2::ThreadRealtimeAppendSpeechParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadRealtimeAppendSpeechResponse,
    },
    #[experimental("thread/realtime/stop")]
    ThreadRealtimeStop => "thread/realtime/stop" {
        params: v2::ThreadRealtimeStopParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadRealtimeStopResponse,
    },
    #[experimental("thread/realtime/listVoices")]
    ThreadRealtimeListVoices => "thread/realtime/listVoices" {
        params: v2::ThreadRealtimeListVoicesParams,
        serialization: None,
        response: v2::ThreadRealtimeListVoicesResponse,
    },
    ModelList => "model/list" {
        params: v2::ModelListParams,
        serialization: None,
        response: v2::ModelListResponse,
    },
    ExperimentalFeatureList => "experimentalFeature/list" {
        params: v2::ExperimentalFeatureListParams,
        serialization: global("config"),
        response: v2::ExperimentalFeatureListResponse,
    },
    PermissionProfileList => "permissionProfile/list" {
        params: v2::PermissionProfileListParams,
        serialization: global_shared_read("config"),
        response: v2::PermissionProfileListResponse,
    },
    #[experimental("mock/experimentalMethod")]
    /// Test-only method used to validate experimental gating.
    MockExperimentalMethod => "mock/experimentalMethod" {
        params: v2::MockExperimentalMethodParams,
        serialization: None,
        response: v2::MockExperimentalMethodResponse,
    },
    #[experimental("environment/add")]
    /// Adds or replaces a remote environment by id for later selection.
    EnvironmentAdd => "environment/add" {
        params: v2::EnvironmentAddParams,
        serialization: global("environment"),
        response: v2::EnvironmentAddResponse,
    },
    #[experimental("environment/info")]
    /// Reads information from a configured execution environment.
    EnvironmentInfo => "environment/info" {
        params: v2::EnvironmentInfoParams,
        serialization: global_shared_read("environment"),
        response: v2::EnvironmentInfoResponse,
    },
    #[experimental("environment/status")]
    /// Reads the current status of a configured execution environment.
    EnvironmentStatus => "environment/status" {
        params: v2::EnvironmentStatusParams,
        serialization: global_shared_read("environment"),
        response: v2::EnvironmentStatusResponse,
    },

    LoginAccount => "account/login/start" {
        params: v2::LoginAccountParams,
        inspect_params: true,
        serialization: global("account-auth"),
        response: v2::LoginAccountResponse,
    },

    CancelLoginAccount => "account/login/cancel" {
        params: v2::CancelLoginAccountParams,
        serialization: global("account-auth"),
        response: v2::CancelLoginAccountResponse,
    },

    LogoutAccount => "account/logout" {
        params: #[ts(type = "undefined")] #[serde(skip_serializing_if = "Option::is_none")] Option<()>,
        serialization: global("account-auth"),
        response: v2::LogoutAccountResponse,
    },

    GetAccountRateLimits => "account/rateLimits/read" {
        params: #[ts(type = "undefined")] #[serde(skip_serializing_if = "Option::is_none")] Option<()>,
        serialization: None,
        response: v2::GetAccountRateLimitsResponse,
    },

    ConsumeAccountRateLimitResetCredit => "account/rateLimitResetCredit/consume" {
        params: v2::ConsumeAccountRateLimitResetCreditParams,
        serialization: global("account-auth"),
        response: v2::ConsumeAccountRateLimitResetCreditResponse,
    },

    GetAccountTokenUsage => "account/usage/read" {
        params: #[ts(type = "undefined")] #[serde(skip_serializing_if = "Option::is_none")] Option<()>,
        serialization: None,
        response: v2::GetAccountTokenUsageResponse,
    },

    GetWorkspaceMessages => "account/workspaceMessages/read" {
        params: #[ts(type = "undefined")] #[serde(skip_serializing_if = "Option::is_none")] Option<()>,
        serialization: None,
        response: v2::GetWorkspaceMessagesResponse,
    },

    SendAddCreditsNudgeEmail => "account/sendAddCreditsNudgeEmail" {
        params: v2::SendAddCreditsNudgeEmailParams,
        serialization: global("account-auth"),
        response: v2::SendAddCreditsNudgeEmailResponse,
    },

    /// Execute a standalone command (argv vector) under the server's sandbox.
    OneOffCommandExec => "command/exec" {
        params: v2::CommandExecParams,
        inspect_params: true,
        serialization: optional_command_process_id(params.process_id),
        response: v2::CommandExecResponse,
    },
    /// Write stdin bytes to a running `command/exec` session or close stdin.
    CommandExecWrite => "command/exec/write" {
        params: v2::CommandExecWriteParams,
        serialization: command_process_id(params.process_id),
        response: v2::CommandExecWriteResponse,
    },
    /// Terminate a running `command/exec` session by client-supplied `processId`.
    CommandExecTerminate => "command/exec/terminate" {
        params: v2::CommandExecTerminateParams,
        serialization: command_process_id(params.process_id),
        response: v2::CommandExecTerminateResponse,
    },
    /// Resize a running PTY-backed `command/exec` session by client-supplied `processId`.
    CommandExecResize => "command/exec/resize" {
        params: v2::CommandExecResizeParams,
        serialization: command_process_id(params.process_id),
        response: v2::CommandExecResizeResponse,
    },
    #[experimental("process/spawn")]
    /// Spawn a standalone process (argv vector) without a Codex sandbox.
    ProcessSpawn => "process/spawn" {
        params: v2::ProcessSpawnParams,
        serialization: process_handle(params.process_handle),
        response: v2::ProcessSpawnResponse,
    },
    #[experimental("process/writeStdin")]
    /// Write stdin bytes to a running `process/spawn` session or close stdin.
    ProcessWriteStdin => "process/writeStdin" {
        params: v2::ProcessWriteStdinParams,
        serialization: process_handle(params.process_handle),
        response: v2::ProcessWriteStdinResponse,
    },
    #[experimental("process/kill")]
    /// Terminate a running `process/spawn` session by client-supplied `processHandle`.
    ProcessKill => "process/kill" {
        params: v2::ProcessKillParams,
        serialization: process_handle(params.process_handle),
        response: v2::ProcessKillResponse,
    },
    #[experimental("process/resizePty")]
    /// Resize a running PTY-backed `process/spawn` session by client-supplied `processHandle`.
    ProcessResizePty => "process/resizePty" {
        params: v2::ProcessResizePtyParams,
        serialization: process_handle(params.process_handle),
        response: v2::ProcessResizePtyResponse,
    },

    ConfigRead => "config/read" {
        params: v2::ConfigReadParams,
        serialization: global_shared_read("config"),
        response: v2::ConfigReadResponse,
    },
    ConfigValueWrite => "config/value/write" {
        params: v2::ConfigValueWriteParams,
        serialization: global("config"),
        manual_payload_conversion: manual,
        response: v2::ConfigWriteResponse,
    },
    ConfigBatchWrite => "config/batchWrite" {
        params: v2::ConfigBatchWriteParams,
        serialization: global("config"),
        manual_payload_conversion: manual,
        response: v2::ConfigWriteResponse,
    },

    GetAccount => "account/read" {
        params: v2::GetAccountParams,
        serialization: global("account-auth"),
        response: v2::GetAccountResponse,
    },

    /// DEPRECATED APIs below
    GetConversationSummary => "getConversationSummary" {
        params: v1::GetConversationSummaryParams,
        serialization: None,
        response: v1::GetConversationSummaryResponse,
    },
    GitDiffToRemote => "gitDiffToRemote" {
        params: v1::GitDiffToRemoteParams,
        serialization: None,
        response: v1::GitDiffToRemoteResponse,
    },
    /// DEPRECATED in favor of GetAccount
    GetAuthStatus => "getAuthStatus" {
        params: v1::GetAuthStatusParams,
        serialization: global("account-auth"),
        response: v1::GetAuthStatusResponse,
    },
    // Legacy fuzzy search cancellation is intentionally concurrent: clients reuse a
    // cancellation token so a newer request can cancel an older in-flight search.
    FuzzyFileSearch => "fuzzyFileSearch" {
        params: FuzzyFileSearchParams,
        serialization: None,
        response: FuzzyFileSearchResponse,
    },
    #[experimental("fuzzyFileSearch/sessionStart")]
    FuzzyFileSearchSessionStart => "fuzzyFileSearch/sessionStart" {
        params: FuzzyFileSearchSessionStartParams,
        serialization: fuzzy_session_id(params.session_id),
        response: FuzzyFileSearchSessionStartResponse,
    },
    #[experimental("fuzzyFileSearch/sessionUpdate")]
    FuzzyFileSearchSessionUpdate => "fuzzyFileSearch/sessionUpdate" {
        params: FuzzyFileSearchSessionUpdateParams,
        serialization: fuzzy_session_id(params.session_id),
        response: FuzzyFileSearchSessionUpdateResponse,
    },
    #[experimental("fuzzyFileSearch/sessionStop")]
    FuzzyFileSearchSessionStop => "fuzzyFileSearch/sessionStop" {
        params: FuzzyFileSearchSessionStopParams,
        serialization: fuzzy_session_id(params.session_id),
        response: FuzzyFileSearchSessionStopResponse,
    },
}

/// Generates an `enum ServerRequest` where each variant is a request that the
/// server can send to the client along with the corresponding params and
/// response types. It also generates helper types used by the app/server
/// infrastructure (payload enum, request constructor, and export helpers).
macro_rules! server_request_definitions {
    (
        $(
            $(#[experimental($reason:expr)])?
            $(#[doc = $variant_doc:literal])*
            $variant:ident $(=> $wire:literal)? {
                params: $params:ty,
                response: $response:ty,
            }
        ),* $(,)?
    ) => {
        /// Request initiated from the server and sent to the client.
        #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
        #[allow(clippy::large_enum_variant)]
        #[serde(tag = "method", rename_all = "camelCase")]
        pub enum ServerRequest {
            $(
                $(#[doc = $variant_doc])*
                $(#[serde(rename = $wire)] #[ts(rename = $wire)])?
                $variant {
                    #[serde(rename = "id")]
                    request_id: RequestId,
                    params: $params,
                },
            )*
        }

        impl ServerRequest {
            pub fn id(&self) -> &RequestId {
                match self {
                    $(Self::$variant { request_id, .. } => request_id,)*
                }
            }

            pub fn response_from_result(
                &self,
                result: crate::Result,
            ) -> serde_json::Result<ServerResponse> {
                match self {
                    $(
                        Self::$variant { request_id, .. } => {
                            let response = serde_json::from_value::<$response>(result)?;
                            Ok(ServerResponse::$variant {
                                request_id: request_id.clone(),
                                response,
                            })
                        }
                    )*
                }
            }
        }

        /// Typed response from the client to the server.
        #[derive(Serialize, Deserialize, Debug, Clone)]
        #[serde(tag = "method", rename_all = "camelCase")]
        pub enum ServerResponse {
            $(
                $(#[doc = $variant_doc])*
                $(#[serde(rename = $wire)])?
                $variant {
                    #[serde(rename = "id")]
                    request_id: RequestId,
                    response: $response,
                },
            )*
        }

        impl ServerResponse {
            pub fn id(&self) -> &RequestId {
                match self {
                    $(Self::$variant { request_id, .. } => request_id,)*
                }
            }

            pub fn method(&self) -> String {
                serde_json::to_value(self)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("method")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| "<unknown>".to_string())
            }
        }

        #[derive(Debug, Clone, PartialEq, JsonSchema)]
        #[allow(clippy::large_enum_variant)]
        pub enum ServerRequestPayload {
            $( $variant($params), )*
        }

        impl ServerRequestPayload {
            pub fn request_with_id(self, request_id: RequestId) -> ServerRequest {
                match self {
                    $(Self::$variant(params) => ServerRequest::$variant { request_id, params },)*
                }
            }
        }

        #[cfg(test)]
        pub(crate) const EXPERIMENTAL_SERVER_METHODS: &[&str] = &[
            $(
                experimental_method_entry!($(#[experimental($reason)])? $(=> $wire)?),
            )*
        ];
        #[cfg(test)]
        pub(crate) const EXPERIMENTAL_SERVER_METHOD_PARAM_TYPES: &[&str] = &[
            $(
                experimental_type_entry!($(#[experimental($reason)])? $params),
            )*
        ];
        #[cfg(test)]
        pub(crate) const EXPERIMENTAL_SERVER_METHOD_RESPONSE_TYPES: &[&str] = &[
            $(
                experimental_type_entry!($(#[experimental($reason)])? $response),
            )*
        ];

    };
}

/// Generates `ServerNotification` enum and helpers, including a JSON Schema
/// exporter for each notification.
macro_rules! server_notification_definitions {
    (
        $(
            $(#[$variant_meta:meta])*
            $variant:ident $(=> $wire:literal)? ( $payload:ty )
        ),* $(,)?
    ) => {
        /// Notification sent from the server to the client.
        #[derive(
            Serialize,
            Deserialize,
            Debug,
            Clone,
            JsonSchema,
            TS,
            Display,
            ExperimentalApi,
        )]
        #[allow(clippy::large_enum_variant)]
        #[serde(tag = "method", content = "params", rename_all = "camelCase")]
        #[strum(serialize_all = "camelCase")]
        pub enum ServerNotification {
            $(
                $(#[$variant_meta])*
                $(#[serde(rename = $wire)] #[ts(rename = $wire)] #[strum(serialize = $wire)])?
                $variant($payload),
            )*
        }

        impl ServerNotification {
            pub fn to_params(self) -> Result<serde_json::Value, serde_json::Error> {
                match self {
                    $(Self::$variant(params) => serde_json::to_value(params),)*
                }
            }
        }

        impl TryFrom<JSONRPCNotification> for ServerNotification {
            type Error = serde_json::Error;

            fn try_from(value: JSONRPCNotification) -> Result<Self, serde_json::Error> {
                serde_json::from_value(serde_json::to_value(value)?)
            }
        }

    };
}
/// Notifications sent from the client to the server.
macro_rules! client_notification_definitions {
    (
        $(
            $(#[$variant_meta:meta])*
            $variant:ident $( ( $payload:ty ) )?
        ),* $(,)?
    ) => {
        #[derive(Serialize, Deserialize, Debug, Clone, JsonSchema, TS, Display)]
        #[serde(tag = "method", content = "params", rename_all = "camelCase")]
        #[strum(serialize_all = "camelCase")]
        pub enum ClientNotification {
            $(
                $(#[$variant_meta])*
                $variant $( ( $payload ) )?,
            )*
        }

    };
}

impl TryFrom<JSONRPCRequest> for ServerRequest {
    type Error = serde_json::Error;

    fn try_from(value: JSONRPCRequest) -> Result<Self, Self::Error> {
        serde_json::from_value(serde_json::to_value(value)?)
    }
}

server_request_definitions! {
    /// EXPERIMENTAL - Request input from the user for a tool call.
    ToolRequestUserInput => "item/tool/requestUserInput" {
        params: v2::ToolRequestUserInputParams,
        response: v2::ToolRequestUserInputResponse,
    },

    /// Execute a dynamic tool call on the client.
    DynamicToolCall => "item/tool/call" {
        params: v2::DynamicToolCallParams,
        response: v2::DynamicToolCallResponse,
    },

    ChatgptAuthTokensRefresh => "account/chatgptAuthTokens/refresh" {
        params: v2::ChatgptAuthTokensRefreshParams,
        response: v2::ChatgptAuthTokensRefreshResponse,
    },

    /// Generate a fresh upstream attestation result on demand.
    AttestationGenerate => "attestation/generate" {
        params: v2::AttestationGenerateParams,
        response: v2::AttestationGenerateResponse,
    },

    #[experimental("currentTime/read")]
    /// Read the current time from an external clock owned by the client.
    CurrentTimeRead => "currentTime/read" {
        params: v2::CurrentTimeReadParams,
        response: v2::CurrentTimeReadResponse,
    },

}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct FuzzyFileSearchParams {
    pub query: String,
    pub roots: Vec<String>,
    // if provided, will cancel any previous request that used the same value
    pub cancellation_token: Option<String>,
}

/// Superset of [`codex_file_search::FileMatch`]
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
pub struct FuzzyFileSearchResult {
    pub root: String,
    pub path: String,
    pub match_type: FuzzyFileSearchMatchType,
    pub file_name: String,
    pub score: u32,
    pub indices: Option<Vec<u32>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub enum FuzzyFileSearchMatchType {
    File,
    Directory,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
pub struct FuzzyFileSearchResponse {
    pub files: Vec<FuzzyFileSearchResult>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct FuzzyFileSearchSessionStartParams {
    pub session_id: String,
    pub roots: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS, Default)]
pub struct FuzzyFileSearchSessionStartResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct FuzzyFileSearchSessionUpdateParams {
    pub session_id: String,
    pub query: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS, Default)]
pub struct FuzzyFileSearchSessionUpdateResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct FuzzyFileSearchSessionStopParams {
    pub session_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS, Default)]
pub struct FuzzyFileSearchSessionStopResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct FuzzyFileSearchSessionUpdatedNotification {
    pub session_id: String,
    pub query: String,
    pub files: Vec<FuzzyFileSearchResult>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct FuzzyFileSearchSessionCompletedNotification {
    pub session_id: String,
}

server_notification_definitions! {
    /// NEW NOTIFICATIONS
    Error => "error" (v2::ErrorNotification),
    ThreadStarted => "thread/started" (v2::ThreadStartedNotification),
    ThreadStatusChanged => "thread/status/changed" (v2::ThreadStatusChangedNotification),
    ThreadArchived => "thread/archived" (v2::ThreadArchivedNotification),
    ThreadDeleted => "thread/deleted" (v2::ThreadDeletedNotification),
    ThreadUnarchived => "thread/unarchived" (v2::ThreadUnarchivedNotification),
    ThreadClosed => "thread/closed" (v2::ThreadClosedNotification),
    SkillsChanged => "skills/changed" (v2::SkillsChangedNotification),
    ThreadNameUpdated => "thread/name/updated" (v2::ThreadNameUpdatedNotification),
    ThreadGoalUpdated => "thread/goal/updated" (v2::ThreadGoalUpdatedNotification),
    ThreadGoalCleared => "thread/goal/cleared" (v2::ThreadGoalClearedNotification),
    #[experimental("thread/environment/connected")]
    EnvironmentConnected => "thread/environment/connected" (v2::EnvironmentConnectionNotification),
    #[experimental("thread/environment/disconnected")]
    EnvironmentDisconnected => "thread/environment/disconnected" (v2::EnvironmentConnectionNotification),
    #[experimental("thread/settings/updated")]
    ThreadSettingsUpdated => "thread/settings/updated" (v2::ThreadSettingsUpdatedNotification),
    ThreadTokenUsageUpdated => "thread/tokenUsage/updated" (v2::ThreadTokenUsageUpdatedNotification),
    TurnStarted => "turn/started" (v2::TurnStartedNotification),
    TurnCompleted => "turn/completed" (v2::TurnCompletedNotification),
    TurnDiffUpdated => "turn/diff/updated" (v2::TurnDiffUpdatedNotification),
    TurnPlanUpdated => "turn/plan/updated" (v2::TurnPlanUpdatedNotification),
    ItemStarted => "item/started" (v2::ItemStartedNotification),
    ItemCompleted => "item/completed" (v2::ItemCompletedNotification),
    /// This event is internal-only. Used by Codex Cloud.
    RawResponseItemCompleted => "rawResponseItem/completed" (v2::RawResponseItemCompletedNotification),
    /// This event is internal-only. Used by clients that need exact upstream usage.
    RawResponseCompleted => "rawResponse/completed" (v2::RawResponseCompletedNotification),
    AgentMessageDelta => "item/agentMessage/delta" (v2::AgentMessageDeltaNotification),
    /// EXPERIMENTAL - proposed plan streaming deltas for plan items.
    /// Stream base64-encoded stdout/stderr chunks for a running `command/exec` session.
    CommandExecOutputDelta => "command/exec/outputDelta" (v2::CommandExecOutputDeltaNotification),
    /// Stream base64-encoded stdout/stderr chunks for a running `process/spawn` session.
    #[experimental("process/outputDelta")]
    ProcessOutputDelta => "process/outputDelta" (v2::ProcessOutputDeltaNotification),
    /// Final exit notification for a `process/spawn` session.
    #[experimental("process/exited")]
    ProcessExited => "process/exited" (v2::ProcessExitedNotification),
    CommandExecutionOutputDelta => "item/commandExecution/outputDelta" (v2::CommandExecutionOutputDeltaNotification),
    TerminalInteraction => "item/commandExecution/terminalInteraction" (v2::TerminalInteractionNotification),
    /// Deprecated legacy apply_patch output stream notification.
    FileChangeOutputDelta => "item/fileChange/outputDelta" (v2::FileChangeOutputDeltaNotification),
    FileChangePatchUpdated => "item/fileChange/patchUpdated" (v2::FileChangePatchUpdatedNotification),
    ServerRequestResolved => "serverRequest/resolved" (v2::ServerRequestResolvedNotification),
    AccountUpdated => "account/updated" (v2::AccountUpdatedNotification),
    AccountRateLimitsUpdated => "account/rateLimits/updated" (v2::AccountRateLimitsUpdatedNotification),
    FsChanged => "fs/changed" (v2::FsChangedNotification),
    ReasoningSummaryTextDelta => "item/reasoning/summaryTextDelta" (v2::ReasoningSummaryTextDeltaNotification),
    ReasoningSummaryPartAdded => "item/reasoning/summaryPartAdded" (v2::ReasoningSummaryPartAddedNotification),
    ReasoningTextDelta => "item/reasoning/textDelta" (v2::ReasoningTextDeltaNotification),
    /// Deprecated: Use `ContextCompaction` item type instead.
    ContextCompacted => "thread/compacted" (v2::ContextCompactedNotification),
    ModelRerouted => "model/rerouted" (v2::ModelReroutedNotification),
    ModelVerification => "model/verification" (v2::ModelVerificationNotification),
    #[experimental("turn/moderationMetadata")]
    TurnModerationMetadata => "turn/moderationMetadata" (v2::TurnModerationMetadataNotification),
    ModelSafetyBufferingUpdated => "model/safetyBuffering/updated" (v2::ModelSafetyBufferingUpdatedNotification),
    Warning => "warning" (v2::WarningNotification),
    DeprecationNotice => "deprecationNotice" (v2::DeprecationNoticeNotification),
    ConfigWarning => "configWarning" (v2::ConfigWarningNotification),
    FuzzyFileSearchSessionUpdated => "fuzzyFileSearch/sessionUpdated" (FuzzyFileSearchSessionUpdatedNotification),
    FuzzyFileSearchSessionCompleted => "fuzzyFileSearch/sessionCompleted" (FuzzyFileSearchSessionCompletedNotification),
    #[experimental("thread/realtime/started")]
    ThreadRealtimeStarted => "thread/realtime/started" (v2::ThreadRealtimeStartedNotification),
    #[experimental("thread/realtime/itemAdded")]
    ThreadRealtimeItemAdded => "thread/realtime/itemAdded" (v2::ThreadRealtimeItemAddedNotification),
    #[experimental("thread/realtime/transcript/delta")]
    ThreadRealtimeTranscriptDelta => "thread/realtime/transcript/delta" (v2::ThreadRealtimeTranscriptDeltaNotification),
    #[experimental("thread/realtime/transcript/done")]
    ThreadRealtimeTranscriptDone => "thread/realtime/transcript/done" (v2::ThreadRealtimeTranscriptDoneNotification),
    #[experimental("thread/realtime/outputAudio/delta")]
    ThreadRealtimeOutputAudioDelta => "thread/realtime/outputAudio/delta" (v2::ThreadRealtimeOutputAudioDeltaNotification),
    #[experimental("thread/realtime/sdp")]
    ThreadRealtimeSdp => "thread/realtime/sdp" (v2::ThreadRealtimeSdpNotification),
    #[experimental("thread/realtime/error")]
    ThreadRealtimeError => "thread/realtime/error" (v2::ThreadRealtimeErrorNotification),
    #[experimental("thread/realtime/closed")]
    ThreadRealtimeClosed => "thread/realtime/closed" (v2::ThreadRealtimeClosedNotification),

    #[serde(rename = "account/login/completed")]
    #[ts(rename = "account/login/completed")]
    #[strum(serialize = "account/login/completed")]
    AccountLoginCompleted(v2::AccountLoginCompletedNotification),

}

/// Server notification envelope sent over app-server transports.
///
/// `emitted_at_ms` records when app-server emitted the notification, before it
/// is fanned out to individual connections.
#[derive(Serialize, Deserialize, Debug, Clone, TS)]
#[serde(rename_all = "camelCase")]
pub struct ServerNotificationEnvelope {
    #[serde(flatten)]
    pub notification: ServerNotification,
    /// Unix timestamp (in milliseconds) when app-server emitted this notification.
    ///
    /// Optional so clients can decode notifications from older app-server
    /// versions. Current app-server versions always populate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    #[ts(type = "number")]
    pub emitted_at_ms: Option<i64>,
}

client_notification_definitions! {
    Initialized,
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use codex_protocol::ThreadId;
    use codex_protocol::account::PlanType;
    use codex_protocol::config_types::MultiAgentMode;
    use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_READ_ONLY;
    use codex_protocol::parse_command::ParsedCommand;
    use codex_protocol::protocol::CodexResponseHandoffMode;
    use codex_protocol::protocol::ConversationTextRole;
    use codex_protocol::protocol::RealtimeConversationVersion;
    use codex_protocol::protocol::RealtimeOutputModality;
    use codex_protocol::protocol::RealtimeVoice;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use codex_utils_absolute_path::test_support::PathBufExt;
    use codex_utils_absolute_path::test_support::test_path_buf;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::path::PathBuf;

    fn absolute_path_string(path: &str) -> String {
        let path = format!("/{}", path.trim_start_matches('/'));
        test_path_buf(&path).display().to_string()
    }

    fn absolute_path(path: &str) -> AbsolutePathBuf {
        let path = format!("/{}", path.trim_start_matches('/'));
        test_path_buf(&path).abs()
    }

    fn request_id() -> RequestId {
        const REQUEST_ID: i64 = 1;
        RequestId::Integer(REQUEST_ID)
    }

    fn decode_client_request_through_json(
        request: &JSONRPCRequest,
    ) -> std::result::Result<ClientRequest, String> {
        serde_json::to_value(request)
            .and_then(serde_json::from_value)
            .map_err(|err| err.to_string())
    }

    #[test]
    fn jsonrpc_request_conversion_preserves_serde_enum_decoding() {
        let requests = [
            JSONRPCRequest {
                id: RequestId::Integer(1),
                method: "thread/archive".to_string(),
                params: Some(json!({"threadId": "thread-1"})),
                trace: Some(codex_protocol::protocol::W3cTraceContext {
                    traceparent: Some("traceparent".to_string()),
                    tracestate: Some("tracestate".to_string()),
                }),
            },
            // Required params preserve distinct omitted and explicit-null errors.
            JSONRPCRequest {
                id: RequestId::Integer(2),
                method: "thread/archive".to_string(),
                params: None,
                trace: None,
            },
            JSONRPCRequest {
                id: RequestId::Integer(3),
                method: "thread/archive".to_string(),
                params: Some(serde_json::Value::Null),
                trace: None,
            },
            // Optional unit params preserve omitted, null, and empty-object behavior.
            JSONRPCRequest {
                id: RequestId::Integer(4),
                method: "memory/reset".to_string(),
                params: None,
                trace: None,
            },
            JSONRPCRequest {
                id: RequestId::Integer(5),
                method: "memory/reset".to_string(),
                params: Some(serde_json::Value::Null),
                trace: None,
            },
            JSONRPCRequest {
                id: RequestId::Integer(6),
                method: "memory/reset".to_string(),
                params: Some(json!({})),
                trace: None,
            },
            JSONRPCRequest {
                id: RequestId::Integer(7),
                method: "getConversationSummary".to_string(),
                params: Some(json!({
                    "conversationId": "67e55044-10b1-426f-9247-bb680e5fe0c8"
                })),
                trace: None,
            },
            JSONRPCRequest {
                id: RequestId::Integer(8),
                method: "unknown/method".to_string(),
                params: Some(json!({})),
                trace: None,
            },
        ];

        for request in requests {
            let expected = decode_client_request_through_json(&request);
            let actual = ClientRequest::try_from(request).map_err(|err| err.to_string());
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn thread_section_move_round_trips_and_serializes_by_thread() -> Result<()> {
        assert_eq!(
            serde_json::to_value(v2::ThreadSortKey::SectionPosition)?,
            json!("section_position")
        );
        let request = ClientRequest::ThreadSectionMove {
            request_id: request_id(),
            params: v2::ThreadSectionMoveParams {
                thread_id: "thread-1".to_string(),
                section_id: Some("01984de2-8f74-7c91-a3b2-5c5e937cf318".to_string()),
                before_thread_id: Some("thread-2".to_string()),
            },
        };
        assert_eq!(
            serde_json::to_value(&request)?,
            json!({
                "method": "thread/section/move",
                "id": 1,
                "params": {
                    "threadId": "thread-1",
                    "sectionId": "01984de2-8f74-7c91-a3b2-5c5e937cf318",
                    "beforeThreadId": "thread-2"
                }
            })
        );
        assert_eq!(
            request.serialization_scope(),
            Some(ClientRequestSerializationScope::Thread {
                thread_id: "thread-1".to_string()
            })
        );

        let append_request = ClientRequest::try_from(JSONRPCRequest {
            id: request_id(),
            method: "thread/section/move".to_string(),
            params: Some(json!({
                "threadId": "thread-1",
                "sectionId": "01984de2-8f74-7c91-a3b2-5c5e937cf318"
            })),
            trace: None,
        })?;
        assert_eq!(
            append_request,
            ClientRequest::ThreadSectionMove {
                request_id: request_id(),
                params: v2::ThreadSectionMoveParams {
                    thread_id: "thread-1".to_string(),
                    section_id: Some("01984de2-8f74-7c91-a3b2-5c5e937cf318".to_string()),
                    before_thread_id: None,
                },
            }
        );

        let clear_request = ClientRequest::try_from(JSONRPCRequest {
            id: request_id(),
            method: "thread/section/move".to_string(),
            params: Some(json!({
                "threadId": "thread-1",
                "sectionId": null
            })),
            trace: None,
        })?;
        assert_eq!(
            clear_request,
            ClientRequest::ThreadSectionMove {
                request_id: request_id(),
                params: v2::ThreadSectionMoveParams {
                    thread_id: "thread-1".to_string(),
                    section_id: None,
                    before_thread_id: None,
                },
            }
        );
        assert!(
            ClientRequest::try_from(JSONRPCRequest {
                id: request_id(),
                method: "thread/section/move".to_string(),
                params: Some(json!({ "threadId": "thread-1" })),
                trace: None,
            })
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn client_request_serialization_scope_covers_keyed_families() {
        let thread_id = "thread-1".to_string();
        let thread_resume = ClientRequest::ThreadResume {
            request_id: request_id(),
            params: v2::ThreadResumeParams {
                thread_id: thread_id.clone(),
                ..Default::default()
            },
        };
        assert_eq!(
            thread_resume.serialization_scope(),
            Some(ClientRequestSerializationScope::Thread {
                thread_id: thread_id.clone()
            })
        );

        let thread_resume_with_path = ClientRequest::ThreadResume {
            request_id: request_id(),
            params: v2::ThreadResumeParams {
                thread_id: thread_id.clone(),
                path: Some(PathBuf::from("/tmp/resume-thread.jsonl")),
                ..Default::default()
            },
        };
        assert_eq!(
            thread_resume_with_path.serialization_scope(),
            Some(ClientRequestSerializationScope::Thread {
                thread_id: thread_id.clone()
            })
        );

        let thread_fork = ClientRequest::ThreadFork {
            request_id: request_id(),
            params: v2::ThreadForkParams {
                thread_id: thread_id.clone(),
                path: Some(PathBuf::from("/tmp/source-thread.jsonl")),
                ..Default::default()
            },
        };
        assert_eq!(
            thread_fork.serialization_scope(),
            Some(ClientRequestSerializationScope::Thread { thread_id })
        );

        let command_exec = ClientRequest::OneOffCommandExec {
            request_id: request_id(),
            params: v2::CommandExecParams {
                command: vec!["sleep".to_string(), "10".to_string()],
                process_id: Some("proc-1".to_string()),
                tty: false,
                stream_stdin: false,
                stream_stdout_stderr: false,
                output_bytes_cap: None,
                disable_output_cap: false,
                disable_timeout: false,
                timeout_ms: None,
                cwd: None,
                env: None,
                size: None,
                sandbox_policy: None,
                permission_profile: None,
            },
        };
        assert_eq!(
            command_exec.serialization_scope(),
            Some(ClientRequestSerializationScope::CommandExecProcess {
                process_id: "proc-1".to_string()
            })
        );

        let fuzzy_update = ClientRequest::FuzzyFileSearchSessionUpdate {
            request_id: request_id(),
            params: FuzzyFileSearchSessionUpdateParams {
                session_id: "search-1".to_string(),
                query: "lib".to_string(),
            },
        };
        assert_eq!(
            fuzzy_update.serialization_scope(),
            Some(ClientRequestSerializationScope::FuzzyFileSearchSession {
                session_id: "search-1".to_string()
            })
        );

        let fs_watch = ClientRequest::FsWatch {
            request_id: request_id(),
            params: v2::FsWatchParams {
                watch_id: "watch-1".to_string(),
                path: absolute_path("/tmp/repo"),
            },
        };
        assert_eq!(
            fs_watch.serialization_scope(),
            Some(ClientRequestSerializationScope::FsWatch {
                watch_id: "watch-1".to_string()
            })
        );

        let skills_list = ClientRequest::SkillsList {
            request_id: request_id(),
            params: v2::SkillsListParams {
                cwds: Vec::new(),
                force_reload: false,
            },
        };
        assert_eq!(
            skills_list.serialization_scope(),
            Some(ClientRequestSerializationScope::GlobalSharedRead("config"))
        );

        let skills_extra_roots_set = ClientRequest::SkillsExtraRootsSet {
            request_id: request_id(),
            params: v2::SkillsExtraRootsSetParams {
                extra_roots: vec![absolute_path("/tmp/skills")],
            },
        };
        assert_eq!(
            skills_extra_roots_set.serialization_scope(),
            Some(ClientRequestSerializationScope::Global("config"))
        );

        let config_read = ClientRequest::ConfigRead {
            request_id: request_id(),
            params: v2::ConfigReadParams {
                include_layers: false,
                cwd: None,
            },
        };
        assert_eq!(
            config_read.serialization_scope(),
            Some(ClientRequestSerializationScope::GlobalSharedRead("config"))
        );

        let account_read = ClientRequest::GetAccount {
            request_id: request_id(),
            params: v2::GetAccountParams {
                refresh_token: false,
            },
        };
        assert_eq!(
            account_read.serialization_scope(),
            Some(ClientRequestSerializationScope::Global("account-auth"))
        );

        let thread_goal_set = ClientRequest::ThreadGoalSet {
            request_id: request_id(),
            params: v2::ThreadGoalSetParams {
                thread_id: "goal-thread".to_string(),
                objective: Some("ship it".to_string()),
                status: None,
                token_budget: None,
            },
        };
        assert_eq!(
            thread_goal_set.serialization_scope(),
            Some(ClientRequestSerializationScope::Thread {
                thread_id: "goal-thread".to_string()
            })
        );

        let guardian_approval = ClientRequest::ThreadApproveGuardianDeniedAction {
            request_id: request_id(),
            params: v2::ThreadApproveGuardianDeniedActionParams {
                thread_id: "guardian-thread".to_string(),
                event: json!({ "type": "guardian" }),
            },
        };
        assert_eq!(
            guardian_approval.serialization_scope(),
            Some(ClientRequestSerializationScope::Thread {
                thread_id: "guardian-thread".to_string()
            })
        );

        let add_credits_nudge = ClientRequest::SendAddCreditsNudgeEmail {
            request_id: request_id(),
            params: v2::SendAddCreditsNudgeEmailParams {
                credit_type: v2::AddCreditsNudgeCreditType::Credits,
            },
        };
        assert_eq!(
            add_credits_nudge.serialization_scope(),
            Some(ClientRequestSerializationScope::Global("account-auth"))
        );

        let environment_add = ClientRequest::EnvironmentAdd {
            request_id: request_id(),
            params: v2::EnvironmentAddParams {
                environment_id: "remote-a".to_string(),
                exec_server_url: "ws://127.0.0.1:8765".to_string(),
                connect_timeout_ms: None,
            },
        };
        assert_eq!(
            environment_add.serialization_scope(),
            Some(ClientRequestSerializationScope::Global("environment"))
        );
    }

    #[test]
    fn client_request_serialization_scope_covers_unkeyed_representatives() {
        let initialize = ClientRequest::Initialize {
            request_id: request_id(),
            params: v1::InitializeParams {
                client_info: v1::ClientInfo {
                    name: "test".to_string(),
                    title: None,
                    version: "0.1.0".to_string(),
                },
                capabilities: None,
            },
        };
        assert_eq!(initialize.serialization_scope(), None);

        let thread_start = ClientRequest::ThreadStart {
            request_id: request_id(),
            params: v2::ThreadStartParams::default(),
        };
        assert_eq!(thread_start.serialization_scope(), None);

        let command_exec = ClientRequest::OneOffCommandExec {
            request_id: request_id(),
            params: v2::CommandExecParams {
                command: vec!["true".to_string()],
                process_id: None,
                tty: false,
                stream_stdin: false,
                stream_stdout_stderr: false,
                output_bytes_cap: None,
                disable_output_cap: false,
                disable_timeout: false,
                timeout_ms: None,
                cwd: None,
                env: None,
                size: None,
                sandbox_policy: None,
                permission_profile: None,
            },
        };
        assert_eq!(command_exec.serialization_scope(), None);

        let fs_read = ClientRequest::FsReadFile {
            request_id: request_id(),
            params: v2::FsReadFileParams {
                path: absolute_path("/tmp/file.txt"),
            },
        };
        assert_eq!(fs_read.serialization_scope(), None);

        let thread_turns_list = ClientRequest::ThreadTurnsList {
            request_id: request_id(),
            params: v2::ThreadTurnsListParams {
                thread_id: "thread-1".to_string(),
                cursor: None,
                limit: None,
                sort_direction: None,
                items_view: None,
            },
        };
        assert_eq!(thread_turns_list.serialization_scope(), None);

        let thread_items_list = ClientRequest::ThreadItemsList {
            request_id: request_id(),
            params: v2::ThreadItemsListParams {
                thread_id: "thread-1".to_string(),
                turn_id: None,
                cursor: None,
                limit: None,
                sort_direction: None,
            },
        };
        assert_eq!(thread_items_list.serialization_scope(), None);
    }

    #[test]
    fn serialize_get_conversation_summary() -> Result<()> {
        let request = ClientRequest::GetConversationSummary {
            request_id: RequestId::Integer(42),
            params: v1::GetConversationSummaryParams::ThreadId {
                conversation_id: ThreadId::from_string("67e55044-10b1-426f-9247-bb680e5fe0c8")?,
            },
        };
        assert_eq!(
            json!({
                "method": "getConversationSummary",
                "id": 42,
                "params": {
                    "conversationId": "67e55044-10b1-426f-9247-bb680e5fe0c8"
                }
            }),
            serde_json::to_value(&request)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_initialize_capabilities() -> Result<()> {
        let request = ClientRequest::Initialize {
            request_id: RequestId::Integer(42),
            params: v1::InitializeParams {
                client_info: v1::ClientInfo {
                    name: "codex_vscode".to_string(),
                    title: Some("Codex VS Code Extension".to_string()),
                    version: "0.1.0".to_string(),
                },
                capabilities: Some(v1::InitializeCapabilities {
                    experimental_api: true,
                    request_attestation: true,
                    opt_out_notification_methods: Some(vec![
                        "thread/started".to_string(),
                        "item/agentMessage/delta".to_string(),
                    ]),
                }),
            },
        };

        assert_eq!(
            json!({
                "method": "initialize",
                "id": 42,
                "params": {
                    "clientInfo": {
                        "name": "codex_vscode",
                        "title": "Codex VS Code Extension",
                        "version": "0.1.0"
                    },
                    "capabilities": {
                        "experimentalApi": true,
                        "requestAttestation": true,
                        "optOutNotificationMethods": [
                            "thread/started",
                            "item/agentMessage/delta"
                        ]
                    }
                }
            }),
            serde_json::to_value(&request)?,
        );
        Ok(())
    }

    #[test]
    fn deserialize_initialize_capabilities() -> Result<()> {
        let request: ClientRequest = serde_json::from_value(json!({
            "method": "initialize",
            "id": 42,
            "params": {
                "clientInfo": {
                    "name": "codex_vscode",
                    "title": "Codex VS Code Extension",
                    "version": "0.1.0"
                },
                "capabilities": {
                    "experimentalApi": true,
                    "requestAttestation": true,
                    "optOutNotificationMethods": [
                        "thread/started",
                        "item/agentMessage/delta"
                    ]
                }
            }
        }))?;

        assert_eq!(
            request,
            ClientRequest::Initialize {
                request_id: RequestId::Integer(42),
                params: v1::InitializeParams {
                    client_info: v1::ClientInfo {
                        name: "codex_vscode".to_string(),
                        title: Some("Codex VS Code Extension".to_string()),
                        version: "0.1.0".to_string(),
                    },
                    capabilities: Some(v1::InitializeCapabilities {
                        experimental_api: true,
                        request_attestation: true,
                        opt_out_notification_methods: Some(vec![
                            "thread/started".to_string(),
                            "item/agentMessage/delta".to_string(),
                        ]),
                    }),
                },
            }
        );
        Ok(())
    }

    #[test]
    fn conversation_id_serializes_as_plain_string() -> Result<()> {
        let id = ThreadId::from_string("67e55044-10b1-426f-9247-bb680e5fe0c8")?;

        assert_eq!(
            json!("67e55044-10b1-426f-9247-bb680e5fe0c8"),
            serde_json::to_value(id)?
        );
        Ok(())
    }

    #[test]
    fn conversation_id_deserializes_from_plain_string() -> Result<()> {
        let id: ThreadId = serde_json::from_value(json!("67e55044-10b1-426f-9247-bb680e5fe0c8"))?;

        assert_eq!(
            ThreadId::from_string("67e55044-10b1-426f-9247-bb680e5fe0c8")?,
            id,
        );
        Ok(())
    }

    #[test]
    fn serialize_client_notification() -> Result<()> {
        let notification = ClientNotification::Initialized;
        // Note there is no "params" field for this notification.
        assert_eq!(
            json!({
                "method": "initialized",
            }),
            serde_json::to_value(&notification)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_server_request() -> Result<()> {
        let conversation_id = ThreadId::from_string("67e55044-10b1-426f-9247-bb680e5fe0c8")?;
        let params = v1::ExecCommandApprovalParams {
            conversation_id,
            call_id: "call-42".to_string(),
            approval_id: Some("approval-42".to_string()),
            command: vec!["echo".to_string(), "hello".to_string()],
            cwd: PathBuf::from("/tmp"),
            reason: Some("because tests".to_string()),
            parsed_cmd: vec![ParsedCommand::Unknown {
                cmd: "echo hello".to_string(),
            }],
        };
        let request = ServerRequest::ExecCommandApproval {
            request_id: RequestId::Integer(7),
            params: params.clone(),
        };

        assert_eq!(
            json!({
                "method": "execCommandApproval",
                "id": 7,
                "params": {
                    "conversationId": "67e55044-10b1-426f-9247-bb680e5fe0c8",
                    "callId": "call-42",
                    "approvalId": "approval-42",
                    "command": ["echo", "hello"],
                    "cwd": "/tmp",
                    "reason": "because tests",
                    "parsedCmd": [
                        {
                            "type": "unknown",
                            "cmd": "echo hello"
                        }
                    ]
                }
            }),
            serde_json::to_value(&request)?,
        );

        let payload = ServerRequestPayload::ExecCommandApproval(params);
        assert_eq!(request.id(), &RequestId::Integer(7));
        assert_eq!(payload.request_with_id(RequestId::Integer(7)), request);
        Ok(())
    }

    #[test]
    fn serialize_chatgpt_auth_tokens_refresh_request() -> Result<()> {
        let request = ServerRequest::ChatgptAuthTokensRefresh {
            request_id: RequestId::Integer(8),
            params: v2::ChatgptAuthTokensRefreshParams {
                reason: v2::ChatgptAuthTokensRefreshReason::Unauthorized,
                previous_account_id: Some("org-123".to_string()),
            },
        };
        assert_eq!(
            json!({
                "method": "account/chatgptAuthTokens/refresh",
                "id": 8,
                "params": {
                    "reason": "unauthorized",
                    "previousAccountId": "org-123"
                }
            }),
            serde_json::to_value(&request)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_attestation_generate_request() -> Result<()> {
        let params = v2::AttestationGenerateParams {};
        let request = ServerRequest::AttestationGenerate {
            request_id: RequestId::Integer(9),
            params: params.clone(),
        };
        assert_eq!(
            json!({
                "method": "attestation/generate",
                "id": 9,
                "params": {}
            }),
            serde_json::to_value(&request)?,
        );

        let payload = ServerRequestPayload::AttestationGenerate(params);
        assert_eq!(request.id(), &RequestId::Integer(9));
        assert_eq!(payload.request_with_id(RequestId::Integer(9)), request);
        Ok(())
    }

    #[test]
    fn serialize_current_time_read_request() -> Result<()> {
        let params = v2::CurrentTimeReadParams {
            thread_id: "thread-123".to_string(),
        };
        let request = ServerRequest::CurrentTimeRead {
            request_id: RequestId::Integer(10),
            params: params.clone(),
        };
        assert_eq!(
            json!({
                "method": "currentTime/read",
                "id": 10,
                "params": {
                    "threadId": "thread-123"
                }
            }),
            serde_json::to_value(&request)?,
        );

        let payload = ServerRequestPayload::CurrentTimeRead(params);
        assert_eq!(request.id(), &RequestId::Integer(10));
        assert_eq!(payload.request_with_id(RequestId::Integer(10)), request);
        Ok(())
    }

    #[test]
    fn serialize_server_response() -> Result<()> {
        let response = ServerResponse::CommandExecutionRequestApproval {
            request_id: RequestId::Integer(8),
            response: v2::CommandExecutionRequestApprovalResponse {
                decision: v2::CommandExecutionApprovalDecision::AcceptForSession,
            },
        };

        assert_eq!(response.id(), &RequestId::Integer(8));
        assert_eq!(response.method(), "item/commandExecution/requestApproval");
        assert_eq!(
            json!({
                "method": "item/commandExecution/requestApproval",
                "id": 8,
                "response": {
                    "decision": "acceptForSession"
                }
            }),
            serde_json::to_value(&response)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_get_account_rate_limits() -> Result<()> {
        let request = ClientRequest::GetAccountRateLimits {
            request_id: RequestId::Integer(1),
            params: None,
        };
        assert_eq!(request.id(), &RequestId::Integer(1));
        assert_eq!(
            json!({
                "method": "account/rateLimits/read",
                "id": 1,
            }),
            serde_json::to_value(&request)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_get_account_token_usage() -> Result<()> {
        let request = ClientRequest::GetAccountTokenUsage {
            request_id: RequestId::Integer(1),
            params: None,
        };
        assert_eq!(request.id(), &RequestId::Integer(1));
        assert_eq!(
            json!({
                "method": "account/usage/read",
                "id": 1,
            }),
            serde_json::to_value(&request)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_get_workspace_messages() -> Result<()> {
        let request = ClientRequest::GetWorkspaceMessages {
            request_id: RequestId::Integer(1),
            params: None,
        };
        assert_eq!(request.id(), &RequestId::Integer(1));
        assert_eq!(
            json!({
                "method": "account/workspaceMessages/read",
                "id": 1,
            }),
            serde_json::to_value(&request)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_client_response() -> Result<()> {
        let cwd = absolute_path("/tmp");
        let response = ClientResponse::ThreadStart {
            request_id: RequestId::Integer(7),
            response: v2::ThreadStartResponse {
                thread: v2::Thread {
                    id: "67e55044-10b1-426f-9247-bb680e5fe0c8".to_string(),
                    extra: None,
                    session_id: "67e55044-10b1-426f-9247-bb680e5fe0c7".to_string(),
                    forked_from_id: None,
                    parent_thread_id: None,
                    preview: "first prompt".to_string(),
                    ephemeral: true,
                    section: None,
                    section_entered_at: None,
                    history_mode: Default::default(),
                    model_provider: "openai".to_string(),
                    created_at: 1,
                    updated_at: 2,
                    recency_at: Some(3),
                    status: v2::ThreadStatus::Idle,
                    path: None,
                    cwd: cwd.clone(),
                    cli_version: "0.0.0".to_string(),
                    source: v2::SessionSource::Exec,
                    can_accept_direct_input: None,
                    thread_source: None,
                    agent_nickname: None,
                    agent_role: None,
                    git_info: None,
                    name: None,
                    turns: Vec::new(),
                },
                model: "gpt-5".to_string(),
                model_provider: "openai".to_string(),
                service_tier: None,
                cwd,
                runtime_workspace_roots: Vec::new(),
                instruction_sources: vec![
                    codex_utils_path_uri::LegacyAppPathString::from_abs_path(&absolute_path(
                        "/tmp/AGENTS.md",
                    )),
                ],
                approval_policy: v2::AskForApproval::OnRequest,
                approvals_reviewer: v2::ApprovalsReviewer::User,
                sandbox: v2::SandboxPolicy::DangerFullAccess,
                active_permission_profile: None,
                reasoning_effort: None,
                multi_agent_mode: MultiAgentMode::ExplicitRequestOnly,
            },
        };

        assert_eq!(response.id(), &RequestId::Integer(7));
        assert_eq!(response.method(), "thread/start");
        assert_eq!(
            json!({
                "method": "thread/start",
                "id": 7,
                "response": {
                    "thread": {
                        "id": "67e55044-10b1-426f-9247-bb680e5fe0c8",
                        "extra": null,
                        "sessionId": "67e55044-10b1-426f-9247-bb680e5fe0c7",
                        "forkedFromId": null,
                        "parentThreadId": null,
                        "preview": "first prompt",
                        "ephemeral": true,
                        "section": null,
                        "sectionEnteredAt": null,
                        "historyMode": "legacy",
                        "modelProvider": "openai",
                        "createdAt": 1,
                        "updatedAt": 2,
                        "recencyAt": 3,
                        "status": {
                            "type": "idle"
                        },
                        "path": null,
                        "cwd": absolute_path_string("tmp"),
                        "cliVersion": "0.0.0",
                        "source": "exec",
                        "canAcceptDirectInput": null,
                        "threadSource": null,
                        "agentNickname": null,
                        "agentRole": null,
                        "gitInfo": null,
                        "name": null,
                        "turns": []
                    },
                    "model": "gpt-5",
                    "modelProvider": "openai",
                    "serviceTier": null,
                    "cwd": absolute_path_string("tmp"),
                    "runtimeWorkspaceRoots": [],
                    "instructionSources": [absolute_path_string("tmp/AGENTS.md")],
                    "approvalPolicy": "on-request",
                    "approvalsReviewer": "user",
                    "sandbox": {
                        "type": "dangerFullAccess"
                    },
                    "activePermissionProfile": null,
                    "reasoningEffort": null,
                    "multiAgentMode": "explicitRequestOnly"
                }
            }),
            serde_json::to_value(&response)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_config_requirements_read() -> Result<()> {
        let request = ClientRequest::ConfigRequirementsRead {
            request_id: RequestId::Integer(1),
            params: None,
        };
        assert_eq!(
            json!({
                "method": "configRequirements/read",
                "id": 1,
            }),
            serde_json::to_value(&request)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_account_login_api_key() -> Result<()> {
        let request = ClientRequest::LoginAccount {
            request_id: RequestId::Integer(2),
            params: v2::LoginAccountParams::ApiKey {
                api_key: "secret".to_string(),
            },
        };
        assert_eq!(
            json!({
                "method": "account/login/start",
                "id": 2,
                "params": {
                    "type": "apiKey",
                    "apiKey": "secret"
                }
            }),
            serde_json::to_value(&request)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_account_login_chatgpt() -> Result<()> {
        let request = ClientRequest::LoginAccount {
            request_id: RequestId::Integer(3),
            params: v2::LoginAccountParams::Chatgpt {
                app_brand: None,
                codex_streamlined_login: false,
                use_hosted_login_success_page: false,
            },
        };
        assert_eq!(
            json!({
                "method": "account/login/start",
                "id": 3,
                "params": {
                    "type": "chatgpt",
                    "appBrand": null
                }
            }),
            serde_json::to_value(&request)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_account_login_chatgpt_streamlined() -> Result<()> {
        let request = ClientRequest::LoginAccount {
            request_id: RequestId::Integer(3),
            params: v2::LoginAccountParams::Chatgpt {
                app_brand: None,
                codex_streamlined_login: true,
                use_hosted_login_success_page: false,
            },
        };
        assert_eq!(
            json!({
                "method": "account/login/start",
                "id": 3,
                "params": {
                    "type": "chatgpt",
                    "appBrand": null,
                    "codexStreamlinedLogin": true
                }
            }),
            serde_json::to_value(&request)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_account_login_chatgpt_with_hosted_success_page() -> Result<()> {
        let request = ClientRequest::LoginAccount {
            request_id: RequestId::Integer(3),
            params: v2::LoginAccountParams::Chatgpt {
                app_brand: Some(v2::LoginAppBrand::Chatgpt),
                codex_streamlined_login: true,
                use_hosted_login_success_page: true,
            },
        };
        assert_eq!(
            json!({
                "method": "account/login/start",
                "id": 3,
                "params": {
                    "type": "chatgpt",
                    "appBrand": "chatgpt",
                    "codexStreamlinedLogin": true,
                    "useHostedLoginSuccessPage": true
                }
            }),
            serde_json::to_value(&request)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_account_login_chatgpt_device_code() -> Result<()> {
        let request = ClientRequest::LoginAccount {
            request_id: RequestId::Integer(4),
            params: v2::LoginAccountParams::ChatgptDeviceCode,
        };
        assert_eq!(
            json!({
                "method": "account/login/start",
                "id": 4,
                "params": {
                    "type": "chatgptDeviceCode"
                }
            }),
            serde_json::to_value(&request)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_account_logout() -> Result<()> {
        let request = ClientRequest::LogoutAccount {
            request_id: RequestId::Integer(5),
            params: None,
        };
        assert_eq!(
            json!({
                "method": "account/logout",
                "id": 5,
            }),
            serde_json::to_value(&request)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_account_login_chatgpt_auth_tokens() -> Result<()> {
        let request = ClientRequest::LoginAccount {
            request_id: RequestId::Integer(6),
            params: v2::LoginAccountParams::ChatgptAuthTokens {
                access_token: "access-token".to_string(),
                chatgpt_account_id: "org-123".to_string(),
                chatgpt_plan_type: Some("business".to_string()),
            },
        };
        assert_eq!(
            json!({
                "method": "account/login/start",
                "id": 6,
                "params": {
                    "type": "chatgptAuthTokens",
                    "accessToken": "access-token",
                    "chatgptAccountId": "org-123",
                    "chatgptPlanType": "business"
                }
            }),
            serde_json::to_value(&request)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_get_account() -> Result<()> {
        let request = ClientRequest::GetAccount {
            request_id: RequestId::Integer(6),
            params: v2::GetAccountParams {
                refresh_token: false,
            },
        };
        assert_eq!(
            json!({
                "method": "account/read",
                "id": 6,
                "params": {}
            }),
            serde_json::to_value(&request)?,
        );
        let request = ClientRequest::GetAccount {
            request_id: RequestId::Integer(7),
            params: v2::GetAccountParams {
                refresh_token: true,
            },
        };
        assert_eq!(
            json!({
                "method": "account/read",
                "id": 7,
                "params": {
                    "refreshToken": true
                }
            }),
            serde_json::to_value(&request)?,
        );
        Ok(())
    }

    #[test]
    fn account_serializes_fields_in_camel_case() -> Result<()> {
        let api_key = v2::Account::ApiKey {};
        assert_eq!(
            json!({
                "type": "apiKey",
            }),
            serde_json::to_value(&api_key)?,
        );

        let chatgpt = v2::Account::Chatgpt {
            email: Some("user@example.com".to_string()),
            plan_type: PlanType::Plus,
        };
        assert_eq!(
            json!({
                "type": "chatgpt",
                "email": "user@example.com",
                "planType": "plus",
            }),
            serde_json::to_value(&chatgpt)?,
        );

        let chatgpt_without_email = v2::Account::Chatgpt {
            email: None,
            plan_type: PlanType::Pro,
        };
        assert_eq!(
            json!({
                "type": "chatgpt",
                "email": null,
                "planType": "pro",
            }),
            serde_json::to_value(&chatgpt_without_email)?,
        );

        Ok(())
    }

    #[test]
    fn serialize_list_models() -> Result<()> {
        let request = ClientRequest::ModelList {
            request_id: RequestId::Integer(6),
            params: v2::ModelListParams::default(),
        };
        assert_eq!(
            json!({
                "method": "model/list",
                "id": 6,
                "params": {
                    "limit": null,
                    "cursor": null,
                    "includeHidden": null
                }
            }),
            serde_json::to_value(&request)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_environment_add() -> Result<()> {
        let request = ClientRequest::EnvironmentAdd {
            request_id: RequestId::Integer(9),
            params: v2::EnvironmentAddParams {
                environment_id: "remote-a".to_string(),
                exec_server_url: "ws://127.0.0.1:8765".to_string(),
                connect_timeout_ms: Some(300_000),
            },
        };
        assert_eq!(
            json!({
                "method": "environment/add",
                "id": 9,
                "params": {
                    "environmentId": "remote-a",
                    "execServerUrl": "ws://127.0.0.1:8765",
                    "connectTimeoutMs": 300000
                }
            }),
            serde_json::to_value(&request)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_fs_get_metadata() -> Result<()> {
        let request = ClientRequest::FsGetMetadata {
            request_id: RequestId::Integer(10),
            params: v2::FsGetMetadataParams {
                path: absolute_path("tmp/example"),
            },
        };
        assert_eq!(
            json!({
                "method": "fs/getMetadata",
                "id": 10,
                "params": {
                    "path": absolute_path_string("tmp/example")
                }
            }),
            serde_json::to_value(&request)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_fs_watch() -> Result<()> {
        let request = ClientRequest::FsWatch {
            request_id: RequestId::Integer(10),
            params: v2::FsWatchParams {
                watch_id: "watch-git".to_string(),
                path: absolute_path("tmp/repo/.git"),
            },
        };
        assert_eq!(
            json!({
                "method": "fs/watch",
                "id": 10,
                "params": {
                    "watchId": "watch-git",
                    "path": absolute_path_string("tmp/repo/.git")
                }
            }),
            serde_json::to_value(&request)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_list_experimental_features() -> Result<()> {
        let request = ClientRequest::ExperimentalFeatureList {
            request_id: RequestId::Integer(8),
            params: v2::ExperimentalFeatureListParams::default(),
        };
        assert_eq!(
            json!({
                "method": "experimentalFeature/list",
                "id": 8,
                "params": {
                    "cursor": null,
                    "limit": null,
                    "threadId": null
                }
            }),
            serde_json::to_value(&request)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_list_experimental_features_with_thread_id() -> Result<()> {
        let request = ClientRequest::ExperimentalFeatureList {
            request_id: RequestId::Integer(8),
            params: v2::ExperimentalFeatureListParams {
                cursor: Some("3".to_string()),
                limit: Some(2),
                thread_id: Some("00000000-0000-4000-8000-000000000001".to_string()),
            },
        };
        assert_eq!(
            json!({
                "method": "experimentalFeature/list",
                "id": 8,
                "params": {
                    "cursor": "3",
                    "limit": 2,
                    "threadId": "00000000-0000-4000-8000-000000000001"
                }
            }),
            serde_json::to_value(&request)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_thread_background_terminals_clean() -> Result<()> {
        let request = ClientRequest::ThreadBackgroundTerminalsClean {
            request_id: RequestId::Integer(8),
            params: v2::ThreadBackgroundTerminalsCleanParams {
                thread_id: "thr_123".to_string(),
            },
        };
        assert_eq!(
            json!({
                "method": "thread/backgroundTerminals/clean",
                "id": 8,
                "params": {
                    "threadId": "thr_123"
                }
            }),
            serde_json::to_value(&request)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_thread_background_terminals_list() -> Result<()> {
        let request = ClientRequest::ThreadBackgroundTerminalsList {
            request_id: RequestId::Integer(8),
            params: v2::ThreadBackgroundTerminalsListParams {
                thread_id: "thr_123".to_string(),
                cursor: None,
                limit: None,
            },
        };
        assert_eq!(
            json!({
                "method": "thread/backgroundTerminals/list",
                "id": 8,
                "params": {
                    "threadId": "thr_123",
                    "cursor": null,
                    "limit": null
                }
            }),
            serde_json::to_value(&request)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_thread_background_terminals_terminate() -> Result<()> {
        let request = ClientRequest::ThreadBackgroundTerminalsTerminate {
            request_id: RequestId::Integer(8),
            params: v2::ThreadBackgroundTerminalsTerminateParams {
                thread_id: "thr_123".to_string(),
                process_id: "42".to_string(),
            },
        };
        assert_eq!(
            json!({
                "method": "thread/backgroundTerminals/terminate",
                "id": 8,
                "params": {
                    "threadId": "thr_123",
                    "processId": "42"
                }
            }),
            serde_json::to_value(&request)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_thread_realtime_start() -> Result<()> {
        let request = ClientRequest::ThreadRealtimeStart {
            request_id: RequestId::Integer(9),
            params: v2::ThreadRealtimeStartParams {
                client_managed_handoffs: Some(true),
                delegation_ack_filler: Some(false),
                flush_transcript_tail_on_session_end: Some(true),
                codex_responses_as_items: None,
                codex_response_item_prefix: None,
                codex_response_handoff_mode: Some(CodexResponseHandoffMode::BemTags),
                codex_response_handoff_channel_prefixes: Some(std::collections::BTreeMap::from([
                    ("analysis".to_string(), vec!["[THINKING]".to_string()]),
                    (
                        "commentary".to_string(),
                        vec!["[PROGRESS]".to_string(), "[UPDATE]".to_string()],
                    ),
                    ("final".to_string(), vec!["[DONE]".to_string()]),
                ])),
                thread_id: "thr_123".to_string(),
                model: Some("realtime-treatment-model".to_string()),
                output_modality: RealtimeOutputModality::Audio,
                include_startup_context: Some(false),
                initial_items: Some(vec![
                    v2::ThreadRealtimeInitialItem {
                        role: ConversationTextRole::Developer,
                        text: "Remember this.".to_string(),
                    },
                    v2::ThreadRealtimeInitialItem {
                        role: ConversationTextRole::Assistant,
                        text: "Understood.".to_string(),
                    },
                ]),
                realtime_start_instructions: Some("Use realtime output channels.".to_string()),
                realtime_end_instructions: Some("Resume normal text responses.".to_string()),
                prompt: Some(Some("You are on a call".to_string())),
                realtime_session_id: Some("sess_456".to_string()),
                transport: None,
                version: Some(RealtimeConversationVersion::V3),
                voice: Some(RealtimeVoice::Marin),
            },
        };
        assert_eq!(
            json!({
                "method": "thread/realtime/start",
                "id": 9,
                "params": {
                    "threadId": "thr_123",
                    "clientManagedHandoffs": true,
                    "delegationAckFiller": false,
                    "flushTranscriptTailOnSessionEnd": true,
                    "codexResponsesAsItems": null,
                    "codexResponseItemPrefix": null,
                    "codexResponseHandoffMode": "bemTags",
                    "codexResponseHandoffChannelPrefixes": {
                        "analysis": ["[THINKING]"],
                        "commentary": ["[PROGRESS]", "[UPDATE]"],
                        "final": ["[DONE]"]
                    },
                    "model": "realtime-treatment-model",
                    "outputModality": "audio",
                    "includeStartupContext": false,
                    "initialItems": [
                        {
                            "role": "developer",
                            "text": "Remember this."
                        },
                        {
                            "role": "assistant",
                            "text": "Understood."
                        }
                    ],
                    "realtimeStartInstructions": "Use realtime output channels.",
                    "realtimeEndInstructions": "Resume normal text responses.",
                    "prompt": "You are on a call",
                    "realtimeSessionId": "sess_456",
                    "transport": null,
                    "version": "v3",
                    "voice": "marin"
                }
            }),
            serde_json::to_value(&request)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_thread_realtime_start_prompt_default_and_null() -> Result<()> {
        let default_prompt_request = ClientRequest::ThreadRealtimeStart {
            request_id: RequestId::Integer(9),
            params: v2::ThreadRealtimeStartParams {
                client_managed_handoffs: None,
                delegation_ack_filler: None,
                flush_transcript_tail_on_session_end: None,
                codex_responses_as_items: None,
                codex_response_item_prefix: None,
                codex_response_handoff_mode: None,
                codex_response_handoff_channel_prefixes: None,
                thread_id: "thr_123".to_string(),
                model: None,
                output_modality: RealtimeOutputModality::Audio,
                include_startup_context: None,
                initial_items: None,
                realtime_start_instructions: None,
                realtime_end_instructions: None,
                prompt: None,
                realtime_session_id: None,
                transport: None,
                version: None,
                voice: None,
            },
        };
        assert_eq!(
            json!({
                "method": "thread/realtime/start",
                "id": 9,
                "params": {
                    "threadId": "thr_123",
                    "clientManagedHandoffs": null,
                    "delegationAckFiller": null,
                    "flushTranscriptTailOnSessionEnd": null,
                    "codexResponsesAsItems": null,
                    "codexResponseItemPrefix": null,
                    "codexResponseHandoffMode": null,
                    "codexResponseHandoffChannelPrefixes": null,
                    "model": null,
                    "outputModality": "audio",
                    "includeStartupContext": null,
                    "initialItems": null,
                    "realtimeStartInstructions": null,
                    "realtimeEndInstructions": null,
                    "realtimeSessionId": null,
                    "transport": null,
                    "version": null,
                    "voice": null
                }
            }),
            serde_json::to_value(&default_prompt_request)?,
        );

        let null_prompt_request = ClientRequest::ThreadRealtimeStart {
            request_id: RequestId::Integer(9),
            params: v2::ThreadRealtimeStartParams {
                client_managed_handoffs: None,
                delegation_ack_filler: None,
                flush_transcript_tail_on_session_end: None,
                codex_responses_as_items: None,
                codex_response_item_prefix: None,
                codex_response_handoff_mode: None,
                codex_response_handoff_channel_prefixes: None,
                thread_id: "thr_123".to_string(),
                model: None,
                output_modality: RealtimeOutputModality::Audio,
                include_startup_context: None,
                initial_items: None,
                realtime_start_instructions: None,
                realtime_end_instructions: None,
                prompt: Some(None),
                realtime_session_id: None,
                transport: None,
                version: None,
                voice: None,
            },
        };
        assert_eq!(
            json!({
                "method": "thread/realtime/start",
                "id": 9,
                "params": {
                    "threadId": "thr_123",
                    "clientManagedHandoffs": null,
                    "delegationAckFiller": null,
                    "flushTranscriptTailOnSessionEnd": null,
                    "codexResponsesAsItems": null,
                    "codexResponseItemPrefix": null,
                    "codexResponseHandoffMode": null,
                    "codexResponseHandoffChannelPrefixes": null,
                    "model": null,
                    "outputModality": "audio",
                    "includeStartupContext": null,
                    "initialItems": null,
                    "realtimeStartInstructions": null,
                    "realtimeEndInstructions": null,
                    "prompt": null,
                    "realtimeSessionId": null,
                    "transport": null,
                    "version": null,
                    "voice": null
                }
            }),
            serde_json::to_value(&null_prompt_request)?,
        );

        let default_prompt_value = json!({
            "method": "thread/realtime/start",
            "id": 9,
            "params": {
                "threadId": "thr_123",
                // Retain runtime compatibility with clients that have not yet removed this field.
                "codexResponseHandoffPrefix": "",
                "outputModality": "audio",
                "realtimeSessionId": null,
                "transport": null,
                "voice": null
            }
        });
        assert_eq!(
            serde_json::from_value::<ClientRequest>(default_prompt_value)?,
            default_prompt_request,
        );

        let null_prompt_value = json!({
            "method": "thread/realtime/start",
            "id": 9,
            "params": {
                "threadId": "thr_123",
                "outputModality": "audio",
                "prompt": null,
                "realtimeSessionId": null,
                "transport": null,
                "voice": null
            }
        });
        assert_eq!(
            serde_json::from_value::<ClientRequest>(null_prompt_value)?,
            null_prompt_request,
        );

        Ok(())
    }

    #[test]
    fn serialize_thread_realtime_append_speech() -> Result<()> {
        let request = ClientRequest::ThreadRealtimeAppendSpeech {
            request_id: RequestId::Integer(10),
            params: v2::ThreadRealtimeAppendSpeechParams {
                thread_id: "thr_123".to_string(),
                text: "Short voice update".to_string(),
            },
        };
        assert_eq!(
            json!({
                "method": "thread/realtime/appendSpeech",
                "id": 10,
                "params": {
                    "threadId": "thr_123",
                    "text": "Short voice update"
                }
            }),
            serde_json::to_value(&request)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_thread_status_changed_notification() -> Result<()> {
        let notification =
            ServerNotification::ThreadStatusChanged(v2::ThreadStatusChangedNotification {
                thread_id: "thr_123".to_string(),
                status: v2::ThreadStatus::Idle,
            });
        assert_eq!(
            json!({
                "method": "thread/status/changed",
                "params": {
                    "threadId": "thr_123",
                    "status": {
                        "type": "idle"
                    },
                }
            }),
            serde_json::to_value(&notification)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_model_safety_buffering_updated_notification() -> Result<()> {
        let notification = ServerNotification::ModelSafetyBufferingUpdated(
            v2::ModelSafetyBufferingUpdatedNotification {
                thread_id: "thr_123".to_string(),
                turn_id: "turn_123".to_string(),
                model: "current-model".to_string(),
                use_cases: vec!["cyber".to_string()],
                reasons: vec!["user_risk".to_string()],
                show_buffering_ui: true,
                faster_model: Some("faster-model".to_string()),
            },
        );
        assert_eq!(
            json!({
                "method": "model/safetyBuffering/updated",
                "params": {
                    "threadId": "thr_123",
                    "turnId": "turn_123",
                    "model": "current-model",
                    "useCases": ["cyber"],
                    "reasons": ["user_risk"],
                    "showBufferingUi": true,
                    "fasterModel": "faster-model"
                }
            }),
            serde_json::to_value(&notification)?,
        );
        Ok(())
    }

    #[test]
    fn serialize_thread_realtime_output_audio_delta_notification() -> Result<()> {
        let notification = ServerNotification::ThreadRealtimeOutputAudioDelta(
            v2::ThreadRealtimeOutputAudioDeltaNotification {
                thread_id: "thr_123".to_string(),
                audio: v2::ThreadRealtimeAudioChunk {
                    data: "AQID".to_string(),
                    sample_rate: 24_000,
                    num_channels: 1,
                    samples_per_channel: Some(512),
                    item_id: None,
                },
            },
        );
        assert_eq!(
            json!({
                "method": "thread/realtime/outputAudio/delta",
                "params": {
                    "threadId": "thr_123",
                    "audio": {
                        "data": "AQID",
                        "sampleRate": 24000,
                        "numChannels": 1,
                        "samplesPerChannel": 512,
                        "itemId": null
                    }
                }
            }),
            serde_json::to_value(&notification)?,
        );
        Ok(())
    }

    #[test]
    fn mock_experimental_method_is_marked_experimental() {
        let request = ClientRequest::MockExperimentalMethod {
            request_id: RequestId::Integer(1),
            params: v2::MockExperimentalMethodParams::default(),
        };
        let reason = crate::experimental_api::ExperimentalApi::experimental_reason(&request);
        assert_eq!(reason, Some("mock/experimentalMethod"));
    }

    #[test]
    fn environment_add_is_marked_experimental() {
        let request = ClientRequest::EnvironmentAdd {
            request_id: RequestId::Integer(1),
            params: v2::EnvironmentAddParams {
                environment_id: "remote-a".to_string(),
                exec_server_url: "ws://127.0.0.1:8765".to_string(),
                connect_timeout_ms: None,
            },
        };
        let reason = crate::experimental_api::ExperimentalApi::experimental_reason(&request);
        assert_eq!(reason, Some("environment/add"));
    }

    #[test]
    fn command_exec_permission_profile_is_marked_experimental() {
        let request = ClientRequest::OneOffCommandExec {
            request_id: RequestId::Integer(1),
            params: v2::CommandExecParams {
                command: vec!["pwd".to_string()],
                process_id: None,
                tty: false,
                stream_stdin: false,
                stream_stdout_stderr: false,
                output_bytes_cap: None,
                disable_output_cap: false,
                disable_timeout: false,
                timeout_ms: None,
                cwd: None,
                env: None,
                size: None,
                sandbox_policy: None,
                permission_profile: Some(BUILT_IN_PERMISSION_PROFILE_READ_ONLY.to_string()),
            },
        };

        let reason = crate::experimental_api::ExperimentalApi::experimental_reason(&request);
        assert_eq!(reason, Some("command/exec.permissionProfile"));
    }

    #[test]
    fn thread_realtime_start_is_marked_experimental() {
        let request = ClientRequest::ThreadRealtimeStart {
            request_id: RequestId::Integer(1),
            params: v2::ThreadRealtimeStartParams {
                client_managed_handoffs: None,
                delegation_ack_filler: None,
                flush_transcript_tail_on_session_end: None,
                codex_responses_as_items: None,
                codex_response_item_prefix: None,
                codex_response_handoff_mode: None,
                codex_response_handoff_channel_prefixes: None,
                thread_id: "thr_123".to_string(),
                model: None,
                output_modality: RealtimeOutputModality::Audio,
                include_startup_context: None,
                initial_items: None,
                realtime_start_instructions: None,
                realtime_end_instructions: None,
                prompt: Some(Some("You are on a call".to_string())),
                realtime_session_id: None,
                transport: None,
                version: None,
                voice: None,
            },
        };
        let reason = crate::experimental_api::ExperimentalApi::experimental_reason(&request);
        assert_eq!(reason, Some("thread/realtime/start"));
    }

    #[test]
    fn thread_goal_methods_are_not_marked_experimental() {
        let set_request = ClientRequest::ThreadGoalSet {
            request_id: RequestId::Integer(1),
            params: v2::ThreadGoalSetParams {
                thread_id: "thr_123".to_string(),
                objective: Some("ship goal mode".to_string()),
                status: Some(v2::ThreadGoalStatus::Active),
                token_budget: Some(Some(10_000)),
            },
        };
        let get_request = ClientRequest::ThreadGoalGet {
            request_id: RequestId::Integer(2),
            params: v2::ThreadGoalGetParams {
                thread_id: "thr_123".to_string(),
            },
        };
        let clear_request = ClientRequest::ThreadGoalClear {
            request_id: RequestId::Integer(3),
            params: v2::ThreadGoalClearParams {
                thread_id: "thr_123".to_string(),
            },
        };

        assert_eq!(
            crate::experimental_api::ExperimentalApi::experimental_reason(&set_request),
            None
        );
        assert_eq!(
            crate::experimental_api::ExperimentalApi::experimental_reason(&get_request),
            None
        );
        assert_eq!(
            crate::experimental_api::ExperimentalApi::experimental_reason(&clear_request),
            None
        );
    }

    #[test]
    fn thread_goal_notifications_are_not_marked_experimental() {
        let goal = v2::ThreadGoal {
            thread_id: "thr_123".to_string(),
            objective: "ship goal mode".to_string(),
            status: v2::ThreadGoalStatus::Active,
            token_budget: Some(10_000),
            tokens_used: 123,
            time_used_seconds: 45,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_123,
        };
        let updated = ServerNotification::ThreadGoalUpdated(v2::ThreadGoalUpdatedNotification {
            thread_id: "thr_123".to_string(),
            turn_id: None,
            goal,
        });
        let cleared = ServerNotification::ThreadGoalCleared(v2::ThreadGoalClearedNotification {
            thread_id: "thr_123".to_string(),
        });

        assert_eq!(
            crate::experimental_api::ExperimentalApi::experimental_reason(&updated),
            None
        );
        assert_eq!(
            crate::experimental_api::ExperimentalApi::experimental_reason(&cleared),
            None
        );
    }

    #[test]
    fn thread_settings_updated_notification_is_marked_experimental() {
        let notification =
            ServerNotification::ThreadSettingsUpdated(v2::ThreadSettingsUpdatedNotification {
                thread_id: "thr_123".to_string(),
                thread_settings: v2::ThreadSettings {
                    cwd: absolute_path("/tmp/repo"),
                    approval_policy: v2::AskForApproval::Never,
                    approvals_reviewer: v2::ApprovalsReviewer::User,
                    sandbox_policy: v2::SandboxPolicy::DangerFullAccess,
                    active_permission_profile: None,
                    model: "gpt-5.4".to_string(),
                    model_provider: "openai".to_string(),
                    service_tier: None,
                    effort: None,
                    summary: None,
                    agent_settings: codex_protocol::config_types::AgentSettings {
                        settings: codex_protocol::config_types::Settings {
                            model: "gpt-5.4".to_string(),
                            reasoning_effort: None,
                            developer_instructions: None,
                        },
                    },
                    multi_agent_mode: Default::default(),
                    personality: None,
                },
            });

        assert_eq!(
            crate::experimental_api::ExperimentalApi::experimental_reason(&notification),
            Some("thread/settings/updated")
        );
    }

    #[test]
    fn turn_moderation_metadata_notification_is_marked_experimental() {
        let notification =
            ServerNotification::TurnModerationMetadata(v2::TurnModerationMetadataNotification {
                thread_id: "thr_123".to_string(),
                turn_id: "turn_123".to_string(),
                metadata: json!({"presentation": "inline"}),
            });

        assert_eq!(
            crate::experimental_api::ExperimentalApi::experimental_reason(&notification),
            Some("turn/moderationMetadata")
        );
    }

    #[test]
    fn thread_realtime_started_notification_is_marked_experimental() {
        let notification =
            ServerNotification::ThreadRealtimeStarted(v2::ThreadRealtimeStartedNotification {
                thread_id: "thr_123".to_string(),
                realtime_session_id: Some("sess_456".to_string()),
                version: RealtimeConversationVersion::V1,
            });
        let reason = crate::experimental_api::ExperimentalApi::experimental_reason(&notification);
        assert_eq!(reason, Some("thread/realtime/started"));
    }

    #[test]
    fn thread_realtime_output_audio_delta_notification_is_marked_experimental() {
        let notification = ServerNotification::ThreadRealtimeOutputAudioDelta(
            v2::ThreadRealtimeOutputAudioDeltaNotification {
                thread_id: "thr_123".to_string(),
                audio: v2::ThreadRealtimeAudioChunk {
                    data: "AQID".to_string(),
                    sample_rate: 24_000,
                    num_channels: 1,
                    samples_per_channel: Some(512),
                    item_id: None,
                },
            },
        );
        let reason = crate::experimental_api::ExperimentalApi::experimental_reason(&notification);
        assert_eq!(reason, Some("thread/realtime/outputAudio/delta"));
    }

    #[test]
    fn command_execution_request_approval_additional_permissions_is_marked_experimental() {
        let params = v2::CommandExecutionRequestApprovalParams {
            thread_id: "thr_123".to_string(),
            turn_id: "turn_123".to_string(),
            item_id: "call_123".to_string(),
            started_at_ms: 0,
            approval_id: None,
            environment_id: None,
            reason: None,
            network_approval_context: None,
            command: Some("cat file".to_string()),
            cwd: None,
            command_actions: None,
            additional_permissions: Some(v2::AdditionalPermissionProfile {
                network: None,
                file_system: Some(v2::AdditionalFileSystemPermissions {
                    read: Some(vec![absolute_path("/tmp/allowed").into()]),
                    write: None,
                    glob_scan_max_depth: None,
                    entries: None,
                }),
            }),
            proposed_execpolicy_amendment: None,
            proposed_network_policy_amendments: None,
            available_decisions: None,
        };
        let reason = crate::experimental_api::ExperimentalApi::experimental_reason(&params);
        assert_eq!(
            reason,
            Some("item/commandExecution/requestApproval.additionalPermissions")
        );
    }
}

#[cfg(test)]
#[path = "common_tests.rs"]
mod common_tests;
