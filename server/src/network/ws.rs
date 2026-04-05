use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};

use super::agent_relay::{self, AgentRelays};
use super::commands::GameCommand;

#[derive(Clone)]
pub struct AppState {
    pub broadcast_tx: Arc<broadcast::Sender<Bytes>>,
    pub command_tx: mpsc::Sender<GameCommand>,
    pub stripe_secret: Option<String>,
    pub agent_relays: AgentRelays,
}

pub async fn start_server(state: AppState) {
    let app = Router::new()
        .route("/health", get(health))
        .route("/ws", get(ws_upgrade))
        .route("/api/bounties", post(create_bounty))
        .route("/api/bounties", get(list_bounties))
        .route("/api/contracts", post(create_contract))
        .route("/api/stripe/test", get(stripe_test))
        .route("/agent/{id}/ws", get({
            let relays = state.agent_relays.clone();
            move |ws, path| agent_relay::agent_ws_handler(ws, path, axum::extract::State(relays))
        }))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080")
        .await
        .expect("Failed to bind to port 8080");

    tracing::info!("axum listening on 0.0.0.0:8080");
    axum::serve(listener, app).await.expect("axum server failed");
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

// --- WebSocket ---

async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state.broadcast_tx))
}

async fn handle_socket(mut socket: WebSocket, tx: Arc<broadcast::Sender<Bytes>>) {
    tracing::info!("client connected");
    let mut rx = tx.subscribe();

    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(bytes) => {
                        if socket.send(Message::Binary(bytes)).await.is_err() { break; }
                    }
                    Err(_) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(_)) => {}
                    _ => break,
                }
            }
        }
    }

    tracing::info!("client disconnected");
}

// --- REST: Bounty creation ---

#[derive(Deserialize)]
struct CreateBountyRequest {
    description: String,
    reward_gold: u32,
    /// Optional Stripe payment intent ID for paid bounties.
    payment_intent_id: Option<String>,
    /// Bounty objective type.
    objective: Option<String>,
}

#[derive(Serialize)]
struct CreateBountyResponse {
    id: String,
    description: String,
    reward_gold: u32,
    funded: bool,
}

async fn create_bounty(
    State(state): State<AppState>,
    Json(req): Json<CreateBountyRequest>,
) -> Result<Json<CreateBountyResponse>, (StatusCode, String)> {
    let mut reward = req.reward_gold;
    let mut funded = false;

    // If a payment intent is provided, verify with Stripe.
    if let Some(ref pi_id) = req.payment_intent_id {
        if let Some(ref secret) = state.stripe_secret {
            match verify_stripe_payment(secret, pi_id).await {
                Ok(amount_cents) => {
                    // Convert cents to gold: $1 = 10 gold.
                    reward = (amount_cents / 10) as u32;
                    funded = true;
                    tracing::info!(
                        "Stripe payment verified: {} cents → {} gold",
                        amount_cents,
                        reward
                    );
                }
                Err(e) => {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        format!("Stripe verification failed: {e}"),
                    ));
                }
            }
        } else {
            // No Stripe key configured — accept in test mode.
            tracing::warn!("Stripe not configured, accepting bounty in test mode");
            funded = true;
        }
    }

    let bounty_id = uuid::Uuid::new_v4();

    // Send command to Bevy.
    let cmd = GameCommand::CreateBounty {
        id: bounty_id,
        description: req.description.clone(),
        reward_gold: reward,
        objective: req.objective.clone(),
    };

    state
        .command_tx
        .send(cmd)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(CreateBountyResponse {
        id: bounty_id.to_string(),
        description: req.description,
        reward_gold: reward,
        funded,
    }))
}

// --- REST: Contract creation (multi-step bounties) ---

#[derive(Deserialize)]
struct CreateContractRequest {
    title: String,
    description: String,
    reward_gold: u32,
    ttl_ticks: u32,
    steps: Vec<super::commands::ContractStep>,
}

