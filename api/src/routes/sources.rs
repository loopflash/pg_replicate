use actix_web::{
    delete, get,
    http::{header::ContentType, StatusCode},
    post,
    web::{Data, Json, Path},
    HttpRequest, HttpResponse, Responder, ResponseError,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use thiserror::Error;
use utoipa::ToSchema;

use super::ErrorMessage;
use crate::{
    db::{
        self,
        sources::{SourceConfig, SourcesDbError},
    },
    encryption::EncryptionKey,
};

pub mod publications;
pub mod tables;

#[derive(Debug, Error)]
enum SourceError {
    #[error("database error: {0}")]
    DatabaseError(#[from] sqlx::Error),

    #[error("source with id {0} not found")]
    SourceNotFound(i64),

    #[error("tenant id missing in request")]
    TenantIdMissing,

    #[error("tenant id ill formed in request")]
    TenantIdIllFormed,

    #[error("sources db error: {0}")]
    SourcesDb(#[from] SourcesDbError),
}

impl SourceError {
    fn to_message(&self) -> String {
        match self {
            // Do not expose internal database details in error messages
            SourceError::DatabaseError(_) => "internal server error".to_string(),
            // Every other message is ok, as they do not divulge sensitive information
            e => e.to_string(),
        }
    }
}

impl ResponseError for SourceError {
    fn status_code(&self) -> StatusCode {
        match self {
            SourceError::DatabaseError(_) | SourceError::SourcesDb(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
            SourceError::SourceNotFound(_) => StatusCode::NOT_FOUND,
            SourceError::TenantIdMissing | SourceError::TenantIdIllFormed => {
                StatusCode::BAD_REQUEST
            }
        }
    }

    fn error_response(&self) -> HttpResponse {
        let error_message = ErrorMessage {
            error: self.to_message(),
        };
        let body =
            serde_json::to_string(&error_message).expect("failed to serialize error message");
        HttpResponse::build(self.status_code())
            .insert_header(ContentType::json())
            .body(body)
    }
}

#[derive(Deserialize, ToSchema)]
pub struct PostSourceRequest {
    #[schema(required = true)]
    pub config: SourceConfig,
}

#[derive(Serialize, ToSchema)]
pub struct PostSourceResponse {
    id: i64,
}

#[derive(Serialize, ToSchema)]
pub struct GetSourceResponse {
    #[schema(example = 1)]
    id: i64,
    #[schema(example = 1)]
    tenant_id: i64,
    config: SourceConfig,
}

// TODO: read tenant_id from a jwt
fn extract_tenant_id(req: &HttpRequest) -> Result<i64, SourceError> {
    let headers = req.headers();
    let tenant_id = headers
        .get("tenant_id")
        .ok_or(SourceError::TenantIdMissing)?;
    let tenant_id = tenant_id
        .to_str()
        .map_err(|_| SourceError::TenantIdIllFormed)?;
    let tenant_id: i64 = tenant_id
        .parse()
        .map_err(|_| SourceError::TenantIdIllFormed)?;
    Ok(tenant_id)
}

#[utoipa::path(
    request_body = PostSourceRequest,
    responses(
        (status = 200, description = "Create new source", body = PostSourceResponse),
        (status = 500, description = "Internal server error")
    )
)]
#[post("/sources")]
pub async fn create_source(
    req: HttpRequest,
    pool: Data<PgPool>,
    encryption_key: Data<EncryptionKey>,
    source: Json<PostSourceRequest>,
) -> Result<impl Responder, SourceError> {
    let source = source.0;
    let tenant_id = extract_tenant_id(&req)?;
    let config = source.config;
    let id = db::sources::create_source(&pool, tenant_id, config, &encryption_key).await?;
    let response = PostSourceResponse { id };
    Ok(Json(response))
}

#[utoipa::path(
    params(
        ("source_id" = i64, Path, description = "Id of the source"),
    ),
    responses(
        (status = 200, description = "Return source with id = source_id", body = GetSourceResponse),
        (status = 404, description = "Source not found"),
        (status = 500, description = "Internal server error")
    )
)]
#[get("/sources/{source_id}")]
pub async fn read_source(
    req: HttpRequest,
    pool: Data<PgPool>,
    encryption_key: Data<EncryptionKey>,
    source_id: Path<i64>,
) -> Result<impl Responder, SourceError> {
    let tenant_id = extract_tenant_id(&req)?;
    let source_id = source_id.into_inner();
    let response = db::sources::read_source(&pool, tenant_id, source_id, &encryption_key)
        .await?
        .map(|s| GetSourceResponse {
            id: s.id,
            tenant_id: s.tenant_id,
            config: s.config,
        })
        .ok_or(SourceError::SourceNotFound(source_id))?;
    Ok(Json(response))
}

#[utoipa::path(
    request_body = PostSourceRequest,
    params(
        ("source_id" = i64, Path, description = "Id of the source"),
    ),
    responses(
        (status = 200, description = "Update source with id = source_id"),
        (status = 404, description = "Source not found"),
        (status = 500, description = "Internal server error")
    )
)]
#[post("/sources/{source_id}")]
pub async fn update_source(
    req: HttpRequest,
    pool: Data<PgPool>,
    source_id: Path<i64>,
    encryption_key: Data<EncryptionKey>,
    source: Json<PostSourceRequest>,
) -> Result<impl Responder, SourceError> {
    let source = source.0;
    let tenant_id = extract_tenant_id(&req)?;
    let source_id = source_id.into_inner();
    let config = source.config;
    db::sources::update_source(&pool, tenant_id, source_id, config, &encryption_key)
        .await?
        .ok_or(SourceError::SourceNotFound(source_id))?;
    Ok(HttpResponse::Ok().finish())
}

#[utoipa::path(
    params(
        ("source_id" = i64, Path, description = "Id of the source"),
    ),
    responses(
        (status = 200, description = "Delete source with id = source_id"),
        (status = 404, description = "Source not found"),
        (status = 500, description = "Internal server error")
    )
)]
#[delete("/sources/{source_id}")]
pub async fn delete_source(
    req: HttpRequest,
    pool: Data<PgPool>,
    source_id: Path<i64>,
) -> Result<impl Responder, SourceError> {
    let tenant_id = extract_tenant_id(&req)?;
    let source_id = source_id.into_inner();
    db::sources::delete_source(&pool, tenant_id, source_id)
        .await?
        .ok_or(SourceError::SourceNotFound(tenant_id))?;
    Ok(HttpResponse::Ok().finish())
}

#[utoipa::path(
    responses(
        (status = 200, description = "Return all sources"),
        (status = 500, description = "Internal server error")
    )
)]
#[get("/sources")]
pub async fn read_all_sources(
    req: HttpRequest,
    pool: Data<PgPool>,
    encryption_key: Data<EncryptionKey>,
) -> Result<impl Responder, SourceError> {
    let tenant_id = extract_tenant_id(&req)?;
    let mut sources = vec![];
    for source in db::sources::read_all_sources(&pool, tenant_id, &encryption_key).await? {
        let source = GetSourceResponse {
            id: source.id,
            tenant_id: source.tenant_id,
            config: source.config,
        };
        sources.push(source);
    }
    Ok(Json(sources))
}
