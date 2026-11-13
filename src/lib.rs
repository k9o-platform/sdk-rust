//! # k9o — Kilimanjaro platform SDK for Rust
//!
//! Idiomatic Rust SDK: `snake_case` methods +
//! `Result<Response<T>, KError>` error delivery + `Client::new(ClientOpts{...})`
//! constructor + reqwest async transport.
//!
//! ## Canonical usage
//!
//! ```ignore
//! use k9o::{Client, ClientOpts, KError};
//! use serde::Deserialize;
//!
//! #[derive(Debug, Deserialize)]
//! struct Post { id: String, title: String }
//!
//! #[tokio::main]
//! async fn main() -> Result<(), KError> {
//!     let k9o = Client::new(ClientOpts {
//!         base_url: "https://acme.api.k9o.io".into(),
//!         api_key: std::env::var("K9O_API_KEY").ok(),
//!         org_slug: Some("acme".into()),
//!         project_slug: Some("member-portal".into()),
//!         ..Default::default()
//!     })?;
//!
//!     match k9o.from("posts").select().limit(10).execute::<Post>().await {
//!         Ok(resp) => {
//!             for row in resp.data { println!("{:?}", row); }
//!             if let Some(cursor) = resp.meta.cursor {
//!                 println!("next cursor: {:?}", cursor.next);
//!             }
//!         }
//!         Err(e) => {
//!             if e.code == "K9O_TIER_CAP_HIT" {
//!                 eprintln!("[{}] {}: {:?}", e.code, e.message, e.recommendation);
//!             }
//!             return Err(e);
//!         }
//!     }
//!
//!     Ok(())
//! }
//! ```

pub mod ai;
pub mod approvals;
pub mod audit;
pub mod client;
pub mod client_core;
pub mod compute;
pub mod crypto;
pub mod custody;
pub mod data;
pub mod datasets;
pub mod end_users;
pub mod envelope;
pub mod functions;
pub mod gate;
pub mod jcs;
pub mod kerror;
pub mod keys;
pub mod kibo;
pub mod me;
pub mod metadata;
pub mod migrations;
pub mod moat;
pub mod org;
pub mod orgs;
pub mod policies;
pub mod projects;
pub mod query;
pub mod rampart;
pub mod realtime;
pub mod schedules;
pub mod schema;
pub mod search;
pub mod storage;
pub mod streams;
pub mod studio;
pub mod url_grammar;
pub mod vectors;

mod ed25519;
mod hash;
mod http;
mod minire;
#[cfg(test)]
mod rampart_tests;
mod region;
mod stream;

// Public re-exports
pub use client::{Client, ClientOpts};
pub use data::{
    ApprovalWrite, DeleteBuilder, FromBuilder, InsertBuilder, SelectBuilder, TextSearchOptions,
    TextSearchType, UpdateBuilder, VectorMetric, VectorSearchOptions, WriteOutcome,
};
pub use envelope::{
    Envelope, Meta, MetaApproval, MetaContext, MetaCursor, MetaIdempotency, MetaRawQuery,
    MetaSubscription, MetaTiming, MetaWarning, Response,
};
pub use gate::{GateNamespace, RefreshTokenInput, SignInInput, SignOutInput, SignUpInput};
pub use http::{Backoff, RetryConfig};
pub use kerror::KError;
pub use projects::{
    CreateProjectInput, Project, ProjectsNamespace, ShowroomLocale, UpdateProjectInput,
};
pub use query::{translate_named_params, Params};
pub use search::SearchInput;
pub use url_grammar::{
    org_path, parse_api_host, platform_path, project_path, resolve_base_url, validate_base_url,
    validate_slug, ApiHost, DEFAULT_BASE_URL,
};

pub use ai::{AiNamespace, EmbedInput, EmbedResult, EmbedText, GenerateOptions};
pub use approvals::{
    AddApprovalCommentInput, ApprovalComment, ApprovalCommentPage, ApprovalCommentsNamespace,
    ApprovalPoliciesNamespace, ApprovalPolicy, ApprovalPolicyDeleted, ApprovalsNamespace,
    ApproveInput, BulkApprovalFailure, BulkApprovalsInput, BulkApprovalsResult,
    CreateApprovalPolicyInput, ListApprovalCommentsOptions, ListApprovalsOptions, PendingChange,
    QueuedChange, RejectInput,
};
pub use compute::{
    ComputeNamespace, LlmInvokeInput, LlmInvokeParams, LlmModel, LlmResponse, LlmStreamChunk,
    LlmUsage, LlmsNamespace, LLM_MODEL_IDS,
};
pub use custody::{CustodyJob, CustodyNamespace, CustodyStatus, HsmAttestation, HsmInfo};
pub use jcs::{canonicalize as jcs_canonicalize, JcsError};
pub use metadata::{
    MetadataChange, MetadataFilterInput, MetadataIndex, MetadataIndexColumn, MetadataIndexEntry,
    MetadataIndexNamespace, MetadataNamespace, MetadataNearestInput, MetadataOp, MetadataRow,
    MetadataSearchInput, MetadataSubscribeOptions, MetadataVectorsNamespace, MetadataWhere,
};
pub use moat::{
    Message as MoatMessage, Moat, MoatConfigError, MoatOptions, MoatReason, MoatRefusal, PiiFilter,
};
pub use org::{
    CreateIdpInput, GrantRoleInput, OrgAuditQueryOpts, OrgDefaultLlm, OrgDefaultLlmSet,
    OrgNamespace, SetSmtpInput, UpdateIdpInput,
};
pub use rampart::{
    Classification, Jwk, JwkSet, RampartNamespace, ReceiptRefusal, ReceiptRefusalReason,
    ReceiptSource, VerifiedReceipt, VerifyOptions, RECEIPT_TYP,
};
pub use realtime::{
    topic as realtime_topic, RealtimeEvent, RealtimeNamespace, RealtimeReplay, ReplayOptions,
    Resync, ResyncFuture, SubscribeOptions, Subscription,
};
pub use schema::{
    CollectionSummary, ListCollectionsOptions, SchemaCollectionsNamespace, SchemaNamespace,
};
pub use storage::{
    BucketHandle, CreateBucketOptions, DeletedBucket, RemovedObject, SignedUrl, SignedUrlOptions,
    StorageBucket, StorageNamespace, StorageObject, UpdateBucketOptions, UploadOptions,
};
pub use stream::SseStream;
pub use streams::{
    PublishResult, ReplayResult, Stream, StreamDeleted, StreamEvent, StreamSubscription,
    StreamsNamespace,
};
pub use studio::{
    Dashboard, DashboardsNamespace, StudioNamespace, Workflow, WorkflowRun, WorkflowRunsNamespace,
    WorkflowsNamespace,
};

