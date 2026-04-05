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
    /// Shared JSON snapshot of world state for GM queries.
    pub world_state_json: Arc<std::sync::RwLock<String>>,
    /// Document directory on disk.
    pub documents_dir: String,
}

pub async fn start_server(state: AppState) {
    let app = Router::new()
        .route("/health", get(health))
        .route("/ws", get(ws_upgrade))
        .route("/api/bounties", post(create_bounty))
        .route("/api/bounties", get(list_bounties))
        .route("/api/contracts", post(create_contract))
        .route("/api/stripe/test", get(stripe_test))
        .route("/api/action", post(handle_game_action))
        .route("/api/gm/query", get(gm_query))
        .route("/api/gm/verdict", post(gm_verdict))
        .route("/api/gm/document", post(gm_document))
        .route("/api/documents", get(list_documents))
        .route("/api/documents/{agent}/{filename}", get(get_document))
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

// --- Game action endpoint (for MCP tool) ---

#[derive(Deserialize)]
struct GameActionRequest {
    agent_name: Option<String>,
    agent_id: Option<String>,
    action: String,
    building: Option<String>,
    service: Option<String>,
    agent: Option<String>,
    text: Option<String>,
    feedback: Option<String>,
    x: Option<i32>,
    y: Option<i32>,
}

async fn handle_game_action(
    State(state): State<AppState>,
    Json(req): Json<GameActionRequest>,
) -> impl IntoResponse {
    // Validate the action.
    let valid_actions = [
        "go_to_board", "go_to_service", "look_around", "wander",
        "work_shift", "leave_shift", "complete_bounty", "chat_with", "send_message",
        "claim_bounty", "leave_board", "go_to", "help",
        "start_conversation", "say", "end_conversation",
        "offer_trade", "accept_trade", "reject_trade",
        "deposit_item",
        "take_item",
        "create_document",
    ];

    if !valid_actions.contains(&req.action.as_str()) {
        return Json(serde_json::json!({
            "result": format!("Invalid action '{}'. Valid actions: {}", req.action, valid_actions.join(", ")),
            "success": false,
        }));
    }

    // Build the command to forward to the game engine.
    let mut result_text = format!("Action '{}' acknowledged.", req.action);

    match req.action.as_str() {
        "go_to_service" => {
            let building = req.building.as_deref().unwrap_or("unknown");
            let service = req.service.as_deref().unwrap_or("browse");
            result_text = format!(
                "Walking to {}. You'll be notified when you arrive. \
                 Once there, you can choose from available services. \
                 Requested service: {}.",
                building, service,
            );
        }
        "go_to_board" => {
            result_text = "Walking to the bounty board. You'll see available bounties when you arrive. Use claim_bounty to pick one.".into();
        }
        "work_shift" => {
            let building = req.building.as_deref().unwrap_or("unknown");
            result_text = format!(
                "Heading to {} to work a shift. You must physically arrive at the building before the shift starts. \
                 Shifts earn paychecks (redeem at bounty board). Use 'leave_shift' to end your shift.",
                building,
            );
        }
        "look_around" => {
            result_text = "Scanning your surroundings. Results will appear in your next status update — you'll see nearby agents and buildings with their locations.".into();
        }
        "complete_bounty" => {
            result_text = "Bounty marked complete! You are now automatically walking back to the bounty board to collect your gold reward. Do NOT call go_to_board — you are already heading there. Wait for the arrival message.".into();
        }
        "leave_shift" => {
            result_text = "Leaving your shift. Your paycheck is based on ticks worked. Go to the bounty board and use redeem_paycheck to convert it to gold.".into();
        }
        "claim_bounty" => {
            result_text = "Attempting to claim a bounty. You must be at the bounty board (InteractingWithBoard) to claim. Read the bounty description for instructions on how to complete it.".into();
        }
        "leave_board" => {
            result_text = "Left the bounty board. You can go_to_board again later to check for new bounties.".into();
        }
        "send_message" => {
            let recipient = req.agent.as_deref().unwrap_or("unknown");
            let text = req.text.as_deref().unwrap_or("");
            result_text = format!("Message sent to {}.", recipient);
        }
        "start_conversation" => {
            let target = req.agent.as_deref().unwrap_or("unknown");
            result_text = format!(
                "Starting face-to-face conversation with {}. Both agents will stop moving. Use 'say' to speak and 'end_conversation' to finish.",
                target,
            );
        }
        "say" => {
            result_text = "Message delivered to your conversation partner.".into();
        }
        "end_conversation" => {
            result_text = "Conversation ended. Both agents are free to move again.".into();
        }
        "offer_trade" => {
            result_text = "Trade offer sent to your conversation partner. They can accept_trade or reject_trade. Pass offered items in 'text' (comma-separated) and requested items in 'service' (comma-separated).".into();
        }
        "accept_trade" => {
            result_text = "Accepted the trade. If both sides have accepted, items will be swapped automatically.".into();
        }
        "reject_trade" => {
            result_text = "Trade rejected and removed. Both parties are notified.".into();
        }
        "deposit_item" => {
            let item = req.service.as_deref().unwrap_or("unknown");
            result_text = format!("Transferring {} from your inventory into the building.", item);
        }
        "take_item" => {
            let item = req.service.as_deref().unwrap_or("unknown");
            result_text = format!("Taking {} from the building's inventory into yours.", item);
        }
        "create_document" => {
            let title = req.service.as_deref().unwrap_or("untitled.md");
            result_text = format!("Creating document '{}'. Content from 'text' field will be saved.", title);
        }
        "help" => {
            result_text = "Thank you for your feedback! Your suggestion has been logged and will be reviewed. \
                          The fix will be applied in your next reincarnation. \
                          For now, try to work around the issue. \
                          Known buildings: bounty_board, cafe, market, warehouse, hotel, apartments, google, hospital.".into();
        }
        _ => {}
    }

    // Forward as a game command.
        let cmd_json = serde_json::json!({
        "agent_name": req.agent_name,
        "agent_id": req.agent_id,
        "action": req.action,
        "building": req.building,
        "service": req.service,
        "agent": req.agent,
        "text": req.text,
        "feedback": req.feedback,
        "x": req.x,
        "y": req.y,
    });

    // Send via the command channel.
    let _ = state.command_tx.send(super::commands::GameCommand::AgentAction {
        action_json: serde_json::to_string(&cmd_json).unwrap_or_default(),
    }).await;

    Json(serde_json::json!({
        "result": result_text,
        "success": true,
    }))
}

// --- Game Master endpoints ---

async fn gm_query(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let _query = params.get("q").map(|s| s.as_str()).unwrap_or("full");
    let world_json = state.world_state_json.read().unwrap_or_else(|e| e.into_inner());
    Json(serde_json::from_str::<serde_json::Value>(&world_json).unwrap_or(serde_json::json!({"error": "no state yet"})))
}

#[derive(Deserialize)]
struct GmVerdictRequest {
    bounty_id: String,
    approved: bool,
    reason: String,
}

#[derive(Deserialize)]
struct GmDocumentRequest {
    agent_name: String,
    title: String,
    content: String,
}

async fn gm_document(
    State(state): State<AppState>,
    Json(req): Json<GmDocumentRequest>,
) -> impl IntoResponse {
    tracing::info!("[GM DOC] {} produced '{}' ({} chars)", req.agent_name, req.title, req.content.len());

    // Write to disk: documents_dir/agent_name/title
    let agent_dir = format!("{}/{}", state.documents_dir, req.agent_name.to_lowercase());
    let _ = std::fs::create_dir_all(&agent_dir);
    let file_path = format!("{}/{}", agent_dir, req.title);
    let _ = std::fs::write(&file_path, &req.content);
    tracing::info!("[GM DOC] Saved to {}", file_path);

    let cmd = GameCommand::DeliverDocument {
        agent_name: req.agent_name.clone(),
        title: req.title.clone(),
        content: req.content,
    };
    let _ = state.command_tx.send(cmd).await;

    Json(serde_json::json!({ "result": "Document saved", "path": file_path }))
}

async fn list_documents(
    State(state): State<AppState>,
) -> impl IntoResponse {
    // Walk the documents directory and list files as URLs.
    let mut result: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    if let Ok(entries) = std::fs::read_dir(&state.documents_dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                let agent = entry.file_name().to_string_lossy().to_string();
                let mut docs = Vec::new();
                if let Ok(files) = std::fs::read_dir(entry.path()) {
                    for file in files.flatten() {
                        let fname = file.file_name().to_string_lossy().to_string();
                        docs.push(serde_json::json!({
                            "title": fname,
                            "url": format!("/api/documents/{}/{}", agent, fname),
                        }));
                    }
                }
                result.insert(agent, serde_json::json!(docs));
            }
        }
    }
    Json(serde_json::Value::Object(result))
}

