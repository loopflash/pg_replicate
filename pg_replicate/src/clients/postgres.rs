use std::{collections::HashMap, str::FromStr};

use native_tls::{Certificate, TlsConnector};
use pg_escape::{quote_identifier, quote_literal};
use postgres_native_tls::MakeTlsConnector;
use postgres_replication::LogicalReplicationStream;
use thiserror::Error;
use tokio_postgres::{
    config::ReplicationMode,
    types::{Kind, PgLsn, Type},
    Client as PostgresClient, Config, CopyOutStream, NoTls, SimpleQueryMessage,
};
use tracing::{info, warn};

use crate::table::{ColumnSchema, TableId, TableName, TableSchema};

pub struct SlotInfo {
    pub confirmed_flush_lsn: PgLsn,
}

/// A client for Postgres logical replication
pub struct ReplicationClient {
    postgres_client: PostgresClient,
    in_txn: bool,
}

#[derive(Debug, Error)]
pub enum ReplicationClientError {
    #[error("tokio_postgres error: {0}")]
    TokioPostgresError(#[from] tokio_postgres::Error),

    #[error("column {0} is missing from table {1}")]
    MissingColumn(String, String),

    #[error("publication {0} doesn't exist")]
    MissingPublication(String),

    #[error("oid column is not a valid u32")]
    OidColumnNotU32,

    #[error("replica identity '{0}' not supported")]
    ReplicaIdentityNotSupported(String),

    #[error("type modifier column is not a valid u32")]
    TypeModifierColumnNotI32,

    #[error("column {0}'s type with oid {1} in relation {2} is not supported")]
    UnsupportedType(String, u32, String),

    #[error("table {0} doesn't exist")]
    MissingTable(TableName),

    #[error("not a valid PgLsn")]
    InvalidPgLsn,

    #[error("failed to create slot")]
    FailedToCreateSlot,
}

impl ReplicationClient {
    pub async fn connect_tls() -> Result<ReplicationClient, ReplicationClientError> {
        info!("connecting to postgres");
        let mut pg_config = tokio_postgres::Config::new();
        pg_config.host(std::env::var("POSTGRES_HOST").unwrap().as_str());
        pg_config.port(
            std::env::var("POSTGRES_PORT")
                .unwrap()
                .as_str()
                .parse()
                .unwrap(),
        );
        pg_config.user(std::env::var("POSTGRES_USERNAME").unwrap().as_str());
        pg_config.dbname(std::env::var("POSTGRES_DATABASE").unwrap().as_str());
        pg_config.replication_mode(ReplicationMode::Logical);
        
        let mode_password = std::env::var("POSTGRES_PASSWORD_MODE").unwrap_or("static".to_string());
        match mode_password.as_str() {
            "static" => {
                pg_config.password(std::env::var("POSTGRES_PASSWORD").unwrap().as_str());
            }
            "azure-dynamic" => {
                use azure_core::credentials::TokenCredential;
                use azure_identity::DefaultAzureCredential;
                let credential = DefaultAzureCredential::new().unwrap();
                let token = credential
                    .get_token(&["https://ossrdbms-aad.database.windows.net"])
                    .await
                    .unwrap();
                pg_config.password(token.token.secret());
            }
            _ => {}
        };

        let enable_tls = std::env::var("POSTGRES_OWN_CERT")
            .unwrap_or("false".to_string())
            .parse::<bool>()
            .unwrap();

        let mut connector = TlsConnector::builder();
        if enable_tls {
            let cert = std::fs::read("/etc/loopflash/certs/db.crt").unwrap();
            let cert = Certificate::from_pem(&cert).unwrap();
            connector.add_root_certificate(cert);
        }
        let connector = connector.build().unwrap();
        let connector = MakeTlsConnector::new(connector);

        let (postgres_client, connection) = pg_config.connect(connector).await.unwrap();
        
        tokio::spawn(async move {
            info!("waiting for connection to terminate");
            if let Err(e) = connection.await {
                warn!("connection error: {}", e);
            }
        });
        
        Ok(ReplicationClient {
            postgres_client,
            in_txn: false,
        })
    }

