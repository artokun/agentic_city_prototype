use std::collections::HashMap;
use std::process::Stdio;
use tokio::process::Command;
use tokio::sync::mpsc;

/// Send a single prompt to Claude and get a response.
/// Uses --print mode: one-shot request/response, no persistent process.
pub async fn ask_claude(prompt: &str, system_prompt: &str) -> Result<String, String> {
    let mut env: HashMap<String, String> = std::env::vars().collect();
    env.remove("ANTHROPIC_API_KEY");

    let output = Command::new("claude")
        .args([
            "--print",
            "--model", "haiku",
            "--output-format", "text",
            "--append-system-prompt", system_prompt,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .envs(&env)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Failed to spawn claude: {e}"))?;

    // Write prompt to stdin.
    let mut child = output;
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let _ = stdin.write_all(prompt.as_bytes()).await;
        drop(stdin); // Close stdin to signal end of input.
    }

    let output = child.wait_with_output().await
        .map_err(|e| format!("Claude process error: {e}"))?;

    if !output.status.success() {
        return Err(format!("Claude exited with {}", output.status));
    }

    String::from_utf8(output.stdout)
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("UTF-8 error: {e}"))
}