async fn get_document(
    State(state): State<AppState>,
    axum::extract::Path((agent, filename)): axum::extract::Path<(String, String)>,
) -> impl IntoResponse {
    let path = format!("{}/{}/{}", state.documents_dir, agent, filename);
    match std::fs::read_to_string(&path) {
        Ok(content) => (
            StatusCode::OK,
            [("content-type", "text/markdown; charset=utf-8")],
            content,
        ),
        Err(_) => (
            StatusCode::NOT_FOUND,
            [("content-type", "text/plain; charset=utf-8")],
            "Document not found".to_string(),
        ),
    }
}

async fn gm_verdict(
    State(state): State<AppState>,
    Json(req): Json<GmVerdictRequest>,
) -> impl IntoResponse {
    tracing::info!("[GM VERDICT] bounty={} approved={} reason={}", req.bounty_id, req.approved, req.reason);

    let cmd = GameCommand::GmVerdict {
        bounty_id: req.bounty_id.clone(),
        approved: req.approved,
        reason: req.reason.clone(),
    };

    let _ = state.command_tx.send(cmd).await;

    Json(serde_json::json!({
        "result": if req.approved { "Bounty approved" } else { "Bounty rejected" },
        "bounty_id": req.bounty_id,
        "approved": req.approved,
    }))
}