    /// Starts a read-only trasaction with repeatable read isolation level
    pub async fn begin_readonly_transaction(&mut self) -> Result<(), ReplicationClientError> {
        self.postgres_client
            .simple_query("begin read only isolation level repeatable read;")
            .await?;
        self.in_txn = true;
        Ok(())
    }

    /// Commits a transaction
    pub async fn commit_txn(&mut self) -> Result<(), ReplicationClientError> {
        if self.in_txn {
            self.postgres_client.simple_query("commit;").await?;
            self.in_txn = false;
        }
        Ok(())
    }

    async fn rollback_txn(&mut self) -> Result<(), ReplicationClientError> {
        if self.in_txn {
            self.postgres_client.simple_query("rollback;").await?;
            self.in_txn = false;
        }
        Ok(())
    }

    /// Returns a [CopyOutStream] for a table
    pub async fn get_table_copy_stream(
        &self,
        table_name: &TableName,
    ) -> Result<CopyOutStream, ReplicationClientError> {
        let copy_query = format!(
            r#"COPY {} TO STDOUT WITH (FORMAT text);"#,
            table_name.as_quoted_identifier()
        );
        let stream = self.postgres_client.copy_out_simple(&copy_query).await?;

        Ok(stream)
    }

    /// Returns a vector of columns of a table
    pub async fn get_column_schemas(
        &self,
        table_id: TableId,
    ) -> Result<Vec<ColumnSchema>, ReplicationClientError> {
        let column_info_query = format!(
            "select a.attname,
                a.atttypid,
                a.atttypmod,
                a.attnotnull,
                coalesce(i.indisprimary, false) as primary
            from pg_attribute a
            left join pg_index i
                on a.attrelid = i.indrelid
                and a.attnum = any(i.indkey)
                and i.indisprimary = true
            where a.attnum > 0::int2
            and not a.attisdropped
            and a.attgenerated = ''
            and a.attrelid = {table_id}
            order by a.attnum
            ",
        );

        let mut column_schemas = vec![];

        for message in self
            .postgres_client
            .simple_query(&column_info_query)
            .await?
        {
            if let SimpleQueryMessage::Row(row) = message {
                let name = row
                    .try_get("attname")?
                    .ok_or(ReplicationClientError::MissingColumn(
                        "attname".to_string(),
                        "pg_attribute".to_string(),
                    ))?
                    .to_string();

                let type_oid = row
                    .try_get("atttypid")?
                    .ok_or(ReplicationClientError::MissingColumn(
                        "atttypid".to_string(),
                        "pg_attribute".to_string(),
                    ))?
                    .parse()
                    .map_err(|_| ReplicationClientError::OidColumnNotU32)?;

                //TODO: For now we assume all types are simple, fix it later
                let typ = Type::from_oid(type_oid).unwrap_or(Type::new(
                    format!("unnamed(oid: {type_oid})"),
                    type_oid,
                    Kind::Simple,
                    "pg_catalog".to_string(),
                ));

                let modifier = row
                    .try_get("atttypmod")?
                    .ok_or(ReplicationClientError::MissingColumn(
                        "atttypmod".to_string(),
                        "pg_attribute".to_string(),
                    ))?
                    .parse()
                    .map_err(|_| ReplicationClientError::TypeModifierColumnNotI32)?;

                let nullable =
                    row.try_get("attnotnull")?
                        .ok_or(ReplicationClientError::MissingColumn(
                            "attnotnull".to_string(),
                            "pg_attribute".to_string(),
                        ))?
                        == "f";

                let primary =
                    row.try_get("primary")?
                        .ok_or(ReplicationClientError::MissingColumn(
                            "indisprimary".to_string(),
                            "pg_index".to_string(),
                        ))?
                        == "t";

                column_schemas.push(ColumnSchema {
                    name,
                    typ,
                    modifier,
                    nullable,
                    primary,
                })
            }
        }

        Ok(column_schemas)
    }