pub use audit::{
    AuditEvent, AuditNamespace, AuditQueryOpts, AuditReplay, AuditReplayEvent, AuditReplayOptions,
    AuditTail, AuditTailEvent, AuditTailOptions,
};
pub use client_core::{
    AppCore, AppCredential, AppKeyRef, AppOpenInput, AppSealInput, FederatedTokenFn,
    FederatedTokenRequest, FederationInput, FederationSpec, KiboClientCore, KiboClientCoreConfig,
    KmsProvider, Posture, ProviderConfig, TokenFuture, TokenOp, CODE_EXCHANGE_TIMEOUT,
    CODE_WASM_ERROR,
};
pub use crypto::{
    key_ref_matches, ClientCore, CoreOptions, Crypt, CryptList, CryptStatus, CryptsNamespace, CryptoBatch, CryptoFederatedToken, CryptoItemResult,
    CryptoMode, CryptoNamespace, KeyRefProvider, PlainItem, SealedItem, CODE_CUSTODY_MODE_UNKNOWN,
    CODE_KEY_PIN_UNENFORCED, CODE_ZK_CLIENT_CORE_UNAVAILABLE, CODE_ZK_STORE_FLOW_REQUIRED,
};
pub use datasets::{
    AnalyticsNamespace, CreateDatasetInput, CreateDatasetScheduleInput, CreateRollupInput, Dataset,
    DatasetColumn, DatasetRetired, DatasetRollupsNamespace, DatasetSchedule,
    DatasetSchedulesNamespace, DeletedRef, IngestResult, Listed, MaterializeResult, OnError,
    OriginalFile, ReadOptions, ResidencyClass, Rollup, RollupCell, RollupMeasure,
    RollupReadOptions, RollupSubscribeOptions, RollupSubscription, RollupsNamespace, StorageTarget,
    UploadFormat, UploadOptions as DatasetUploadOptions, WriteOptions as DatasetWriteOptions,
};
pub use end_users::{
    EndUser, EndUserRoleGrant, EndUserRoleGrantsNamespace, EndUsersNamespace,
    GrantEndUserRoleInput, ListEndUsersOptions, ListRoleGrantsOptions, RevokeRoleGrantResult,
};
pub use functions::{
    ArtifactUpload, CreateTriggerInput, DeployFunctionInput, FunctionRecord, FunctionTrigger,
    FunctionsNamespace, InvokeDelegation, InvokeOptions, ListFunctionsOptions, TriggersNamespace,
    UpdateTriggerInput,
};
pub use keys::{
    ByokRef, HyokRef, KeyList, KeyRotation, KeyStatusFilter, KeysNamespace, ProjectKey,
    RotateOptions, StepUpChallenge, StepUpFuture, StepUpProof, StepUpRequest,
};
pub use me::{ApiTokenScopeManifest, CreateApiTokenInput, MeNamespace};
pub use migrations::{
    ActionRule, AddColumnOp, AlterTableOp, AppliedMigration, CollectionActions, CollectionPolicy,
    ColumnEncryption, ColumnReferences, ColumnSpec, CreateIndexOp, CreateTableOp, DropColumnOp,
    DropIndexOp, IndexSpec, Migration, MigrationPolicy, MigrationStorage, MigrationsNamespace,
    Operation, PendingMigration, RelationshipSpec, StorageBucketActions, StorageBucketPolicy,
};
pub use orgs::{Org, OrgsNamespace};
pub use policies::{
    CreatePolicyInput, PoliciesNamespace, PolicyBundle, PolicyDiagnostic, UpdatePolicyInput,
    ValidatePolicyInput, ValidatePolicyResult,
};
pub use schedules::{
    CreateScheduleInput, ListScheduleRunsOptions, ListSchedulesOptions, Schedule, ScheduleRun,
    SchedulesNamespace, UpdateScheduleInput,
};

pub use vectors::{
    NearestOptions, NearestResult, NearestVector, UpsertResult, VectorRow, VectorSource,
    VectorsNamespace,
};

// Kibo vault-secrets namespace (ported from the `packages/kibo` TS reference).
pub use kibo::{
    AttestationTier, CreateSecretInput, KiboNamespace, ListSecretsInput, ListSecretsResult,
    MoveSecretResult, RevealChallenge, RevealSecretResult, RotateSecretResult, SecretSummary,
    SecretVisibility, SecretsNamespace, UpdateSecretInput, UpdateSecretResult, VaultNamespace,
};