#[derive(Serialize)]
struct CreateContractResponse {
    id: String,
    title: String,
    reward_gold: u32,
    ttl_ticks: u32,
    step_count: usize,
}

async fn create_contract(
    State(state): State<AppState>,
    Json(req): Json<CreateContractRequest>,
) -> Result<Json<CreateContractResponse>, (StatusCode, String)> {
    let id = uuid::Uuid::new_v4();
    let step_count = req.steps.len();

    state
        .command_tx
        .send(GameCommand::CreateContract {
            id,
            title: req.title.clone(),
            description: req.description,
            reward_gold: req.reward_gold,
            ttl_ticks: req.ttl_ticks,
            steps: req.steps,
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(CreateContractResponse {
        id: id.to_string(),
        title: req.title,
        reward_gold: req.reward_gold,
        ttl_ticks: req.ttl_ticks,
        step_count,
    }))
}

async fn list_bounties() -> impl IntoResponse {
    // Placeholder — full bounty list comes from the WS state stream.
    Json(serde_json::json!({ "note": "Connect to /ws for real-time bounty state" }))
}

// --- Stripe test endpoint ---

async fn stripe_test(State(state): State<AppState>) -> impl IntoResponse {
    let configured = state.stripe_secret.is_some();
    let mode = if configured { "live" } else { "test" };
    Json(serde_json::json!({
        "configured": configured,
        "mode": mode,
    }))
}

// --- Stripe verification ---

/// Expected currency for all payments.
const EXPECTED_CURRENCY: &str = "usd";

async fn verify_stripe_payment(
    secret_key: &str,
    payment_intent_id: &str,
) -> Result<u64, String> {
    // Validate payment intent ID format.
    if !payment_intent_id.starts_with("pi_") {
        return Err("Invalid payment intent ID format".to_string());
    }

    let client = reqwest::Client::new();
    let url = format!(
        "https://api.stripe.com/v1/payment_intents/{}",
        payment_intent_id
    );

    let resp = client
        .get(&url)
        .basic_auth(secret_key, None::<&str>)
        .send()
        .await
        .map_err(|e| format!("HTTP error: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Stripe returned {status}: {body}"));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("JSON parse error: {e}"))?;

    // Validate currency.
    let currency = body["currency"].as_str().unwrap_or("");
    if currency != EXPECTED_CURRENCY {
        return Err(format!(
            "Currency mismatch: expected {EXPECTED_CURRENCY}, got {currency}"
        ));
    }

    // Check payment status.
    let status = body["status"].as_str().unwrap_or("");
    match status {
        "succeeded" => {}
        "requires_payment_method" | "requires_confirmation" | "requires_action" | "processing" => {
            return Err(format!("Payment not complete: status={status}"));
        }
        "canceled" => {
            return Err("Payment was canceled".to_string());
        }
        other => {
            return Err(format!("Unexpected payment status: {other}"));
        }
    }

    let amount = body["amount"]
        .as_u64()
        .ok_or_else(|| "Missing amount".to_string())?;

    // Check for partial refunds — use net amount.
    let amount_received = body["amount_received"].as_u64().unwrap_or(amount);
    if amount_received < amount {
        tracing::warn!(
            "Partial payment: charged={} received={} for {}",
            amount, amount_received, payment_intent_id
        );
    }

    // Check for refunds.
    let refunded = body["amount_refunded"].as_u64().unwrap_or(0);
    if refunded > 0 {
        if refunded >= amount_received {
            return Err("Payment has been fully refunded".to_string());
        }
        tracing::warn!(
            "Partial refund: received={} refunded={} for {}",
            amount_received, refunded, payment_intent_id
        );
    }

    // Return net amount after refunds.
    let net = amount_received.saturating_sub(refunded);
    if net == 0 {
        return Err("Net payment amount is zero".to_string());
    }

    Ok(net)
}
