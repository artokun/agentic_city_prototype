//! Central game configuration. All tunable constants in one place.
//! Override any value via environment variable of the same name.

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

// --- Tick rate ---
/// Milliseconds per game tick. 1000 = 1Hz, 500 = 2Hz.
pub fn tick_ms() -> u64 {
    env_or("TICK_MS", 1000)
}

// --- Context / Energy ---
/// Token budget before energy hits 0. Models have 1M, we cap lower.
pub fn context_limit() -> u32 {
    env_or("CONTEXT_LIMIT", 50_000)
}
/// Token budget for the persistent System AI before `/compact`.
pub fn system_ai_compact_limit() -> u32 {
    env_or("SYSTEM_AI_COMPACT_LIMIT", 15_000)
}

// --- Needs decay (per tick) ---
/// Hunger decay per tick. 5min to drain at 1Hz (100/300 ticks).
pub fn hunger_decay() -> f32 {
    env_or("HUNGER_DECAY", 0.333)
}
/// Boredom decay per tick when idle/wandering.
pub fn boredom_decay_idle() -> f32 {
    env_or("BOREDOM_DECAY_IDLE", 0.05)
}
/// Boredom recovery per tick when actively working.
pub fn boredom_recovery_active() -> f32 {
    env_or("BOREDOM_RECOVERY_ACTIVE", 0.02)
}

// --- AI context ---
/// Ticks between sending context updates to agents. 50 = every 25s at 2Hz.
pub fn context_interval() -> u64 {
    env_or("CONTEXT_INTERVAL", 50)
}
/// Ticks between periodic status updates to agents.
pub fn status_interval() -> u64 {
    env_or("STATUS_INTERVAL", 30)
}

// --- Needs alerts ---
/// Need level below which we alert the agent.
pub fn need_alert_threshold() -> f32 {
    env_or("NEED_ALERT_THRESHOLD", 30.0)
}
/// Ticks cooldown between re-sending the same need alert.
pub fn need_alert_cooldown() -> u64 {
    env_or("NEED_ALERT_COOLDOWN", 200)
}
/// Need level below which the behavior system auto-handles (emergency).
pub fn critical_threshold() -> f32 {
    env_or("CRITICAL_THRESHOLD", 10.0)
}

// --- Hospital ---
/// Gold fee for hospital recovery (can cause debt).
pub fn hospital_fee() -> u32 {
    env_or("HOSPITAL_FEE", 5)
}
/// Gold reward for rescuing a passed-out agent.
pub fn rescue_reward() -> u32 {
    env_or("RESCUE_REWARD", 5)
}
/// Ticks to recover in the hospital.
pub fn recovery_ticks() -> u32 {
    env_or("RECOVERY_TICKS", 300)
}

// --- Economy ---
/// Gold penalty for recycling an expired bounty.
pub fn recycle_cost() -> u32 {
    env_or("RECYCLE_COST", 1)
}

// --- Chat ---
/// Boredom boost per chat message (for both speaker and listener).
pub fn chat_boredom_boost() -> f32 {
    env_or("CHAT_BOREDOM_BOOST", 5.0)
}

// --- LLM sessions ---
/// Base directory for durable session state (checkpoints and event logs).
pub fn runtime_dir() -> String {
    env_or("RUNTIME_DIR", "runtime/llm-sessions".to_string())
}
