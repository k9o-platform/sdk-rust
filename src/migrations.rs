//! Migration lifecycle namespace — `client.migrations()`.
//!
//! Structured declarative-DDL model).
//! The migration `Operation` union is CONTROL-PLANE,
//! so it is TYPED per tier; Rust (tier-1 static) models it as a real enum whose
//! key-tagged JSON is produced by Serde's external tagging. The DYNAMIC
//! data-plane record bodies remain the language open-map type (`serde_json`),
//! and are NOT modelled here — those live on the `from(...)` surface in
//! `data.rs`.
//!
//! Wire shape is grounded in the canonical `packages/core/src/migrations.ts`
//! and the landed static-tier reference `packages/go/migrations.go`. Op set is
//! at 7 (no `alter_column`/`rename_table`/constraint at v2); `alter_table` is
//! live in the gateway runner (`migration-runner.ts:156`)
//! as the BATCHED column mutation. `rename_column` remains the pending 8th and is
//! intentionally NOT present (its parser field names are not yet finalised).

use crate::envelope::Response;
use crate::http::{request, HttpClientConfig};
use crate::kerror::KError;
use crate::stream::require_project;
use crate::url_grammar::{enc, project_path};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Foreign-key reference on a column — compiles to a REFERENCES clause.
/// `on_delete`/`on_update` accept "CASCADE" | "RESTRICT" | "SET NULL" |
/// "NO ACTION".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnReferences {
    pub collection: String,
    pub on: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_delete: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_update: Option<String>,
}

/// At-rest field encryption. The wire form is the union
/// `true | { store, searchable? }`: the bare `true` is the inline-stored
/// encrypted-column shorthand; the object form opts into a `store`
/// ("inline" | "vault") and the D-mode searchable-encryption index. NOTE:
/// `searchable` is NESTED inside the object form — it is NOT a sibling of the
/// column. Modelled as an untagged enum so `true` and the
/// object both round-trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ColumnEncryption {
    /// Inline-stored encrypted column shorthand — serializes to bare `true`.
    Enabled(bool),
    /// Object form: explicit `store` + optional `searchable`.
    Options {
        store: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        searchable: Option<bool>,
    },
}

/// A column definition. `type` is the column-type vocabulary
/// (parser-owned; kept as a `String` rather than a fabricated enum).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnSpec {
    pub name: String,
    #[serde(rename = "type")]
    pub r#type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_key: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_null: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unique: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub references: Option<ColumnReferences>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encrypted: Option<ColumnEncryption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blind_indexed: Option<bool>,
}

/// An index definition. `name` defaults to `<table>_<cols>_idx`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub columns: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unique: Option<bool>,
}

/// Payload of `Operation::CreateTable`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTableOp {
    pub name: String,
    pub columns: Vec<ColumnSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexes: Option<Vec<IndexSpec>>,
}

/// Payload of `Operation::AddColumn`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddColumnOp {
    pub table: String,
    pub column: ColumnSpec,
}

/// Payload of `Operation::DropColumn`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DropColumnOp {
    pub table: String,
    pub column: String,
}

/// Payload of `Operation::AlterTable` — the BATCHED column mutation: one op
/// carries optional `add_columns` + `drop_columns` (not separate add/drop ops).
/// `add_columns` entries are full `ColumnSpec` (incl encrypted/blind_indexed);
/// `drop_columns` is column names. Live in the gateway runner (migration-runner.ts:156).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlterTableOp {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub add_columns: Option<Vec<ColumnSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drop_columns: Option<Vec<String>>,
}

/// Payload of `Operation::CreateIndex`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateIndexOp {
    pub table: String,
    pub index: IndexSpec,
}

/// Payload of `Operation::DropIndex`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DropIndexOp {
    pub name: String,
}

/// A single declarative DDL operation — KEY-TAGGED (exactly one key set), the
/// shape the gateway applies. Serde's external tagging produces the key-tagged
/// wire JSON (`{"create_table": {...}}`, `{"drop_table": "posts"}`, ...).
/// The v2 set is these 7 — no `alter_column`, `rename_table`, or constraint ops
/// at v2. `alter_table` is the BATCHED column mutation (live in the gateway
/// runner). `rename_column` remains a pending future op
/// (its parser field names are not yet finalised) and is added in one variant bump
/// when it lands.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    CreateTable(CreateTableOp),
    DropTable(String),
    AddColumn(AddColumnOp),
    DropColumn(DropColumnOp),
    AlterTable(AlterTableOp),
    CreateIndex(CreateIndexOp),
    DropIndex(DropIndexOp),
}

/// A declarative authorization/relationship rule (compiles to RLS USING +
/// WITH CHECK). `rule` is left open to mirror the spec's shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionRule {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<Value>,
}

/// A declared relationship for embedding/joins (references).
/// `type` accepts "inner" | "left".
#[derive(Debug, Clone, Deserialize)]
pub struct RelationshipSpec {
    pub collection: String,
    pub on: String,
    #[serde(rename = "as")]
    pub as_: String,
    #[serde(rename = "type")]
    pub r#type: Option<String>,
}

// `as` is a Rust keyword; the field is `as_` but must serialize as `as`.
// Hand-written Serialize so external readers see the wire key `as`.
impl Serialize for RelationshipSpec {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut n = 3;
        if self.r#type.is_some() {
            n += 1;
        }
        let mut st = serializer.serialize_struct("RelationshipSpec", n)?;
        st.serialize_field("collection", &self.collection)?;
        st.serialize_field("on", &self.on)?;
        st.serialize_field("as", &self.as_)?;
        if let Some(t) = &self.r#type {
            st.serialize_field("type", t)?;
        }
        st.end()
    }
}

