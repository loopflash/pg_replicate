use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    str::FromStr,
};

use async_trait::async_trait;
use futures::future::BoxFuture;
use thiserror::Error;
use tokio_postgres::types::{FromSql, PgLsn};
use tracing::info;

use crate::{
    conversions::{cdc_event::CdcEvent, table_row::TableRow, Cell},
    pipeline::PipelineResumptionState,
    table::{TableId, TableSchema},
};

use super::{BatchSink, InfallibleSinkError, SinkError};

type Callback = Box<dyn Fn(Vec<Vec<Cell>>) -> BoxFuture<'static, ()> + Send + Sync>;

#[derive(Debug, Error)]
pub enum NotificationSinkError {
    #[error("incorrect commit lsn: {0}(expected: {0})")]
    IncorrectCommitLsn(PgLsn, PgLsn),

    #[error("commit message without begin message")]
    CommitWithoutBegin,
}

impl SinkError for NotificationSinkError {}

pub struct NotificationSink {
    pub lsn: u64,
    pub callback: Callback,
    pub committed_lsn: Option<PgLsn>,
    pub final_lsn: Option<PgLsn>,
}

#[async_trait]
impl BatchSink for NotificationSink {
    type Error = NotificationSinkError;
    async fn get_resumption_state(&mut self) -> Result<PipelineResumptionState, Self::Error> {
        self.committed_lsn = Some(PgLsn::from(self.lsn));

        Ok(PipelineResumptionState {
            copied_tables: HashSet::new(),
            last_lsn: PgLsn::from(self.lsn),
        })
    }

    async fn write_table_schemas(
        &mut self,
        table_schemas: HashMap<TableId, TableSchema>,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn write_table_rows(
        &mut self,
        rows: Vec<TableRow>,
        _table_id: TableId,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn write_cdc_events(&mut self, events: Vec<CdcEvent>) -> Result<PgLsn, Self::Error> {
        let mut list_events = Vec::new();
        let mut new_last_lsn = PgLsn::from(0);
        for event in events {
            match event {
                CdcEvent::Begin(begin_body) => {
                    let final_lsn_u64 = begin_body.final_lsn();
                    self.final_lsn = Some(final_lsn_u64.into());
                }
                CdcEvent::Commit(commit_body) => {
                    let commit_lsn: PgLsn = commit_body.commit_lsn().into();
                    if let Some(final_lsn) = self.final_lsn {
                        if commit_lsn == final_lsn {
                            new_last_lsn = commit_lsn;
                        } else {
                            Err(NotificationSinkError::IncorrectCommitLsn(
                                commit_lsn, final_lsn,
                            ))?
                        }
                    } else {
                        Err(NotificationSinkError::CommitWithoutBegin)?
                    }
                }
                CdcEvent::Insert((_, value)) => {
                    let values = value.values.into_iter().collect::<Vec<_>>();
                    list_events.push(values);
                }
                _ => {},
            }
        }

        if !list_events.is_empty() {
            (self.callback)(list_events).await;
        }

        if new_last_lsn != PgLsn::from(0) {
            self.committed_lsn = Some(new_last_lsn);
        }

        let committed_lsn = self.committed_lsn.expect("committed lsn is none");
        Ok(committed_lsn)
    }

    async fn table_copied(&mut self, table_id: TableId) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn truncate_table(&mut self, table_id: TableId) -> Result<(), Self::Error> {
        Ok(())
    }
}
