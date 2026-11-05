//! Keep extractor failures in the Admin API error format without echoing input values.

use axum::{
    extract::{FromRequest, FromRequestParts, Request},
    http::{StatusCode, request::Parts},
    response::{IntoResponse, Response},
};
use serde::de::DeserializeOwned;

use super::types::AdminErrorResponse;

fn rejection(status: StatusCode, message: &str) -> Response {
    (
        status,
        axum::Json(AdminErrorResponse::invalid_request(message)),
    )
        .into_response()
}

pub(super) struct AdminJson<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for AdminJson<T> {
    type Rejection = Response;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        axum::Json::<T>::from_request(request, state)
            .await
            .map(|axum::Json(value)| Self(value))
            .map_err(|error| {
                let status = error.status();
                let message = match status {
                    StatusCode::PAYLOAD_TOO_LARGE => "Request body exceeds the size limit",
                    StatusCode::UNSUPPORTED_MEDIA_TYPE => "Expected application/json Content-Type",
                    StatusCode::UNPROCESSABLE_ENTITY => {
                        "JSON fields do not match the request schema"
                    }
                    _ => "Invalid JSON request body",
                };
                rejection(status, message)
            })
    }
}

pub(super) struct AdminPath<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned + Send> FromRequestParts<S> for AdminPath<T> {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        axum::extract::Path::<T>::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Path(value)| Self(value))
            .map_err(|error| rejection(error.status(), "Invalid path parameter"))
    }
}

pub(crate) struct AdminQuery<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for AdminQuery<T> {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        axum::extract::Query::<T>::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Query(value)| Self(value))
            .map_err(|error| rejection(error.status(), "Invalid query parameters"))
    }
}
