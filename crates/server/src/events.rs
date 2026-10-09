//! Cursor validation and replayable SSE over the Core event log.
use super::{ApiError, AppState, ErrorEnvelope, identifier, query};
use aporto_protocol as protocol;
use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event as SseEvent, KeepAlive},
    },
};
use serde::Deserialize;
use std::{convert::Infallible, sync::Arc, time::Duration};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EventQuery {
    after: Option<u64>,
    limit: Option<u32>,
}
fn event_params(
    id: String,
    params: EventQuery,
    headers: HeaderMap,
) -> Result<protocol::EventListParams, ApiError> {
    identifier(&id)?;
    if params.limit.is_some_and(|limit| limit == 0 || limit > 200) {
        return Err(ApiError::invalid("event limit must be between 1 and 200"));
    }
    let after = if params.after.is_some() {
        params.after
    } else {
        let mut values = headers.get_all("last-event-id").iter();
        let value = values.next();
        if values.next().is_some() {
            return Err(ApiError::invalid("multiple Last-Event-ID headers"));
        }
        value
            .map(|value| {
                value
                    .to_str()
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .ok_or_else(|| ApiError::invalid("invalid Last-Event-ID"))
            })
            .transpose()?
    };
    Ok(protocol::EventListParams {
        thread_id: id,
        after,
        limit: params.limit.or(Some(100)),
    })
}

fn validate_page(
    params: &protocol::EventListParams,
    page: &protocol::EventListResult,
) -> Result<(), ApiError> {
    let mut cursor = params.after.unwrap_or(0);
    for event in &page.events {
        if event.thread_id != params.thread_id || event.sequence <= cursor {
            return Err(invalid_page());
        }
        cursor = event.sequence;
    }
    // A cursor beyond the delivered events silently loses data on the next poll.
    if page.next_cursor != cursor
        || (page.has_more && page.events.is_empty())
        || page.events.len() > params.limit.unwrap_or(100) as usize
    {
        return Err(invalid_page());
    }
    Ok(())
}
fn invalid_page() -> ApiError {
    ApiError::new(
        StatusCode::BAD_GATEWAY,
        protocol::INTERNAL_ERROR,
        "invalid Core event cursor",
    )
}
fn error_event(error: ApiError) -> SseEvent {
    let envelope = ErrorEnvelope {
        error: protocol::RpcError::new(error.code, error.message),
    };
    SseEvent::default()
        .event("server_error")
        .json_data(envelope)
        .expect("error envelope serializes")
}
pub(super) async fn events_page(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    params: Result<Query<EventQuery>, QueryRejection>,
) -> Result<Json<protocol::EventListResult>, ApiError> {
    let params = event_params(id, query(params)?, headers)?;
    let page = state.core.call("event/list", &params).await?;
    validate_page(&params, &page)?;
    Ok(Json(page))
}
pub(super) async fn events(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    params: Result<Query<EventQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let permit = state.streams.clone().try_acquire_owned().map_err(|_| {
        ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            protocol::OVERLOADED,
            "SSE subscription capacity reached",
        )
    })?;
    let mut params = event_params(id, query(params)?, headers)?;
    let first: protocol::EventListResult = state.core.call("event/list", &params).await?;
    validate_page(&params, &first)?;
    let stream = async_stream::stream! {
        let _permit = permit;
        let mut page = first;
        loop {
            if state.shutdown.is_cancelled() {
                break;
            }
            if let Err(error) = validate_page(&params, &page) {
                yield Ok::<_, Infallible>(error_event(error));
                break;
            }
            for event in page.events {
                let event = SseEvent::default()
                    .event("agent_event")
                    .id(event.sequence.to_string())
                    .json_data(event);
                match event {
                    Ok(event) => yield Ok(event),
                    Err(_) => {
                        yield Ok(error_event(invalid_page()));
                        return;
                    }
                }
            }
            params.after = Some(page.next_cursor);
            if !page.has_more {
                tokio::select! {
                    () = tokio::time::sleep(state.poll_interval) => {},
                    () = state.shutdown.cancelled() => break,
                }
            }
            let next = tokio::select! {
                result = state.core.call::<protocol::EventListResult>("event/list", &params) => result,
                () = state.shutdown.cancelled() => break,
            };
            match next {
                Ok(next) => page = next,
                Err(error) => {
                    yield Ok(error_event(ApiError::from(error)));
                    break;
                }
            }
        }
    };
    let mut response = Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response();
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_cannot_skip_events_move_back_or_continue_without_progress() {
        let params = protocol::EventListParams {
            thread_id: "one".into(),
            after: Some(4),
            limit: Some(2),
        };
        let event = protocol::Event {
            sequence: 5,
            thread_id: "one".into(),
            turn_id: None,
            kind: "turn.started".into(),
            data: serde_json::json!({}),
            created_at: 1,
        };
        let mut page = protocol::EventListResult {
            events: vec![event.clone()],
            next_cursor: 5,
            has_more: false,
        };
        assert!(validate_page(&params, &page).is_ok());
        page.next_cursor = 6;
        assert!(validate_page(&params, &page).is_err());
        page.next_cursor = 4;
        assert!(validate_page(&params, &page).is_err());
        page.events.clear();
        assert!(validate_page(&params, &page).is_ok());
        page.has_more = true;
        assert!(validate_page(&params, &page).is_err());
        page.events = vec![event.clone(), event];
        page.next_cursor = 5;
        assert!(validate_page(&params, &page).is_err());
        page.events.truncate(1);
        page.events[0].thread_id = "other".into();
        assert!(validate_page(&params, &page).is_err());
    }
}
