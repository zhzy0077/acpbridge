//! Integration tests for the real AcpClient from src/acp_client.rs
//!
//! These tests verify the actual AcpClient implementation works correctly
//! with real agent binaries.

use acpbridge::acp_client::{AcpClientState, run_client, send_prompt, AcpCommand};
use acpbridge::channel::{OutgoingMessage, OutgoingContent};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Check if an agent binary is available in PATH
fn is_agent_available(name: &str) -> bool {
    std::process::Command::new("which")
        .arg(name)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Get the correct args for an agent
fn get_agent_args(agent_name: &str) -> Vec<String> {
    if agent_name == "kimi" {
        vec!["acp".to_string()]
    } else {
        vec!["--acp".to_string()]
    }
}

/// Collect all messages from outgoing channel for a duration
async fn collect_messages(
    outgoing_rx: &mut mpsc::Receiver<OutgoingMessage>,
    duration: std::time::Duration,
) -> Vec<OutgoingMessage> {
    let mut messages = vec![];
    let deadline = tokio::time::Instant::now() + duration;
    
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(
            std::time::Duration::from_millis(100),
            outgoing_rx.recv()
        ).await {
            Ok(Some(msg)) => messages.push(msg),
            Ok(None) => break,
            Err(_) => continue,
        }
    }
    
    messages
}