    pub async fn get_table_schemas(
        &self,
        table_names: &[TableName],
    ) -> Result<HashMap<TableId, TableSchema>, ReplicationClientError> {
        let mut table_schemas = HashMap::new();

        for table_name in table_names {
            let table_schema = self.get_table_schema(table_name.clone()).await?;
            if !table_schema.has_primary_keys() {
                warn!(
                    "table {} with id {} will not be copied because it has no primary key",
                    table_schema.table_name, table_schema.table_id
                );
                continue;
            }
            table_schemas.insert(table_schema.table_id, table_schema);
        }

        Ok(table_schemas)
    }

    async fn get_table_schema(
        &self,
        table_name: TableName,
    ) -> Result<TableSchema, ReplicationClientError> {
        let table_id = self
            .get_table_id(&table_name)
            .await?
            .ok_or(ReplicationClientError::MissingTable(table_name.clone()))?;
        let column_schemas = self.get_column_schemas(table_id).await?;
        Ok(TableSchema {
            table_name,
            table_id,
            column_schemas,
        })
    }

    /// Returns the table id (called relation id in Postgres) of a table
    /// Also checks whether the replica identity is default or full and
    /// returns an error if not.
    pub async fn get_table_id(
        &self,
        table: &TableName,
    ) -> Result<Option<TableId>, ReplicationClientError> {
        let quoted_schema = quote_literal(&table.schema);
        let quoted_name = quote_literal(&table.name);

        let table_info_query = format!(
            "select c.oid,
                c.relreplident
            from pg_class c
            join pg_namespace n
                on (c.relnamespace = n.oid)
            where n.nspname = {}
                and c.relname = {}
            ",
            quoted_schema, quoted_name
        );

        for message in self.postgres_client.simple_query(&table_info_query).await? {
            if let SimpleQueryMessage::Row(row) = message {
                let replica_identity =
                    row.try_get("relreplident")?
                        .ok_or(ReplicationClientError::MissingColumn(
                            "relreplident".to_string(),
                            "pg_class".to_string(),
                        ))?;

                if !(replica_identity == "d" || replica_identity == "f") {
                    return Err(ReplicationClientError::ReplicaIdentityNotSupported(
                        replica_identity.to_string(),
                    ));
                }

                let oid: u32 = row
                    .try_get("oid")?
                    .ok_or(ReplicationClientError::MissingColumn(
                        "oid".to_string(),
                        "pg_class".to_string(),
                    ))?
                    .parse()
                    .map_err(|_| ReplicationClientError::OidColumnNotU32)?;
                return Ok(Some(oid));
            }
        }

        Ok(None)
    }

    /// Returns the slot info of an existing slot. The slot info currently only has the
    /// confirmed_flush_lsn column of the pg_replication_slots table.
    async fn get_slot(&self, slot_name: &str) -> Result<Option<SlotInfo>, ReplicationClientError> {
        let query = format!(
            r#"select confirmed_flush_lsn from pg_replication_slots where slot_name = {};"#,
            quote_literal(slot_name)
        );

        let query_result = self.postgres_client.simple_query(&query).await?;

        for res in &query_result {
            if let SimpleQueryMessage::Row(row) = res {
                let confirmed_flush_lsn = row
                    .get("confirmed_flush_lsn")
                    .ok_or(ReplicationClientError::MissingColumn(
                        "confirmed_flush_lsn".to_string(),
                        "pg_replication_slots".to_string(),
                    ))?
                    .parse()
                    .map_err(|_| ReplicationClientError::InvalidPgLsn)?;

                return Ok(Some(SlotInfo {
                    confirmed_flush_lsn,
                }));
            }
        }

        Ok(None)
    }

