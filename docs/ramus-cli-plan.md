# Ramus CLI (Codex Fork) Implementation Plan

## Overview

This document outlines the modifications needed to fork OpenAI's Codex CLI into Ramus CLI, adding webhook event emission capabilities for integration with the GoalGPT/Ramus application. The goal is to enable real-time progress tracking and state management for autonomous agents.

## Core Requirements

### Event Emission System
- Emit plan updates via webhooks with HMAC-SHA256 signatures
- Write plan state to local files (plan.json, plan-events.jsonl)
- Optional stdout streaming with `@plan` prefix
- Idempotent event delivery with sequence numbers
- Retry logic with jittered exponential backoff for webhook failures
- Persist sequence numbers across restarts

### CLI Flags (New)
- `--plan-webhook <URL>` - Webhook endpoint for plan updates
- `--webhook-secret <SECRET>` - HMAC signing secret  
- `--plan-events <PATH>` - Path for event log file (JSONL format)
- `--plan-state <PATH>` - Path for current plan state (JSON)
- `--task-id <ID>` - Unique task identifier
- `--run-id <ID>` - Unique run identifier
- `--emit-plan-stdout` - Enable plan events to stdout

## Implementation Changes

### Note on Reqwest Timeouts
With reqwest 0.12, use the following pattern for timeouts:
```rust
Client::builder()
    .connect_timeout(Duration::from_secs(1))
    .timeout(Duration::from_secs(2))  // Total request timeout
    .build()?
```

### 1. Dependencies to Add (codex-rs/exec/Cargo.toml)

```toml
reqwest = { version = "0.12", features = ["json", "rustls-tls"] }
hmac = "0.12"
sha2 = "0.10"
backoff = { version = "0.4", features = ["tokio"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
time = { version = "0.3", features = ["formatting", "parsing", "serde"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
```

### 2. Event Emitter Module (New File: codex-rs/exec/src/plan_emitter.rs)

Create a new module to handle all event emission logic:

```rust
// Key components needed:
- PlanEmitter struct with webhook URL, secret, file paths
- emit_plan_update() method for EventMsg::PlanUpdate  
- compute_hmac() for webhook signatures (sign canonical string: "v0:{timestamp}:{body}")
- retry_webhook() with full jittered exponential backoff (200ms, 500ms, 1s max, 3 retries)
- write_event_log() for JSONL append (single write_all + \n)
- write_plan_state() for atomic state snapshot (write to .tmp, fsync, rename, fsync parent dir)
- persist_sequence() to save last_seq to plan.meta.json
- load_sequence() to restore seq on restart
```

### 3. CLI Argument Extensions (codex-rs/exec/src/cli.rs)

Add new command-line arguments to the exec subcommand:

```rust
#[derive(Parser)]
struct ExecArgs {
    // ... existing args ...
    
    #[arg(long, env = "RAMUS_WEBHOOK_URL", value_parser = validate_url)]
    plan_webhook: Option<String>,
    
    #[arg(long, env = "RAMUS_WEBHOOK_SECRET")]
    webhook_secret: Option<String>,
    
    #[arg(long)]
    plan_events: Option<PathBuf>,
    
    #[arg(long)]
    plan_state: Option<PathBuf>,
    
    #[arg(long)]
    task_id: Option<String>,
    
    #[arg(long)]
    run_id: Option<String>,
    
    #[arg(long)]
    emit_plan_stdout: bool,
}

fn validate_url(s: &str) -> Result<String, String> {
    let url = url::Url::parse(s).map_err(|e| e.to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("URL must use http or https scheme".to_string());
    }
    Ok(s.to_string())
}
```

### 4. Event Loop Integration (codex-rs/exec/src/exec.rs)

Modify the main event loop to emit events:

```rust
// On startup:
let mut sequence_number = if let Some(emitter) = &plan_emitter {
    emitter.load_sequence().await.unwrap_or(1)
} else {
    1
};

// Create directories if needed
if let Some(path) = &args.plan_events {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
}

// In the event processing loop:
match event {
    EventMsg::PlanUpdate(plan) => {
        // Existing plan update logic...
        
        // NEW: Emit to configured destinations
        if let Some(emitter) = &plan_emitter {
            emitter.emit_plan_update(&plan, sequence_number).await?;
            emitter.persist_sequence(sequence_number).await?;
            sequence_number += 1;
        }
    }
    // ... other event handlers ...
}

// On SIGINT/SIGTERM:
if let Some(emitter) = &plan_emitter {
    emitter.emit_shutdown(sequence_number).await?;
}
```

### 5. Event Schemas (Verified from Codex Source)

**✅ VERIFIED**: Schema matches exactly with `UpdatePlanArgs` from `codex-rs/core/src/plan_tool.rs` and the `EventMsg::PlanUpdate` variant in `codex-rs/core/src/protocol.rs`.

#### Core Schema Components (from source):
```rust
// From plan_tool.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanItemArg {
    pub step: String,
    pub status: StepStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdatePlanArgs {
    #[serde(default)]
    pub explanation: Option<String>,
    pub plan: Vec<PlanItemArg>,
}
```

#### Webhook Event (JSON)
```json
{
  "event": "plan_update",
  "run_id": "run-123",
  "task_id": "task-456",
  "seq": 7,
  "ts": "2025-08-14T16:12:03Z",
  "plan": {
    "explanation": "Starting outreach phase",
    "plan": [
      {
        "step": "Define product vision",
        "status": "completed"
      },
      {
        "step": "Map personas",
        "status": "in_progress"
      },
      {
        "step": "Outline MVP",
        "status": "pending"
      }
    ]
  },
  "meta": {
    "model": "gpt-4",
    "codex_ver": "1.0.0"
  }
}
```

#### Webhook Headers
```
POST /plan-update
Content-Type: application/json
X-Run-Id: run-123
X-Task-Id: task-456
X-Seq: 7
X-Timestamp: 2025-08-14T16:12:03Z
X-Signature: sha256=<hex(HMAC_SHA256(secret, "v0:{timestamp}:{body}"))>
```

### 6. File Outputs

#### plan.json (Current State)
- Atomically overwritten on each update (write to .tmp, fsync, rename)
- Contains latest plan state matching webhook schema
- Used for recovery/resume

#### plan-events.jsonl (Event Log)
- Append-only log (single write_all + \n per event)
- One JSON object per line
- Full audit trail of plan evolution

#### plan.meta.json (Metadata)
- Persists `last_seq` for sequence number recovery
- Updated atomically alongside plan.json
- Example: `{"last_seq": 42}`

### 7. Error Handling & Resilience

- Webhook failures should not crash the CLI
- Reqwest client with timeouts: connect=1s, timeout=2s (total request)
- Jittered exponential backoff: 200ms, 500ms, 1s (max 3 retries)
- After failures, log single line to stderr and continue
- No panics or cascading failures
- Validate timestamp within 300s window on receiver side (configurable, 120s for tight sync)

### 8. Testing Requirements

#### Unit Tests
- HMAC signature generation using hmac::Mac::verify_slice() (constant-time)
- Event serialization with exact schema
- Atomic file writing logic
- Jittered backoff calculations
- Sequence number persistence/recovery

#### Integration Tests
- Mock webhook server using wiremock-rs with timestamp validation
- Verify HMAC signatures on raw body
- Test idempotency with duplicate sequence numbers
- File output validation (atomic writes)
- Unicode/long steps handling (no truncation)
- Duplicate event coalescing

#### Contract Tests
- Ensure event schema matches UpdatePlanArgs exactly (verify from source)
- Validate webhook headers including X-Timestamp
- Test with Python runner service using wiremock-rs
- Verify runner rejects stale timestamps (>300s default)
- Test canonical string HMAC: "v0:{timestamp}:{body}"

## Migration Path

### Phase 1: Core Implementation (2-3 hours)
1. Add dependencies to Cargo.toml
2. Create plan_emitter module
3. Add CLI arguments
4. Wire into event loop
5. Basic testing

### Phase 2: Resilience (1 hour)
1. Implement retry logic
2. Add circuit breaker
3. Error logging
4. Load testing

