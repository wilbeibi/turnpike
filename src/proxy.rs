use crate::custody::Custody;
use crate::json_usage::JsonUsageExtractor;
use crate::parsers::{model_from_request_body, model_from_response_value, raw_usage_value};
use crate::paths::{calls_db, keys_env};
use crate::peer::resolve_peer_exe;
use crate::providers::{MergeSse, ParseJson, Provider, PROVIDERS};
use crate::record::{classify_error, Record, Store, Usage};
use crate::sse::SseSplitter;
use anyhow::Result;
use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use bytes::Bytes;
use futures_util::{stream, Stream, StreamExt};
use jiff::Timestamp;
use log::{info, warn};
use reqwest::{Body as ReqwestBody, Client};
use serde_json::Value;
use std::net::{Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::net::TcpListener;
use tokio::sync::mpsc::{self, error::TrySendError};

#[derive(Clone)]
struct ProxyState {
    provider: &'static Provider,
    client: Client,
    store: Arc<Mutex<Store>>,
    custody: Arc<Custody>,
    /// Test seam: send every provider here instead of its `upstream_url`.
    upstream_override: Option<String>,
}

const MAX_MODEL_INSPECT_BYTES: usize = 256 * 1024;
/// Responses API streams echo the entire request (instructions, tools) inside
/// the `response.completed` event, so a single event can approach the size of
/// the request body itself — far past the old 64 KiB chat-delta ceiling.
const MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;
const MAX_CLIENT_BYTES: usize = 128;
/// Cap on verbatim usage sub-objects retained for the `raw_usage` audit
/// column. Providers send one (OpenAI/Gemini) or two (Anthropic
/// `message_start` + `message_delta`); the cap only guards against a
/// pathological stream repeating usage-shaped events.
const MAX_RAW_USAGE_OBJS: usize = 8;
const BODY_CHANNEL_CAP: usize = 4;
/// Must absorb one MAX_SSE_EVENT_BYTES event arriving as ~16 KiB h2 frames
/// while the observer is busy serde-parsing a previous request-echoing event
/// (Responses API `response.created` bodies); otherwise the tee drops and
/// zeroes usage on exactly the largest — most expensive — calls.
const OBSERVER_CHANNEL_CAP: usize = 64;

#[derive(Clone)]
struct RecordBase {
    id: String,
    ts: String,
    provider: String,
    model: Option<String>,
    endpoint: String,
    status: Option<u16>,
    stream: bool,
    started: Instant,
    client: Option<String>,
    /// Provenance of `client`: "header" (declared via x-turnpike-client) or
    /// "ua" (User-Agent fallback). A `&'static str` because the vocabulary is
    /// closed; read-time attribution ranks the two.
    client_source: Option<&'static str>,
    /// Peer socket address, carried to the write task so the caller's process
    /// can be resolved off the forward path (invariant 2).
    peer: SocketAddr,
}

enum ObserveMsg {
    Chunk {
        bytes: Bytes,
        elapsed_ms: u64,
    },
    Finish {
        elapsed_ms: u64,
    },
    UpstreamError {
        elapsed_ms: u64,
        kind: String,
        message: String,
    },
    ClientDisconnect {
        elapsed_ms: u64,
    },
}

enum ObserverKind {
    Sse {
        merge: MergeSse,
    },
    Json {
        parse: ParseJson,
        usage_key: &'static str,
        enabled: bool,
    },
}

pub async fn run_all() -> Result<()> {
    if !crate::paths::prices_json().exists() {
        eprintln!("warning: no price table found; run `turnpike prices pull` to fetch one");
    }
    let client = Client::builder().use_rustls_tls().build()?;
    let store = Arc::new(Mutex::new(Store::open(&calls_db())?));
    let custody = Arc::new(Custody::open(keys_env()));
    custody.clone().spawn_watch();

    let mut handles = Vec::new();

    for provider in PROVIDERS {
        let state = ProxyState {
            provider,
            client: client.clone(),
            store: store.clone(),
            custody: custody.clone(),
            upstream_override: None,
        };

        let app = router(state);

        // Bind both loopback stacks. `<provider>.localhost` resolves to ::1
        // before 127.0.0.1 on most systems, so an IPv4-only listener silently
        // refuses the name-routed form. Two explicit loopback sockets fix that
        // without widening exposure — never bind `::`/`0.0.0.0`, which would
        // put turnpike's API keys on the network. IPv4 is required; the ::1
        // bind is best-effort so IPv6-disabled hosts degrade to v4-only.
        let port = provider.default_port;
        let v4 = SocketAddr::from(([127, 0, 0, 1], port));
        let listener = TcpListener::bind(v4).await?;
        info!("turnpike [{}] listening on http://{}", provider.name, v4);
        handles.push(spawn_serve(listener, app.clone()));

        let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, port));
        match TcpListener::bind(v6).await {
            Ok(listener) => {
                info!("turnpike [{}] listening on http://{}", provider.name, v6);
                handles.push(spawn_serve(listener, app));
            }
            Err(e) => warn!(
                "turnpike [{}] IPv6 loopback bind failed ({e}); \
                 {}.localhost may not resolve — use http://127.0.0.1:{port}",
                provider.name, provider.name
            ),
        }
    }

    for h in handles {
        let _ = h.await;
    }

    // All servers have stopped; fold the WAL back and refresh stats so the
    // DB is compact and query-ready at rest.
    store.lock().unwrap_or_else(|e| e.into_inner()).checkpoint();
    Ok(())
}

fn router(state: ProxyState) -> Router {
    Router::new()
        .fallback(handle_request)
        .with_state(Arc::new(state))
}

fn spawn_serve(listener: TcpListener, app: Router) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // ConnectInfo carries the peer SocketAddr into handle_request so the
        // caller's process can be resolved from /proc (peer.rs).
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap_or_else(|e| warn!("serve error: {e}"));
    })
}