/// Per-action RLS rules on a collection.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CollectionActions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub select: Option<ActionRule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub insert: Option<ActionRule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update: Option<ActionRule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete: Option<ActionRule>,
}

/// Per-collection policy block (RLS + relationships).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CollectionPolicy {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub references: Option<Vec<RelationshipSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actions: Option<CollectionActions>,
}

/// Per-action rules on a storage bucket.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StorageBucketActions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read: Option<ActionRule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload: Option<ActionRule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete: Option<ActionRule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list: Option<ActionRule>,
}

/// Per-bucket storage policy block.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StorageBucketPolicy {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actions: Option<StorageBucketActions>,
}

/// The storage section of a `MigrationPolicy`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MigrationStorage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buckets: Option<std::collections::HashMap<String, StorageBucketPolicy>>,
}

/// The optional `policy` section authored alongside a migration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MigrationPolicy {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collections: Option<std::collections::HashMap<String, CollectionPolicy>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage: Option<MigrationStorage>,
}

/// A migration — a versioned, checksummed unit of structured DDL.
/// `up` applies the change; `down` reverses it. Both are operation arrays.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Migration {
    pub version: i64,
    pub slug: String,
    pub up: Vec<Operation>,
    pub down: Vec<Operation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<MigrationPolicy>,
}

/// Applied-migration record returned by `list` + `get`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppliedMigration {
    pub version: i64,
    pub slug: String,
    pub checksum: String,
    pub applied_at: String,
}

/// Pending-migration record returned by `list_pending`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingMigration {
    pub pending_id: String,
    pub version: i64,
    pub slug: String,
    pub checksum: String,
    pub submitted_at: String,
    pub submitted_by: String,
}

#[derive(Serialize)]
struct MigrationsBody<'a> {
    migrations: &'a [Migration],
}

#[derive(Serialize)]
struct RejectBody<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
}

/// Migrations namespace — `client.migrations()`
pub struct MigrationsNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> MigrationsNamespace<'a> {
    /// Apply migrations immediately (PROJECT_ADMIN minimum).
    pub async fn apply(&self, migrations: &[Migration]) -> Result<Response<Value>, KError> {
        let slug = require_project(self.config, "migrations")?;
        request::<Value, _>(
            self.config,
            Method::POST,
            &project_path(slug, "migrations/apply"),
            Some(&MigrationsBody { migrations }),
            None,
        )
        .await
    }

    /// List applied migrations on the project.
    pub async fn list(&self) -> Result<Response<Vec<AppliedMigration>>, KError> {
        let slug = require_project(self.config, "migrations")?;
        request::<Vec<AppliedMigration>, ()>(
            self.config,
            Method::GET,
            &project_path(slug, "migrations"),
            None,
            None,
        )
        .await
    }

    /// Get a single applied migration by version.
    pub async fn get(&self, version: &str) -> Result<Response<AppliedMigration>, KError> {
        let slug = require_project(self.config, "migrations")?;
        request::<AppliedMigration, ()>(
            self.config,
            Method::GET,
            &project_path(slug, &format!("migrations/{}", enc(version))),
            None,
            None,
        )
        .await
    }

    /// Queue migrations for maker-checker approval
    pub async fn queue(&self, migrations: &[Migration]) -> Result<Response<Value>, KError> {
        let slug = require_project(self.config, "migrations")?;
        request::<Value, _>(
            self.config,
            Method::POST,
            &project_path(slug, "migrations/queue"),
            Some(&MigrationsBody { migrations }),
            None,
        )
        .await
    }

    /// List pending (queued, not yet approved) migrations.
    pub async fn list_pending(&self) -> Result<Response<Vec<PendingMigration>>, KError> {
        let slug = require_project(self.config, "migrations")?;
        request::<Vec<PendingMigration>, ()>(
            self.config,
            Method::GET,
            &project_path(slug, "migrations/pending"),
            None,
            None,
        )
        .await
    }

    /// Approve + apply a pending migration.
    pub async fn apply_pending(
        &self,
        pending_id: &str,
    ) -> Result<Response<AppliedMigration>, KError> {
        let slug = require_project(self.config, "migrations")?;
        request::<AppliedMigration, ()>(
            self.config,
            Method::POST,
            &project_path(
                slug,
                &format!("migrations/pending/{}/apply", enc(pending_id)),
            ),
            None,
            None,
        )
        .await
    }

    /// Reject a pending migration (does not apply).
    pub async fn reject_pending(
        &self,
        pending_id: &str,
        reason: Option<&str>,
    ) -> Result<Response<Value>, KError> {
        let slug = require_project(self.config, "migrations")?;
        request::<Value, _>(
            self.config,
            Method::POST,
            &project_path(
                slug,
                &format!("migrations/pending/{}/reject", enc(pending_id)),
            ),
            Some(&RejectBody { reason }),
            None,
        )
        .await
    }

    /// Delete a pending migration from the queue.
    pub async fn delete_pending(&self, pending_id: &str) -> Result<Response<Value>, KError> {
        let slug = require_project(self.config, "migrations")?;
        request::<Value, ()>(
            self.config,
            Method::DELETE,
            &project_path(slug, &format!("migrations/pending/{}", enc(pending_id))),
            None,
            None,
        )
        .await
    }
}