### Phase 3: Production Hardening (1 hour)
1. Performance optimization
2. Memory usage profiling
3. Documentation
4. CI/CD integration

## Configuration Examples

### Basic Usage
```bash
codex exec --plan-webhook http://localhost:8000/plan-update \
           --webhook-secret my-secret \
           --task-id task-123 \
           --run-id run-456
```

### Full Configuration
```bash
codex exec --plan-webhook https://api.goalgpt.com/webhooks/plan \
           --webhook-secret $WEBHOOK_SECRET \
           --plan-events /srv/run/task-123/events.jsonl \
           --plan-state /srv/run/task-123/plan.json \
           --task-id task-123 \
           --run-id run-456 \
           --emit-plan-stdout
```

### Environment Variables
```bash
export RAMUS_WEBHOOK_URL=http://localhost:8000/plan-update
export RAMUS_WEBHOOK_SECRET=secret-key-here
codex exec --task-id task-123
```

## Security Considerations

1. **Secret Management**
   - Never log webhook secrets
   - Use environment variables for production
   - Rotate secrets regularly

2. **HMAC Validation**
   - Sign canonical string: `HMAC_SHA256(secret, "v0:{timestamp}:{body}")`
   - Use hmac::Mac::verify_slice() for constant-time comparison
   - Include timestamp validation (±300s window default, configurable to 120s)
   - Reject requests with invalid signatures or stale timestamps

3. **Network Security**
   - Use HTTPS in production (rustls-tls, no OpenSSL dependency)
   - Implement request timeouts (connect=1s, timeout=2s for total request)
   - Validate webhook URLs with url::Url::parse in clap value_parser

4. **Data Privacy**
   - Mirror built-in tool schema exactly (no extra data)
   - Don't include file contents in events
   - Minimize data in webhooks

## Backwards Compatibility

- All new features are opt-in via CLI flags
- No changes to default behavior
- Existing workflows unaffected
- Can be merged upstream without conflicts

## Success Criteria

✅ Webhooks emit on every plan update with exact UpdatePlanArgs schema
✅ HMAC signatures validate correctly on raw body
✅ Files written atomically (.tmp → rename)
✅ Idempotency via sequence numbers (persisted across restarts)
✅ Resilient to network failures (bounded retries, no crashes)
✅ No performance regression
✅ Works with Python runner service
✅ Contract tests pass (including unicode & duplicate seq)
✅ Timestamp validation within 300s window (configurable)
✅ Graceful shutdown with final event on SIGINT/SIGTERM

## Quick Win Checklist

- [x] **Verify exact UpdatePlanArgs schema from Codex source**
- [ ] Change flag to `--emit-plan-stdout`
- [ ] Add `X-Timestamp` header and 300s window validation (configurable)
- [ ] Sign canonical string "v0:{timestamp}:{body}" for HMAC
- [ ] Persist `last_seq` in plan.meta.json
- [ ] Atomic writes (.tmp → fsync → rename → fsync parent dir)
- [ ] Full jittered backoff (200ms, 500ms, 1s max, 3 retries)
- [ ] Single stderr line on webhook failure (use tracing)
- [ ] Use reqwest 0.12 with rustls-tls
- [ ] Use hmac::Mac::verify_slice() for constant-time comparison
- [ ] Add tests: unicode steps + duplicate seq + wiremock-rs
- [ ] Create directories with fs::create_dir_all if missing
- [ ] Validate URL with url::Url::parse in clap value_parser
- [ ] Handle SIGINT/SIGTERM gracefully (tokio::signal)

## Future Enhancements

- Batch event delivery for efficiency
- Compression for large plans
- Custom event types beyond plan updates
- WebSocket support for bidirectional communication
- Metrics and telemetry emission
- Plugin system for custom emitters

## References

- [GoalGPT Architecture Document](../goalpt-architecture.md)
- [OpenAI Codex CLI Documentation](https://github.com/openai/codex)
- [HMAC RFC 2104](https://tools.ietf.org/html/rfc2104)
- [JSON Lines Format](https://jsonlines.org/)