    /// Creates a logical replication slot. This will only succeed if the postgres connection
    /// is in logical replication mode. Otherwise it will fail with the following error:
    /// `syntax error at or near "CREATE_REPLICATION_SLOT"``
    ///
    /// Returns the consistent_point column as slot info.
    async fn create_slot(&self, slot_name: &str) -> Result<SlotInfo, ReplicationClientError> {
        let query = format!(
            r#"CREATE_REPLICATION_SLOT {} LOGICAL pgoutput USE_SNAPSHOT"#,
            quote_identifier(slot_name)
        );
        let results = self.postgres_client.simple_query(&query).await?;

        for result in results {
            if let SimpleQueryMessage::Row(row) = result {
                let consistent_point: PgLsn = row
                    .get("consistent_point")
                    .ok_or(ReplicationClientError::MissingColumn(
                        "consistent_point".to_string(),
                        "create_replication_slot".to_string(),
                    ))?
                    .parse()
                    .map_err(|_| ReplicationClientError::InvalidPgLsn)?;
                return Ok(SlotInfo {
                    confirmed_flush_lsn: consistent_point,
                });
            }
        }
        Err(ReplicationClientError::FailedToCreateSlot)
    }

    /// Either return the slot info of an existing slot or creates a new
    /// slot and returns its slot info.
    pub async fn get_or_create_slot(
        &mut self,
        slot_name: &str,
    ) -> Result<SlotInfo, ReplicationClientError> {
        if let Some(slot_info) = self.get_slot(slot_name).await? {
            Ok(slot_info)
        } else {
            self.rollback_txn().await?;
            self.begin_readonly_transaction().await?;
            Ok(self.create_slot(slot_name).await?)
        }
    }

    /// Returns all table names in a publication
    pub async fn get_publication_table_names(
        &self,
        publication: &str,
    ) -> Result<Vec<TableName>, ReplicationClientError> {
        let publication_query = format!(
            "select schemaname, tablename from pg_publication_tables where pubname = {};",
            quote_literal(publication)
        );

        let mut table_names = vec![];
        for msg in self
            .postgres_client
            .simple_query(&publication_query)
            .await?
        {
            if let SimpleQueryMessage::Row(row) = msg {
                let schema = row
                    .get(0)
                    .ok_or(ReplicationClientError::MissingColumn(
                        "schemaname".to_string(),
                        "pg_publication_tables".to_string(),
                    ))?
                    .to_string();

                let name = row
                    .get(1)
                    .ok_or(ReplicationClientError::MissingColumn(
                        "tablename".to_string(),
                        "pg_publication_tables".to_string(),
                    ))?
                    .to_string();

                table_names.push(TableName { schema, name })
            }
        }

        Ok(table_names)
    }

    pub async fn publication_exists(
        &self,
        publication: &str,
    ) -> Result<bool, ReplicationClientError> {
        let publication_exists_query = format!(
            "select 1 as exists from pg_publication where pubname = {};",
            quote_literal(publication)
        );
        for msg in self
            .postgres_client
            .simple_query(&publication_exists_query)
            .await?
        {
            if let SimpleQueryMessage::Row(_) = msg {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub async fn get_logical_replication_stream(
        &self,
        publication: &str,
        slot_name: &str,
        start_lsn: PgLsn,
    ) -> Result<LogicalReplicationStream, ReplicationClientError> {
        let options = format!(
            r#"("proto_version" '1', "publication_names" {})"#,
            quote_literal(publication),
        );

        let query = format!(
            r#"START_REPLICATION SLOT {} LOGICAL {} {}"#,
            quote_identifier(slot_name),
            start_lsn,
            options
        );

        let copy_stream = self
            .postgres_client
            .copy_both_simple::<bytes::Bytes>(&query)
            .await?;

        let stream = LogicalReplicationStream::new(copy_stream);

        Ok(stream)
    }
}
