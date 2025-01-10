use std::{collections::{HashMap, HashSet}, str::FromStr};

use async_trait::async_trait;
use tokio_postgres::types::{FromSql, PgLsn};
use tracing::info;

use crate::{
    conversions::{cdc_event::CdcEvent, table_row::TableRow},
    pipeline::PipelineResumptionState,
    table::{TableId, TableSchema},
};

use super::{BatchSink, InfallibleSinkError};

pub type Callback = Box<dyn Fn() + Send + Sync>;

pub struct NotificationSink {
    pub lsn: u64,
    pub callback: Callback,
}

#[async_trait]
impl BatchSink for NotificationSink {
    type Error = InfallibleSinkError;
    async fn get_resumption_state(&mut self) -> Result<PipelineResumptionState, Self::Error> {
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
        for event in events {
            match event {
                CdcEvent::Insert(_) | CdcEvent::Update(_) | CdcEvent::Delete(_) => {
                    (self.callback)();
                }
                _ => (),
            }
            info!("{event:?}");
        }
        Ok(PgLsn::from(0))
    }

    async fn table_copied(&mut self, table_id: TableId) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn truncate_table(&mut self, table_id: TableId) -> Result<(), Self::Error> {
        Ok(())
    }
}
