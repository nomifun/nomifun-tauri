//! HTTP routes for `/api/presets` and `/api/preset-tags`.

use axum::{Json, Router, body::Body, extract::{Path, State, rejection::JsonRejection}, http::{HeaderValue, StatusCode, header}, response::Response, routing::{get, patch, post}};
use nomifun_api_types::*;
use nomifun_common::AppError;

pub use crate::state::PresetRouterState;

pub fn preset_routes(state: PresetRouterState) -> Router {
    Router::new()
        .route("/api/presets", get(list).post(create))
        .route("/api/presets/{id}", get(get_one).put(update).delete(delete_one))
        .route("/api/presets/{id}/state", patch(set_state))
        .route("/api/presets/{id}/resolve", post(resolve))
        .route("/api/presets/{id}/avatar", get(get_avatar))
        .route("/api/presets/import", post(import))
        .route("/api/preset-tags", get(list_tags).post(create_tag))
        .route("/api/preset-tags/{preset_tag_id}", axum::routing::put(update_tag).delete(delete_tag))
        .with_state(state)
}

async fn list(State(s):State<PresetRouterState>)->Result<Json<ApiResponse<Vec<PresetResponse>>>,AppError>{Ok(Json(ApiResponse::ok(s.service.list().await?)))}
async fn get_one(State(s):State<PresetRouterState>,Path(id):Path<String>)->Result<Json<ApiResponse<PresetResponse>>,AppError>{Ok(Json(ApiResponse::ok(s.service.get(&id).await?)))}
async fn create(State(s):State<PresetRouterState>,body:Result<Json<CreatePresetRequest>,JsonRejection>)->Result<(StatusCode,Json<ApiResponse<PresetResponse>>),AppError>{let Json(r)=body.map_err(|e|AppError::BadRequest(e.to_string()))?;Ok((StatusCode::CREATED,Json(ApiResponse::ok(s.service.create(r).await?))))}
async fn update(State(s):State<PresetRouterState>,Path(id):Path<String>,body:Result<Json<UpdatePresetRequest>,JsonRejection>)->Result<Json<ApiResponse<PresetResponse>>,AppError>{let Json(r)=body.map_err(|e|AppError::BadRequest(e.to_string()))?;Ok(Json(ApiResponse::ok(s.service.update(&id,r).await?)))}
async fn delete_one(State(s):State<PresetRouterState>,Path(id):Path<String>)->Result<Json<ApiResponse<()>>,AppError>{s.service.delete(&id).await?;Ok(Json(ApiResponse::success()))}
async fn set_state(State(s):State<PresetRouterState>,Path(id):Path<String>,body:Result<Json<SetPresetStateRequest>,JsonRejection>)->Result<Json<ApiResponse<PresetResponse>>,AppError>{let Json(r)=body.map_err(|e|AppError::BadRequest(e.to_string()))?;Ok(Json(ApiResponse::ok(s.service.set_state(&id,r).await?)))}
async fn resolve(State(s):State<PresetRouterState>,Path(id):Path<String>,body:Result<Json<ResolvePresetRequest>,JsonRejection>)->Result<Json<ApiResponse<ResolvedPresetSnapshot>>,AppError>{let Json(r)=body.map_err(|e|AppError::BadRequest(e.to_string()))?;Ok(Json(ApiResponse::ok(s.service.resolve(&id,r.target,r.locale.as_deref(),r.overrides).await?)))}
async fn import(State(s):State<PresetRouterState>,body:Result<Json<ImportPresetsRequest>,JsonRejection>)->Result<Json<ApiResponse<ImportPresetsResult>>,AppError>{let Json(r)=body.map_err(|e|AppError::BadRequest(e.to_string()))?;Ok(Json(ApiResponse::ok(s.service.import(r).await?)))}
async fn list_tags(State(s):State<PresetRouterState>)->Result<Json<ApiResponse<Vec<PresetTagResponse>>>,AppError>{Ok(Json(ApiResponse::ok(s.service.list_tags().await?)))}
async fn create_tag(State(s):State<PresetRouterState>,body:Result<Json<CreatePresetTagRequest>,JsonRejection>)->Result<(StatusCode,Json<ApiResponse<PresetTagResponse>>),AppError>{let Json(r)=body.map_err(|e|AppError::BadRequest(e.to_string()))?;Ok((StatusCode::CREATED,Json(ApiResponse::ok(s.service.create_tag(r).await?))))}
async fn update_tag(State(s):State<PresetRouterState>,Path(preset_tag_id):Path<String>,body:Result<Json<UpdatePresetTagRequest>,JsonRejection>)->Result<Json<ApiResponse<PresetTagResponse>>,AppError>{let Json(r)=body.map_err(|e|AppError::BadRequest(e.to_string()))?;Ok(Json(ApiResponse::ok(s.service.update_tag(&preset_tag_id,r).await?)))}
async fn delete_tag(State(s):State<PresetRouterState>,Path(preset_tag_id):Path<String>)->Result<Json<ApiResponse<()>>,AppError>{s.service.delete_tag(&preset_tag_id).await?;Ok(Json(ApiResponse::success()))}

async fn get_avatar(State(s):State<PresetRouterState>,Path(id):Path<String>)->Result<Response,AppError>{let asset=s.service.avatar_asset(&id).await.ok_or_else(||AppError::NotFound(format!("avatar '{id}' not found")))?;Response::builder().status(StatusCode::OK).header(header::CONTENT_TYPE,content_type(asset.extension.as_deref())).body(Body::from(asset.bytes)).map_err(|e|AppError::Internal(e.to_string()))}
fn content_type(ext:Option<&str>)->HeaderValue{HeaderValue::from_static(match ext{Some("svg")=>"image/svg+xml",Some("png")=>"image/png",Some("jpg"|"jpeg")=>"image/jpeg",Some("gif")=>"image/gif",Some("webp")=>"image/webp",_=>"application/octet-stream"})}