/// Test AcpClient full flow with a specific agent
/// 
/// Steps verified:
/// 1. Client connects (run_client starts successfully)
/// 2. Initialize succeeds (agent sends ready message)
/// 3. Session/new creates session (returns session_id)
/// 4. Prompt "hello" sends successfully
async fn test_agent_full_flow(agent_name: &str) {
    println!("\n=== Testing AcpClient with {} ===", agent_name);

    // Create outgoing channel
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<OutgoingMessage>(100);
    
    // Create AcpClientState
    let state = Arc::new(AcpClientState::new(
        PathBuf::from("/tmp"),
        true,   // show_thinking
        false,  // show_auto_approved
        outgoing_tx,
        None,   // no MCP servers
    ));

    let connected = Arc::new(Mutex::new(false));
    let args = get_agent_args(agent_name);
    let agent_name_owned = agent_name.to_string();

    // Run client in LocalSet
    let local_set = tokio::task::LocalSet::new();
    let state_clone = Arc::clone(&state);
    let connected_clone = Arc::clone(&connected);
    
    let run_future = local_set.run_until(async move {
        let client_handle = tokio::task::spawn_local(async move {
            let result = run_client(
                agent_name_owned,
                args,
                state_clone,
                connected_clone,
            ).await;
            if let Err(e) = &result {
                println!("  run_client error: {}", e);
            }
            result
        });

        // Step 1: Wait for connection (some agents like kimi need more time)
        let max_attempts = if agent_name == "kimi" { 300 } else { 100 };
        let mut attempts = 0;
        let mut error_msgs = vec![];
        
        while !*connected.lock().await && attempts < max_attempts {
            // Check for messages
            if let Ok(msg) = outgoing_rx.try_recv() {
                match &msg.content {
                    OutgoingContent::Error(e) => {
                        println!("  {} error: {}", agent_name, e);
                        error_msgs.push(e.clone());
                    }
                    OutgoingContent::Text(t) => println!("  {} message: {}", agent_name, t),
                    _ => {}
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            attempts += 1;
        }
        
        // Check if run_client finished with an error
        let run_client_result = if client_handle.is_finished() {
            Some(client_handle.await)
        } else {
            None
        };
        
        // Log run_client status but DON'T skip - let the test continue and fail naturally
        match &run_client_result {
            Some(Ok(Ok(()))) => println!("  run_client completed successfully"),
            Some(Ok(Err(e))) => println!("  run_client returned error: {}", e),
            Some(Err(e)) => println!("  run_client panicked: {}", e),
            None => {}
        }
        
        if !*connected.lock().await {
            println!("❌ {} connection failed after {} attempts", agent_name, attempts);
            println!("  Errors received: {:?}", error_msgs);
            if let Some(Ok(Err(e))) = &run_client_result {
                println!("  run_client error: {}", e);
            }
            println!("  This indicates the agent did not complete initialization");
        }
        
        assert!(
            *connected.lock().await, 
            "❌ {} Step 1 FAILED: Client should be connected", 
            agent_name
        );
        println!("✅ {} Step 1 PASSED: Client connected", agent_name);

        // Step 2: Verify initialize response
        let messages = collect_messages(&mut outgoing_rx, std::time::Duration::from_secs(3)).await;
        
        let got_ready = messages.iter().any(|m| {
            if let OutgoingContent::Text(text) = &m.content {
                text.contains("Agent connected and ready")
            } else {
                false
            }
        });
        
        assert!(
            got_ready, 
            "❌ {} Step 2 FAILED: Should receive 'Agent connected and ready' message", 
            agent_name
        );
        println!("✅ {} Step 2 PASSED: Initialize successful, agent ready", agent_name);

        // Step 3: Create new session
        let (session_tx, session_rx) = tokio::sync::oneshot::channel();
        let new_session_cmd = AcpCommand::NewSession { response_tx: session_tx };
        
        let send_result = state.get_command_tx().send(new_session_cmd).await;
        assert!(
            send_result.is_ok(), 
            "❌ {} Step 3 FAILED: Should be able to send NewSession command", 
            agent_name
        );
        
        match tokio::time::timeout(std::time::Duration::from_secs(10), session_rx).await {
            Ok(Ok(Ok(session_id))) => {
                println!("✅ {} Step 3 PASSED: Session created: {}", agent_name, session_id);
                
                // Step 4: Send prompt "hello"
                let prompt_result = send_prompt(&state, "hello".to_string()).await;
                assert!(
                    prompt_result.is_ok(), 
                    "❌ {} Step 4 FAILED: Should be able to send prompt", 
                    agent_name
                );
                println!("✅ {} Step 4 PASSED: Prompt 'hello' sent", agent_name);
                
                // Collect response chunks
                let response_messages = collect_messages(
                    &mut outgoing_rx, 
                    std::time::Duration::from_secs(5)
                ).await;
                
                let got_chunks = response_messages.iter().any(|m| {
                    matches!(m.content, OutgoingContent::StreamChunk(_))
                });
                
                if got_chunks {
                    println!("✅ {} Step 4b PASSED: Received response chunks", agent_name);
                }
            }
            Ok(Ok(Err(e))) => {
                panic!("❌ {} Step 3 FAILED: Session creation error: {}", agent_name, e);
            }
            Ok(Err(_)) => {
                panic!("❌ {} Step 3 FAILED: Session response channel closed", agent_name);
            }
            Err(_) => {
                panic!("❌ {} Step 3 FAILED: Session creation timeout", agent_name);
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        // client_handle.abort() - can't abort after potentially awaiting it
    });
    
    tokio::time::timeout(std::time::Duration::from_secs(60), run_future)
        .await
        .expect(&format!("{} test timed out", agent_name));
    
    println!("✅ {}: ALL PASSED\n", agent_name);
}

// ============================================================================
// Tests for each agent using the unified test function
// ============================================================================

#[tokio::test]
#[ignore = "requires copilot binary"]
async fn test_acp_client_copilot() {
    if !is_agent_available("copilot") {
        println!("Skipping: copilot not found");
        return;
    }
    test_agent_full_flow("copilot").await;
}

#[tokio::test]
#[ignore = "requires gemini binary"]
async fn test_acp_client_gemini() {
    if !is_agent_available("gemini") {
        println!("Skipping: gemini not found");
        return;
    }
    test_agent_full_flow("gemini").await;
}

#[tokio::test]
#[ignore = "requires kimi binary"]
async fn test_acp_client_kimi() {
    if !is_agent_available("kimi") {
        println!("Skipping: kimi not found");
        return;
    }
    test_agent_full_flow("kimi").await;
}

// ============================================================================
// Test: AcpClientState directly
// ============================================================================

#[tokio::test]
async fn test_acp_client_state_directly() {
    use agent_client_protocol::SessionId;
    
    println!("\n=== Testing AcpClientState directly ===");

    let (outgoing_tx, _outgoing_rx) = mpsc::channel::<OutgoingMessage>(100);
    
    let state = Arc::new(AcpClientState::new(
        PathBuf::from("/tmp/test"),
        true,
        false,
        outgoing_tx,
        None,
    ));

    // Test initial state
    assert_eq!(state.get_working_dir().await, PathBuf::from("/tmp/test"));
    assert!(state.get_session_id().await.is_none());
    assert!(state.get_available_models().await.is_empty());
    assert!(state.get_available_modes().await.is_empty());
    println!("✅ Initial state correct");

    // Test updates
    state.set_session_id(Some(SessionId::new("test-session"))).await;
    state.set_available_models(vec!["gpt-4".to_string()]).await;
    state.set_active_model(Some("gpt-4".to_string())).await;
    state.set_available_modes(vec!["default".to_string()]).await;
    state.set_active_mode(Some("default".to_string())).await;
    state.set_working_dir(PathBuf::from("/new/path")).await;
    
    assert_eq!(state.get_session_id().await.unwrap().to_string(), "test-session");
    assert_eq!(state.get_available_models().await.len(), 1);
    assert_eq!(state.get_active_model().await, Some("gpt-4".to_string()));
    assert_eq!(state.get_working_dir().await, PathBuf::from("/new/path"));
    println!("✅ State updates correct");

    // Test command channel
    let _cmd_tx = state.get_command_tx();
    println!("✅ Command channel accessible");

    println!("✅ test_acp_client_state_directly: ALL PASSED\n");
}

// ============================================================================
// Test: Session Config Option Types
// ============================================================================

use acpbridge::acp_client::{SessionConfigOptionState, SessionConfigOptionChoice};

#[test]
fn test_session_config_option_types() {
    println!("\n=== Testing SessionConfigOption types ===");

    let option = SessionConfigOptionState {
        id: "model".to_string(),
        name: "Model".to_string(),
        category: Some("model".to_string()),
        current_value: Some("gpt-4".to_string()),
        choices: vec![
            SessionConfigOptionChoice {
                value: "gpt-4".to_string(),
                name: "GPT-4".to_string(),
                group: Some("OpenAI".to_string()),
            },
        ],
    };
    
    assert_eq!(option.id, "model");
    assert_eq!(option.current_value, Some("gpt-4".to_string()));
    assert_eq!(option.choices.len(), 1);
    println!("✅ SessionConfigOptionState works correctly");
    
    println!("✅ test_session_config_option_types: PASSED\n");
}

// ============================================================================
// Test: Agent Discovery
// ============================================================================

#[test]
fn test_agent_discovery() {
    println!("\n=== Agent Discovery ===");
    
    let agents = vec!["copilot", "gemini", "kimi"];
    for agent in &agents {
        if is_agent_available(agent) {
            println!("✅ {}: Available", agent);
        } else {
            println!("❌ {}: Not found", agent);
        }
    }
    
    println!("========================\n");
}

// ============================================================================
// Test: AcpCommand variants creation
// ============================================================================

#[test]
fn test_acp_command_creation() {
    println!("\n=== Testing AcpCommand creation ===");

    let (tx1, _rx1) = tokio::sync::oneshot::channel();
    let _cmd1 = AcpCommand::NewSession { response_tx: tx1 };
    println!("✅ AcpCommand::NewSession");
    
    let (tx2, _rx2) = tokio::sync::oneshot::channel();
    let _cmd2 = AcpCommand::SetModel { 
        model: "gpt-4".to_string(),
        response_tx: tx2 
    };
    println!("✅ AcpCommand::SetModel");
    
    let (tx3, _rx3) = tokio::sync::oneshot::channel();
    let _cmd3 = AcpCommand::GetModels { response_tx: tx3 };
    println!("✅ AcpCommand::GetModels");
    
    let (tx4, _rx4) = tokio::sync::oneshot::channel();
    let _cmd4 = AcpCommand::SetMode { 
        mode: "agent".to_string(),
        response_tx: tx4 
    };
    println!("✅ AcpCommand::SetMode");
    
    let (tx5, _rx5) = tokio::sync::oneshot::channel();
    let _cmd5 = AcpCommand::GetModes { response_tx: tx5 };
    println!("✅ AcpCommand::GetModes");
    
    let (tx6, _rx6) = tokio::sync::oneshot::channel();
    let _cmd6 = AcpCommand::SetConfigOption { 
        config_id: "reasoning_effort".to_string(),
        value: "high".to_string(),
        response_tx: tx6 
    };
    println!("✅ AcpCommand::SetConfigOption");
    
    let (tx7, _rx7) = tokio::sync::oneshot::channel();
    let _cmd7 = AcpCommand::GetConfigOptions { response_tx: tx7 };
    println!("✅ AcpCommand::GetConfigOptions");
    
    let (tx8, _rx8) = tokio::sync::oneshot::channel();
    let _cmd8 = AcpCommand::ChangeDirectory { 
        path: PathBuf::from("/tmp"),
        response_tx: tx8 
    };
    println!("✅ AcpCommand::ChangeDirectory");
    
    println!("✅ test_acp_command_creation: ALL PASSED\n");
}