async fn handle_request(
    State(state): State<Arc<ProxyState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request<Body>,
) -> Result<Response<Body>, StatusCode> {
    let t0 = Instant::now();
    let ts = Timestamp::now().to_string();
    let call_id = new_call_id();

    let (parts, body) = req.into_parts();
    let method = parts.method;
    let uri = parts.uri;
    let path = uri.path();
    let mut headers = parts.headers;

    // `<provider>.localhost` in the Host header routes by name from any turnpike
    // port; a plain host keeps the port's provider.
    let provider = match provider_from_host(&headers, uri.authority().map(|a| a.as_str())) {
        HostRoute::Named(p) => p,
        HostRoute::None => state.provider,
        HostRoute::Unknown(name) => {
            warn!("turnpike: unknown provider alias {name:?}.localhost; refusing to forward");
            return Err(StatusCode::MISDIRECTED_REQUEST);
        }
    };

    // Credential custody (invariant 1b): a sentinel becomes this provider's
    // key; any other credential is forwarded as sent. The map is snapshotted
    // once, so a reload mid-request cannot change this request's key.
    if let Err(refusal) =
        crate::custody::apply(&mut headers, provider, state.custody.snapshot().as_deref())
    {
        warn!("turnpike [{}]: {}", provider.name, refusal.message());
        return Ok(refusal.response());
    }

    // Model from path (Gemini) or from body.
    let model_from_path = (provider.model_from_path)(path);

    // Attribution is a passive read; the forwarded header set is unchanged.
    // Provenance rides along so read-time views can rank declared identity
    // (header) over observed identity (peer_exe, then the UA fallback).
    let (client, client_source) = match client_from_headers(&headers) {
        Some((value, source)) => (Some(value), Some(source)),
        None => (None, None),
    };

    let needs_body_read = should_inspect_body(&headers)
        && (model_from_path.is_none() || provider.inject_stream_options);
    let (model_from_body, upstream_body) = if needs_body_read {
        let body_bytes = axum::body::to_bytes(body, MAX_MODEL_INSPECT_BYTES)
            .await
            .map_err(|_| StatusCode::BAD_REQUEST)?;
        let model = if model_from_path.is_none() {
            model_from_request_body(&body_bytes)
        } else {
            None
        };
        let forwarded = if provider.inject_stream_options {
            maybe_inject_stream_options(body_bytes)
        } else {
            body_bytes
        };
        (model, ReqwestBody::from(forwarded))
    } else {
        (None, ReqwestBody::wrap_stream(body.into_data_stream()))
    };

    let model = model_from_path.or(model_from_body);

    // Build upstream URL.
    let base = state
        .upstream_override
        .as_deref()
        .unwrap_or(provider.upstream_url);
    let upstream = format!("{base}{uri}");

    let mut upstream_req = state
        .client
        .request(method.clone(), &upstream)
        .body(upstream_body);

    // Forward end-to-end headers. reqwest derives Host from the upstream URL
    // and Content-Length from the body, so we strip both here.
    let request_connection_tokens = connection_tokens(&headers);
    for (name, value) in &headers {
        if *name == header::HOST
            || *name == header::ACCEPT_ENCODING
            || *name == header::CONTENT_LENGTH
            || is_hop_by_hop_header(name, &request_connection_tokens)
        {
            continue;
        }
        upstream_req = upstream_req.header(name, value);
    }

    let endpoint = path.split('?').next().unwrap_or(path).to_string();

    let upstream_resp = match upstream_req.send().await {
        Ok(r) => r,
        Err(e) => {
            let message = sanitized_reqwest_error(&e);
            let kind = classify_error(&message);
            if is_inference_endpoint(&endpoint) {
                let rec = Record {
                    id: call_id,
                    ts,
                    provider: provider.name.to_string(),
                    model,
                    status: None,
                    latency_ms: t0.elapsed().as_millis() as u64,
                    ttft_ms: None,
                    stream: false,
                    input_tokens: None,
                    output_tokens: None,
                    cache_read_input_tokens: None,
                    cache_creation_input_tokens: None,
                    reasoning_output_tokens: None,
                    error_kind: Some(kind.to_string()),
                    error_message: Some(message),
                    cost: None,
                    client,
                    client_source: client_source.map(str::to_string),
                    endpoint: Some(endpoint.clone()),
                    anomaly: None,
                    raw_usage: None,
                    peer_exe: None,
                };
                spawn_record_write(state.store.clone(), rec, peer);
            }
            return Err(StatusCode::BAD_GATEWAY);
        }
    };

    let status = upstream_resp.status();
    let resp_headers = upstream_resp.headers().clone();

    let is_sse = resp_headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.contains("text/event-stream"))
        .unwrap_or(false);

    // Build response builder with upstream status + headers.
    let mut builder = Response::builder().status(status.as_u16());
    let response_connection_tokens = connection_tokens(&resp_headers);
    for (name, value) in &resp_headers {
        if is_hop_by_hop_header(name, &response_connection_tokens) {
            continue;
        }
        builder = builder.header(name, value);
    }

    let base = RecordBase {
        id: call_id,
        ts,
        provider: provider.name.to_string(),
        model,
        endpoint,
        status: Some(status.as_u16()),
        stream: is_sse,
        started: t0,
        client,
        client_source,
        peer,
    };

    let observer_kind = if is_sse {
        ObserverKind::Sse {
            merge: provider.merge_sse,
        }
    } else {
        ObserverKind::Json {
            parse: provider.parse_json,
            usage_key: provider.json_usage_key,
            enabled: status.is_success() && is_json_response(&resp_headers),
        }
    };

    let (body_tx, body_rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(BODY_CHANNEL_CAP);
    let (obs_tx, obs_rx) = mpsc::channel::<ObserveMsg>(OBSERVER_CHANNEL_CAP);
    let observer_dropped = Arc::new(AtomicBool::new(false));

    spawn_observer(
        observer_kind,
        base.clone(),
        state.store.clone(),
        observer_dropped.clone(),
        obs_rx,
    );

    let mut byte_stream = upstream_resp.bytes_stream();
    let forward_task = tokio::spawn(async move {
        while let Some(chunk_res) = byte_stream.next().await {
            let chunk = match chunk_res {
                Ok(c) => c,
                Err(e) => {
                    let message = sanitized_reqwest_error(&e);
                    let kind = classify_error(&message).to_string();
                    let _ = body_tx
                        .send(Err(std::io::Error::other(message.clone())))
                        .await;
                    drop(body_tx);
                    let _ = obs_tx
                        .send(ObserveMsg::UpstreamError {
                            elapsed_ms: base.started.elapsed().as_millis() as u64,
                            kind,
                            message,
                        })
                        .await;
                    return;
                }
            };

            // h2 bodies often end with an empty END_STREAM frame. It carries
            // no bytes, and forwarding it races the client's close-after-full-
            // body, misclassifying a completed call as client_disconnect.
            if chunk.is_empty() {
                continue;
            }

            if !observer_dropped.load(Ordering::Relaxed) {
                try_observe(
                    &obs_tx,
                    &observer_dropped,
                    ObserveMsg::Chunk {
                        bytes: chunk.clone(),
                        elapsed_ms: base.started.elapsed().as_millis() as u64,
                    },
                );
            }

            if body_tx.send(Ok(chunk)).await.is_err() {
                drop(body_tx);
                let _ = obs_tx
                    .send(ObserveMsg::ClientDisconnect {
                        elapsed_ms: base.started.elapsed().as_millis() as u64,
                    })
                    .await;
                return;
            }
        }

        drop(body_tx);
        let _ = obs_tx
            .send(ObserveMsg::Finish {
                elapsed_ms: base.started.elapsed().as_millis() as u64,
            })
            .await;
    });
    std::mem::drop(forward_task);

    let body = Body::from_stream(receiver_stream(body_rx));
    builder
        .body(body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

fn new_call_id() -> String {
    let mut buf = [0u8; 16];
    getrandom::getrandom(&mut buf).unwrap_or(());
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn spawn_observer(
    kind: ObserverKind,
    mut base: RecordBase,
    store: Arc<Mutex<Store>>,
    dropped: Arc<AtomicBool>,
    mut rx: mpsc::Receiver<ObserveMsg>,
) {
    let handle = tokio::spawn(async move {
        let mut usage = Usage::default();
        let mut ttft_ms: Option<u64> = None;
        // Verbatim usage sub-objects, retained for the `raw_usage` audit column.
        let mut raw_usage_objs: Vec<Value> = Vec::new();
        // Distinguishes "an SSE event outgrew its cap" from "the tee
        // channel overflowed"; both zero usage, the anomaly column says why.
        let mut sse_overflow = false;
        let mut sse_splitter = match &kind {
            ObserverKind::Sse { .. } => Some(SseSplitter::new(MAX_SSE_EVENT_BYTES)),
            ObserverKind::Json { .. } => None,
        };
        let mut json_extractor = match &kind {
            ObserverKind::Json { usage_key, .. } => Some(JsonUsageExtractor::new(usage_key)),
            ObserverKind::Sse { .. } => None,
        };

        while let Some(msg) = rx.recv().await {
            match msg {
                ObserveMsg::Chunk { bytes, elapsed_ms } => {
                    if dropped.load(Ordering::Relaxed) {
                        continue;
                    }

                    match &kind {
                        ObserverKind::Sse { merge } => {
                            if ttft_ms.is_none() && !bytes.is_empty() {
                                ttft_ms = Some(elapsed_ms);
                            }
                            let Some(splitter) = sse_splitter.as_mut() else {
                                continue;
                            };
                            let events = match splitter.push(&bytes) {
                                Ok(events) => events,
                                Err(_) => {
                                    sse_overflow = true;
                                    dropped.store(true, Ordering::Relaxed);
                                    continue;
                                }
                            };
                            for event in events {
                                if !should_parse_sse_event(&event.event_type, &event.data) {
                                    continue;
                                }
                                if let Ok(data) = serde_json::from_str::<Value>(&event.data) {
                                    if data.is_object() {
                                        // Backfill model when the request body
                                        // was too large to inspect; streaming
                                        // responses echo it on the chunk that
                                        // also carries usage / message_start.
                                        if base.model.is_none() {
                                            base.model = model_from_response_value(&data);
                                        }
                                        merge(&event.event_type, &data, &mut usage);
                                        if raw_usage_objs.len() < MAX_RAW_USAGE_OBJS {
                                            if let Some(u) = raw_usage_value(&data) {
                                                raw_usage_objs.push(u);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        ObserverKind::Json { enabled, .. } => {
                            if *enabled {
                                if let Some(extractor) = json_extractor.as_mut() {
                                    extractor.push(&bytes);
                                }
                            }
                        }
                    }
                }
                ObserveMsg::Finish { elapsed_ms } => {
                    let mut anomaly = terminal_anomaly(sse_overflow, &dropped, &mut usage);
                    // `sse_overflow`/`observation_dropped` mean the capture was
                    // degraded; `no_usage` (added below) does not — its raw
                    // object is worth keeping precisely because it went
                    // unparsed.
                    let degraded = anomaly.is_some();
                    if anomaly.is_none() {
                        finalize_json_usage(
                            &kind,
                            &mut json_extractor,
                            &mut base,
                            &mut usage,
                            &mut raw_usage_objs,
                        );
                    }
                    // Canary: a successful inference that captured no usage at
                    // all is a silent metering miss (unknown provider shape,
                    // non-SSE stream, usage withheld) — flag it so it is
                    // distinguishable from a genuine zero-token call.
                    if anomaly.is_none()
                        && is_success(base.status)
                        && is_inference_endpoint(&base.endpoint)
                        && usage_is_empty(&usage)
                    {
                        anomaly = Some("no_usage".to_string());
                    }
                    let raw_usage = raw_usage_json(&raw_usage_objs, !degraded);
                    if is_inference_endpoint(&base.endpoint) {
                        spawn_record_write(
                            store,
                            record_from_base(
                                &base, usage, elapsed_ms, ttft_ms, None, None, anomaly, raw_usage,
                            ),
                            base.peer,
                        );
                    }
                    return;
                }
                ObserveMsg::UpstreamError {
                    elapsed_ms,
                    kind: error_kind,
                    message,
                } => {
                    let anomaly = terminal_anomaly(sse_overflow, &dropped, &mut usage);
                    if anomaly.is_none() {
                        finalize_json_usage(
                            &kind,
                            &mut json_extractor,
                            &mut base,
                            &mut usage,
                            &mut raw_usage_objs,
                        );
                    }
                    let raw_usage = raw_usage_json(&raw_usage_objs, anomaly.is_none());
                    if is_inference_endpoint(&base.endpoint) {
                        spawn_record_write(
                            store,
                            record_from_base(
                                &base,
                                usage,
                                elapsed_ms,
                                ttft_ms,
                                Some(error_kind),
                                Some(message),
                                anomaly,
                                raw_usage,
                            ),
                            base.peer,
                        );
                    }
                    return;
                }
                ObserveMsg::ClientDisconnect { elapsed_ms } => {
                    let anomaly = terminal_anomaly(sse_overflow, &dropped, &mut usage);
                    if anomaly.is_none() {
                        finalize_json_usage(
                            &kind,
                            &mut json_extractor,
                            &mut base,
                            &mut usage,
                            &mut raw_usage_objs,
                        );
                    }
                    let raw_usage = raw_usage_json(&raw_usage_objs, anomaly.is_none());
                    if is_inference_endpoint(&base.endpoint) {
                        spawn_record_write(
                            store,
                            record_from_base(
                                &base,
                                usage,
                                elapsed_ms,
                                ttft_ms,
                                Some("client_disconnect".to_string()),
                                Some("downstream client disconnected".to_string()),
                                anomaly,
                                raw_usage,
                            ),
                            base.peer,
                        );
                    }
                    return;
                }
            }
        }
    });
    std::mem::drop(handle);
}

/// Degraded observation at a terminal arm: partial sums are untrustworthy,
/// so usage is zeroed — and the row says why instead of leaving NULL tokens
/// indistinguishable from "provider sent no usage".
fn terminal_anomaly(sse_overflow: bool, dropped: &AtomicBool, usage: &mut Usage) -> Option<String> {
    if sse_overflow {
        *usage = Usage::default();
        Some("sse_overflow".to_string())
    } else if dropped.load(Ordering::Relaxed) {
        *usage = Usage::default();
        Some("observation_dropped".to_string())
    } else {
        None
    }
}

/// Terminal-arm JSON finalization: a fully-captured usage object is
/// all-or-nothing and therefore trustworthy even when the client
/// disconnected or the upstream failed after the body was observed.
fn finalize_json_usage(
    kind: &ObserverKind,
    json_extractor: &mut Option<JsonUsageExtractor>,
    base: &mut RecordBase,
    usage: &mut Usage,
    raw_usage_objs: &mut Vec<Value>,
) {
    if let ObserverKind::Json { parse, enabled, .. } = kind {
        if *enabled {
            if let Some(extractor) = json_extractor.take() {
                if base.model.is_none() {
                    base.model = extractor.model().map(String::from);
                }
                if let Some(v) = extractor.finish_wrapped() {
                    // `finish_wrapped` returns `{usage_key: <usage>}`; store the
                    // inner object verbatim for `raw_usage`.
                    if let Some(inner) = v.as_object().and_then(|m| m.values().next()) {
                        raw_usage_objs.push(inner.clone());
                    }
                    *usage = parse(&v);
                }
            }
        }
    }
}

/// Serialize retained usage sub-objects for the `raw_usage` column. Returns
/// `None` when `trustworthy` is false (a degraded/anomalous observation) or
/// nothing was captured. A single object stays an object; usage split across
/// events (Anthropic) becomes a JSON array.
fn raw_usage_json(objs: &[Value], trustworthy: bool) -> Option<String> {
    if !trustworthy {
        return None;
    }
    match objs {
        [] => None,
        [one] => serde_json::to_string(one).ok(),
        many => serde_json::to_string(&Value::Array(many.to_vec())).ok(),
    }
}

fn is_success(status: Option<u16>) -> bool {
    matches!(status, Some(s) if (200..300).contains(&s))
}

fn usage_is_empty(u: &Usage) -> bool {
    u.input_tokens.is_none()
        && u.output_tokens.is_none()
        && u.cache_read_input_tokens.is_none()
        && u.cache_creation_input_tokens.is_none()
        && u.reasoning_output_tokens.is_none()
        && u.cost.is_none()
}

fn try_observe(tx: &mpsc::Sender<ObserveMsg>, dropped: &AtomicBool, msg: ObserveMsg) {
    match tx.try_send(msg) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => dropped.store(true, Ordering::Relaxed),
        Err(TrySendError::Closed(_)) => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn record_from_base(
    base: &RecordBase,
    usage: Usage,
    latency_ms: u64,
    ttft_ms: Option<u64>,
    error_kind: Option<String>,
    error_message: Option<String>,
    anomaly: Option<String>,
    raw_usage: Option<String>,
) -> Record {
    Record {
        id: base.id.clone(),
        ts: base.ts.clone(),
        provider: base.provider.clone(),
        model: base.model.clone(),
        status: base.status,
        latency_ms,
        ttft_ms,
        stream: base.stream,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
        reasoning_output_tokens: usage.reasoning_output_tokens,
        error_kind,
        error_message,
        cost: usage.cost,
        client: base.client.clone(),
        client_source: base.client_source.map(str::to_string),
        endpoint: Some(base.endpoint.clone()),
        anomaly,
        raw_usage,
        // Resolved from the peer socket in spawn_record_write, off the
        // forward path (invariant 2).
        peer_exe: None,
    }
}

enum HostRoute {
    Named(&'static Provider),
    Unknown(String),
    None,
}

/// Resolve a `<provider>.localhost[:port]` Host (or h2 `:authority`) to a
/// provider by name. A `*.localhost` label that matches no provider is a hard
/// error, never a fallback — falling through to the port's provider would
/// forward (and leak) one provider's credentials to a different upstream.
/// Bare `localhost`, `127.0.0.1`, and anything else keep the port's provider.
fn provider_from_host(headers: &HeaderMap, authority: Option<&str>) -> HostRoute {
    let raw = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or(authority);
    let Some(raw) = raw else {
        return HostRoute::None;
    };
    // Strip an optional :port. IPv6 literals ([::1]:4000) fail the suffix
    // test below regardless of how this split lands on them.
    let host = raw.split(':').next().unwrap_or(raw).to_ascii_lowercase();
    let Some(name) = host.strip_suffix(".localhost") else {
        return HostRoute::None;
    };
    if name.is_empty() {
        return HostRoute::None;
    }
    match PROVIDERS.iter().find(|p| p.name == name) {
        Some(p) => HostRoute::Named(p),
        None => HostRoute::Unknown(name.to_string()),
    }
}

/// Client identity for per-tool attribution: `x-turnpike-client` when the
/// caller sets one, else the request `User-Agent`. Returns the value plus
/// its provenance — "header" (declared) or "ua" (fallback) — because
/// read-time attribution ranks declared identity over observed. A present
/// but blank header yields `None` (no UA fallback), as before.
fn client_from_headers(headers: &HeaderMap) -> Option<(String, &'static str)> {
    if let Some(raw) = headers.get("x-turnpike-client") {
        return normalize_client(raw).map(|s| (s, "header"));
    }
    headers
        .get(header::USER_AGENT)
        .and_then(|raw| normalize_client(raw).map(|s| (s, "ua")))
}

/// Trim and byte-truncate one header value into a stored client string.
/// `HeaderValue::to_str` only passes visible-ASCII values, so byte
/// truncation cannot split a char.
fn normalize_client(raw: &HeaderValue) -> Option<String> {
    let raw = raw.to_str().ok()?.trim();
    if raw.is_empty() {
        return None;
    }
    let mut s = raw.to_string();
    s.truncate(MAX_CLIENT_BYTES);
    Some(s)
}

/// turnpike records *inference* — requests that consume tokens and cost money.
/// The OpenAI-compatible inference surface (plus Anthropic / Gemini) is
/// small and stable; the junk clients probe (`/api/tags`, `/version`,
/// `/props`, model listings, ...) is open-ended. So we allowlist inference
/// rather than chase an ever-growing denylist of probes. Calls are still
/// proxied normally — this only governs whether we log them.
///
/// Bias toward inclusion: anything that can carry a `usage` object must
/// match, or we silently lose cost data.
fn is_inference_endpoint(endpoint: &str) -> bool {
    const MARKERS: &[&str] = &[
        "/completions", // /v1/completions and /v1/chat/completions
        "/embeddings",
        "/messages",       // Anthropic
        "/responses",      // OpenAI Responses API
        "/transcriptions", // Whisper-style audio (Groq/OpenAI) — billed usage
        "/translations",   // Whisper-style audio translation
        "generatecontent", // Gemini :generateContent / :streamGenerateContent
    ];
    let e = endpoint.to_ascii_lowercase();
    MARKERS.iter().any(|m| e.contains(m))
}

fn spawn_record_write(store: Arc<Mutex<Store>>, mut record: Record, peer: SocketAddr) {
    let handle = tokio::task::spawn_blocking(move || {
        // Resolve the caller's process here, on the blocking pool: the /proc
        // scan is detached from the forward path (invariant 2) and this task
        // already blocks on the DB write, so it is the natural place for it.
        // A client that exits before this task runs resolves to None, so
        // persistent callers (agents, daemons) attribute reliably while
        // one-shot scripts may not. No cache: the scan is O(system-wide fds),
        // not call rate, and stays off-path — cheap enough here; add an inode
        // cache only if a large /proc ever makes it show up under load.
        record.peer_exe = resolve_peer_exe(peer);
        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = s.insert(&record) {
            warn!("failed to write turnpike record: {e}");
        }
    });
    std::mem::drop(handle);
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
}

fn should_inspect_body(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .is_some_and(|len| len <= MAX_MODEL_INSPECT_BYTES)
}

fn is_json_response(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("json"))
}

fn receiver_stream(
    rx: tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> {
    stream::unfold(rx, |mut rx| async {
        rx.recv().await.map(|item| (item, rx))
    })
}

fn should_parse_sse_event(event_type: &str, data: &str) -> bool {
    matches!(event_type, "message_start" | "message_delta")
        || data.contains("\"usage\"")
        || data.contains("\"usageMetadata\"")
}

fn connection_tokens(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|token| token.trim().to_ascii_lowercase())
        .filter(|token| !token.is_empty())
        .collect()
}

fn is_hop_by_hop_header(name: &HeaderName, connection_tokens: &[String]) -> bool {
    let name = name.as_str();
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    ) || connection_tokens
        .iter()
        .any(|token| token.eq_ignore_ascii_case(name))
}

fn sanitized_reqwest_error(err: &reqwest::Error) -> String {
    sanitize_error_message(&err.to_string(), err.url())
}

fn sanitize_error_message(message: &str, url: Option<&reqwest::Url>) -> String {
    let Some(url) = url else {
        return message.to_string();
    };
    message.replace(url.as_str(), &redacted_url(url))
}

fn redacted_url(url: &reqwest::Url) -> String {
    let mut redacted = url.clone();
    let _ = redacted.set_username("");
    let _ = redacted.set_password(None);
    redacted.set_query(None);
    redacted.set_fragment(None);
    redacted.to_string()
}

/// If the body is a streaming JSON request (`"stream": true`), inject
/// `stream_options: {"include_usage": true}` so the final SSE chunk carries
/// token counts. Falls back to the original bytes on any parse failure.
fn maybe_inject_stream_options(body: Bytes) -> Bytes {
    let Ok(mut v) = serde_json::from_slice::<Value>(&body) else {
        return body;
    };
    let Some(obj) = v.as_object_mut() else {
        return body;
    };
    if obj.get("stream").and_then(|v| v.as_bool()) != Some(true) {
        return body;
    }
    match obj.get_mut("stream_options") {
        Some(Value::Object(so)) => {
            so.insert("include_usage".to_string(), serde_json::json!(true));
        }
        _ => {
            obj.insert(
                "stream_options".to_string(),
                serde_json::json!({"include_usage": true}),
            );
        }
    }
    serde_json::to_vec(&v).map(Bytes::from).unwrap_or(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bytes(v: serde_json::Value) -> Bytes {
        Bytes::from(serde_json::to_vec(&v).unwrap())
    }

    #[test]
    fn inject_adds_stream_options_when_streaming() {
        let out = maybe_inject_stream_options(bytes(json!({"model": "gpt-4o", "stream": true})));
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["stream_options"]["include_usage"], json!(true));
    }

    #[test]
    fn inject_skips_non_streaming_requests() {
        let input = bytes(json!({"model": "gpt-4o", "stream": false}));
        let out = maybe_inject_stream_options(input.clone());
        assert_eq!(out, input);
    }

    #[test]
    fn inject_overwrites_existing_include_usage() {
        let out = maybe_inject_stream_options(bytes(json!({
            "model": "gpt-4o",
            "stream": true,
            "stream_options": {"include_usage": false, "extra": 1}
        })));
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["stream_options"]["include_usage"], json!(true));
        assert_eq!(v["stream_options"]["extra"], json!(1));
    }

    #[test]
    fn inject_falls_back_on_invalid_json() {
        let garbage = Bytes::from_static(b"not json");
        let out = maybe_inject_stream_options(garbage.clone());
        assert_eq!(out, garbage);
    }

    #[test]
    fn inference_endpoints_are_recorded() {
        for e in [
            "/v1/chat/completions",
            "/api/v1/chat/completions", // OpenRouter
            "/v1/completions",
            "/v1/embeddings",
            "/v1/messages",                    // Anthropic
            "/v1/responses",                   // OpenAI Responses
            "/openai/v1/audio/transcriptions", // Groq whisper
            "/v1/audio/translations",
            "/v1beta/models/gemini-2.0:generateContent", // Gemini
            "/v1beta/models/gemini-2.0:streamGenerateContent",
        ] {
            assert!(is_inference_endpoint(e), "{e} should be recorded");
        }
    }

    #[test]
    fn probes_and_listings_are_skipped() {
        for e in [
            "/api/tags",
            "/api/show",
            "/api/v1/models",
            "/props",
            "/v1/props",
            "/version",
            "/v1/models",
            "/v1/models/deepseek-v4-pro",
        ] {
            assert!(!is_inference_endpoint(e), "{e} should be skipped");
        }
    }

    fn host_route(host: Option<&str>, authority: Option<&str>) -> HostRoute {
        let mut h = HeaderMap::new();
        if let Some(v) = host {
            h.insert("host", v.parse().unwrap());
        }
        provider_from_host(&h, authority)
    }

    #[test]
    fn host_alias_routes_by_name_from_any_port() {
        for host in [
            "openrouter.localhost:4000",
            "OpenRouter.LOCALHOST",
            "openrouter.localhost",
        ] {
            match host_route(Some(host), None) {
                HostRoute::Named(p) => assert_eq!(p.name, "openrouter"),
                _ => panic!("{host} should route to openrouter"),
            }
        }
    }

    #[test]
    fn plain_hosts_keep_the_port_provider() {
        for host in [
            "127.0.0.1:4003",
            "localhost:4003",
            "localhost",
            "[::1]:4000",
        ] {
            assert!(matches!(host_route(Some(host), None), HostRoute::None));
        }
        assert!(matches!(host_route(None, None), HostRoute::None));
    }

    #[test]
    fn unknown_alias_is_refused_not_misrouted() {
        // api.openrouter.localhost and foo.localhost must never fall through
        // to the port's provider: that would send credentials to the wrong
        // upstream.
        for host in ["foo.localhost:4003", "api.openrouter.localhost"] {
            assert!(matches!(
                host_route(Some(host), None),
                HostRoute::Unknown(_)
            ));
        }
    }

    #[test]
    fn h2_authority_is_honored_when_host_header_absent() {
        match host_route(None, Some("deepseek.localhost:4000")) {
            HostRoute::Named(p) => assert_eq!(p.name, "deepseek"),
            _ => panic!("authority should route to deepseek"),
        }
    }

    #[test]
    fn client_prefers_turnpike_header_over_user_agent() {
        let mut h = HeaderMap::new();
        h.insert("user-agent", "node".parse().unwrap());
        h.insert("x-turnpike-client", "opencode".parse().unwrap());
        assert_eq!(client_from_headers(&h), Some(("opencode".into(), "header")));
    }

    #[test]
    fn client_falls_back_to_user_agent_with_ua_provenance() {
        let mut h = HeaderMap::new();
        h.insert("user-agent", "python-requests/2.32".parse().unwrap());
        assert_eq!(
            client_from_headers(&h),
            Some(("python-requests/2.32".into(), "ua"))
        );
    }

    #[test]
    fn blank_user_agent_is_none() {
        let mut h = HeaderMap::new();
        h.insert("user-agent", "  ".parse().unwrap());
        assert_eq!(client_from_headers(&h), None);
    }

    #[test]
    fn long_user_agent_is_truncated_to_bound() {
        let mut h = HeaderMap::new();
        h.insert("user-agent", "x".repeat(1000).parse().unwrap());
        assert_eq!(client_from_headers(&h).unwrap().0.len(), MAX_CLIENT_BYTES);
    }

    #[test]
    fn no_client_headers_is_none() {
        assert_eq!(client_from_headers(&HeaderMap::new()), None);
    }

    #[test]
    fn blank_declared_header_does_not_fall_back_to_ua() {
        let mut h = HeaderMap::new();
        h.insert("x-turnpike-client", "".parse().unwrap());
        h.insert("user-agent", "node".parse().unwrap());
        assert_eq!(client_from_headers(&h), None);
    }

    #[test]
    fn error_url_is_redacted_before_persistence() {
        let url = reqwest::Url::parse(
            "https://user:secret@api.example.com/v1/messages?api_key=sk-secret&alt=sse#frag",
        )
        .unwrap();
        let message = format!("error sending request for url ({url})");
        let sanitized = sanitize_error_message(&message, Some(&url));

        assert!(!sanitized.contains("sk-secret"));
        assert!(!sanitized.contains("user:secret"));
        assert!(!sanitized.contains("alt=sse"));
        assert_eq!(
            sanitized,
            "error sending request for url (https://api.example.com/v1/messages)"
        );
    }

    #[test]
    fn raw_usage_json_shapes_by_count_and_trust() {
        let one = serde_json::json!({"prompt_tokens": 10});
        let two = serde_json::json!({"input_tokens": 5});
        // Nothing captured.
        assert_eq!(raw_usage_json(&[], true), None);
        // Single object stays an object.
        assert_eq!(
            raw_usage_json(std::slice::from_ref(&one), true).as_deref(),
            Some(r#"{"prompt_tokens":10}"#)
        );
        // Split usage (Anthropic) becomes an array.
        let arr = raw_usage_json(&[one.clone(), two], true).unwrap();
        assert!(arr.starts_with('[') && arr.contains("input_tokens"));
        // A degraded observation stores nothing, even with objects in hand.
        assert_eq!(raw_usage_json(std::slice::from_ref(&one), false), None);
    }

    #[test]
    fn no_usage_canary_conditions() {
        // Success + empty usage is the flagged case; anything else is not.
        assert!(is_success(Some(200)) && usage_is_empty(&Usage::default()));
        assert!(!is_success(Some(500)));
        assert!(!is_success(None));
        let with_tokens = Usage {
            output_tokens: Some(1),
            ..Default::default()
        };
        assert!(!usage_is_empty(&with_tokens));
    }
}

/// Custody through the real handler: a fake upstream on an ephemeral port
/// reports the credential it received. Never binds turnpike's own ports.
#[cfg(test)]
mod custody_tests {
    use super::*;
    use crate::custody::test_support::{write_keys, TempDir};
    use crate::custody::{parse, Custody};
    use tokio::sync::Notify;

    const SENTINEL: &str = "tp-0123456789abcdef0123";

    #[derive(Default)]
    struct Upstream {
        seen: Mutex<Vec<String>>,
        release: Notify,
    }

    /// `/slow` streams one SSE event, waits for `release`, then finishes;
    /// anything else answers at once. Both record the Authorization header.
    async fn fake_upstream(up: State<Arc<Upstream>>, req: Request<Body>) -> Response<Body> {
        let auth = req
            .headers()
            .get(header::AUTHORIZATION)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        up.seen.lock().unwrap().push(auth);
        let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(4);
        tx.send(Ok(Bytes::from("data: {\"part\":1}\n\n")))
            .await
            .unwrap();
        let up = up.0.clone();
        let slow = req.uri().path() == "/slow";
        tokio::spawn(async move {
            if slow {
                up.release.notified().await;
            }
            let _ = tx.send(Ok(Bytes::from("data: {\"part\":2}\n\n"))).await;
        });
        Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(tokio_stream(rx)))
            .unwrap()
    }

    fn tokio_stream<T>(mut rx: mpsc::Receiver<T>) -> impl Stream<Item = T> {
        stream::poll_fn(move |cx| rx.poll_recv(cx))
    }

    async fn serve(app: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        format!("http://{addr}")
    }

    /// (proxy base URL, upstream) for the deepseek port with `custody`.
    async fn setup(custody: Arc<Custody>) -> (String, Arc<Upstream>) {
        let up = Arc::new(Upstream::default());
        let upstream_url =
            serve(Router::new().fallback(fake_upstream).with_state(up.clone())).await;
        let state = ProxyState {
            provider: PROVIDERS.iter().find(|p| p.name == "deepseek").unwrap(),
            client: Client::new(),
            store: Arc::new(Mutex::new(Store::open_in_memory().unwrap())),
            custody,
            upstream_override: Some(upstream_url),
        };
        (serve(router(state)).await, up)
    }

    fn keys(key: &str) -> String {
        format!("DEEPSEEK_API_KEY={key}\nTURNPIKE_SENTINEL_TEST={SENTINEL}\n")
    }

    async fn call(client: &Client, url: &str, token: &str) -> reqwest::Response {
        client
            .post(url)
            .bearer_auth(token)
            .body("{}")
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_reload_mid_stream_keeps_that_streams_key() {
        let dir = TempDir::new("proxy-reload");
        let path = dir.0.join("keys.env");
        write_keys(&path, &keys("sk-old"), 0o600);
        let custody = Arc::new(Custody::open(path.clone()));
        let (proxy, up) = setup(custody.clone()).await;
        let client = Client::new();

        let slow = call(&client, &format!("{proxy}/slow"), SENTINEL).await;
        assert_eq!(slow.status(), 200);
        let mut body = slow.bytes_stream();
        let first = body.next().await.unwrap().unwrap();
        assert!(first.starts_with(b"data: {\"part\":1}"));

        // Rotate while the stream is open, then prove the new key is live.
        write_keys(&path, &keys("sk-new"), 0o600);
        custody.reload().unwrap();
        let fast = call(&client, &format!("{proxy}/fast"), SENTINEL).await;
        assert_eq!(fast.status(), 200);
        fast.bytes().await.unwrap();

        // A malformed save now changes nothing either.
        write_keys(&path, "DEEPSEEK_API_KEY=sk-broken\nGARBAGE\n", 0o600);
        assert!(custody.reload().is_err());

        up.release.notify_one();
        let mut rest = Vec::new();
        while let Some(chunk) = body.next().await {
            rest.extend_from_slice(&chunk.unwrap());
        }
        assert!(rest.starts_with(b"data: {\"part\":2}"));

        let after = call(&client, &format!("{proxy}/fast"), SENTINEL).await;
        after.bytes().await.unwrap();
        assert_eq!(
            *up.seen.lock().unwrap(),
            ["Bearer sk-old", "Bearer sk-new", "Bearer sk-new"]
        );
    }

    #[tokio::test]
    async fn an_unmapped_sentinel_is_refused_before_the_upstream() {
        let map = parse(&keys("sk-fake")).unwrap();
        let (proxy, up) = setup(Arc::new(Custody::with_map(Some(map)))).await;
        let resp = call(&Client::new(), &proxy, "tp-ffffffffffffffffffff").await;
        assert_eq!(resp.status(), 401);
        let body: Value = serde_json::from_slice(&resp.bytes().await.unwrap()).unwrap();
        assert_eq!(body["error"]["type"], "turnpike");
        assert!(!body.to_string().contains("tp-ffff"));
        assert!(up.seen.lock().unwrap().is_empty());
    }

    /// Live check, run by hand: `cargo test live_deepseek -- --ignored`.
    /// The key comes from `$DEEPSEEK_API_KEY` into memory only — no file —
    /// and goes through the running turnpike on :4003 so the calls are
    /// metered. Only status codes are ever printed: a provider's 401 body
    /// can echo part of the key.
    #[tokio::test]
    #[ignore]
    async fn live_deepseek_through_a_sentinel() {
        let key = std::env::var("DEEPSEEK_API_KEY").expect("DEEPSEEK_API_KEY");
        assert!(
            !key.starts_with("tp-"),
            "the environment already holds a sentinel"
        );
        let upstream =
            std::env::var("TURNPIKE_LIVE_UPSTREAM").unwrap_or("http://127.0.0.1:4003".into());
        let chat = |client: &Client, proxy: &str, token: &str| {
            client
                .post(format!("{proxy}/v1/chat/completions"))
                .bearer_auth(token)
                .header("x-turnpike-client", "turnpike-test:custody-live")
                .header(header::CONTENT_TYPE, "application/json")
                .body(
                    r#"{"model":"deepseek-chat","stream":true,"max_tokens":8,
                        "messages":[{"role":"user","content":"Reply with: ok"}]}"#,
                )
                .send()
        };
        let live = |map: String| {
            let upstream = upstream.clone();
            async move {
                let state = ProxyState {
                    provider: PROVIDERS.iter().find(|p| p.name == "deepseek").unwrap(),
                    client: Client::new(),
                    store: Arc::new(Mutex::new(Store::open_in_memory().unwrap())),
                    custody: Arc::new(Custody::with_map(Some(parse(&map).unwrap()))),
                    upstream_override: Some(upstream),
                };
                serve(router(state)).await
            }
        };
        let client = Client::new();

        // The sentinel is swapped for the real key; the stream carries usage.
        let proxy = live(format!(
            "DEEPSEEK_API_KEY={key}\nTURNPIKE_SENTINEL_TEST={SENTINEL}\n"
        ))
        .await;
        let resp = chat(&client, &proxy, SENTINEL).await.unwrap();
        let status = resp.status().as_u16();
        let body = resp.bytes().await.unwrap();
        assert!(status == 200, "sentinel call: HTTP {status}");
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("\"usage\""), "no usage event in the stream");
        assert!(text.contains("[DONE]"), "stream did not finish");

        // A bad current key fails loudly; the good _PREVIOUS is never tried.
        let proxy = live(format!(
            "DEEPSEEK_API_KEY=sk-00000000000000000000000000000000\n\
             DEEPSEEK_API_KEY_PREVIOUS={key}\nTURNPIKE_SENTINEL_TEST={SENTINEL}\n"
        ))
        .await;
        let status = chat(&client, &proxy, SENTINEL)
            .await
            .unwrap()
            .status()
            .as_u16();
        assert!(status == 401, "bad key: HTTP {status}, want 401");

        // An unknown sentinel never leaves the machine.
        let status = chat(&client, &proxy, "tp-ffffffffffffffffffff")
            .await
            .unwrap()
            .status()
            .as_u16();
        assert!(status == 401, "unknown sentinel: HTTP {status}");
    }

    #[tokio::test]
    async fn a_real_key_passes_through_with_or_without_custody() {
        for map in [Some(parse(&keys("sk-fake")).unwrap()), None] {
            let (proxy, up) = setup(Arc::new(Custody::with_map(map))).await;
            let resp = call(&Client::new(), &proxy, "sk-client-own").await;
            assert_eq!(resp.status(), 200);
            resp.bytes().await.unwrap();
            assert_eq!(*up.seen.lock().unwrap(), ["Bearer sk-client-own"]);
        }
    }
}
