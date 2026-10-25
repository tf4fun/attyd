//! Opt-in measurements of the complete production publication path. The reducer, fixture
//! construction, assertions and subscriber consumption run outside the measured commit.
use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant as Clock;

use serde_json::json;
use tokio::time::Instant;

use super::Feed;
use crate::protocol::documents::{AgentInfo, AgentLaunch, AgentPhase};
use crate::protocol::timeline::PromptEnd;
use crate::protocol::tree_stream::params::parse_stream_query;
use crate::protocol::tree_stream::wire::Change;
use crate::session_registry::SessionRegistry;
use crate::test_allocations::measure_allocations;

const OUTPUT: &str = "/views/s/output/latest";

// BEGIN BENCH FEED ADAPTER
fn new_feed() -> Feed {
    Feed::new()
}
// END BENCH FEED ADAPTER

fn registry(output_bytes: usize, output_chunks: usize) -> SessionRegistry {
    let mut registry = SessionRegistry::new("epoch");
    registry.open_new("epoch", "s", "/work", json!({ "sessionId": "s" })).unwrap();
    for turn in 0..500 {
        let timeline = Arc::make_mut(registry.sessions.get_mut("s").unwrap().state.timeline.as_mut().unwrap());
        timeline.begin_prompt(&[json!({ "type": "text", "text": "question" })]);
        for update in [
            json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": "thinking" } }),
            json!({ "sessionUpdate": "tool_call", "toolCallId": format!("t{turn}"), "title": "tool" }),
        ] {
            timeline.reduce(&update, None);
        }
        let chunks = if turn == 499 { output_chunks } else { 1 };
        for part in 0..chunks {
            let mut content = json!({ "type": "text", "text": "x".repeat(if turn == 499 { output_bytes / chunks } else { 4096 }) });
            if chunks > 1 {
                content["_meta"] = json!({ "part": part });
            }
            timeline.reduce(&json!({ "sessionUpdate": "agent_message_chunk", "messageId": format!("a{turn}"), "content": content }), None);
        }
        if turn != 499 {
            timeline.end_prompt(PromptEnd::Response(json!({ "stopReason": "end_turn" })));
        }
    }
    registry
}

#[test]
#[ignore = "CI performance evidence; run with --ignored --nocapture --test-threads=1"]
fn feed_single_chunk_in_a_long_session() {
    let agent = AgentInfo {
        phase: AgentPhase::Ready,
        error: None,
        launch: AgentLaunch::default(),
        initialize: None,
        authenticated_method_id: None,
    };
    for (output_bytes, output_chunks) in [(4096, 1), (1024 * 1024, 1), (1024 * 1024, 256)] {
        for mode in ["unfollowed", "window"] {
            let mut registry = registry(output_bytes, output_chunks);
            let mut feed = new_feed();
            let now = Instant::now();
            feed.commit_ending(&registry, &agent, 1, now, &[]);
            assert_eq!(feed.tree().get("/sessions/s/entries").unwrap().items().len(), 2000);
            assert_eq!(feed.tree().get("/sessions/s/turns").unwrap().items().len(), 500);
            let subscription = if mode == "window" {
                let query = parse_stream_query(&format!("follow={OUTPUT}&window={OUTPUT}:4096")).unwrap();
                let id = feed.subscribe_tree(&query);
                let first = feed.next_tree(id).expect("the output window is aligned");
                assert!(first.changes.as_ref().unwrap().iter().any(|change| matches!(change,
                    Change::Snapshot { path, .. } if path == OUTPUT)));
                assert!(feed.next_tree(id).is_none());
                Some(id)
            } else {
                None
            };
            let suffix = " next chunk";
            let mut content = json!({ "type": "text", "text": suffix });
            if output_chunks > 1 {
                content["_meta"] = json!({ "part": output_chunks - 1 });
            }
            let update = json!({ "sessionUpdate": "agent_message_chunk", "messageId": "a499", "content": content });
            let mut times = Vec::with_capacity(30);
            let mut allocations = Vec::with_capacity(30);
            for sample in 0..35 {
                // Only publication is measured: the incoming update already changed the
                // authoritative timeline before the bridge calls commit_ending.
                registry.append_session_update("s", update.clone()).unwrap();
                let (elapsed, allocated) = measure_allocations(|| {
                    let started = Clock::now();
                    feed.commit_ending(&registry, &agent, 1, now, &[]);
                    started.elapsed()
                });
                if sample >= 5 {
                    times.push(elapsed.as_micros());
                    allocations.push(allocated);
                }
                // Consume between commits, so the window case represents a caught-up
                // subscriber rather than an accumulating queue or a coalescing benchmark.
                if let Some(id) = subscription {
                    let event = feed.next_tree(id).expect("the appended output is queued");
                    assert!(event.changes.as_ref().unwrap().iter().any(|change| matches!(change,
                        Change::Ops { path, .. } if path == OUTPUT)));
                    black_box(event);
                    assert!(feed.next_tree(id).is_none());
                    let (output, _) = feed.tree().get(OUTPUT).unwrap().as_value().unwrap();
                    assert_eq!(output["text"].as_str().unwrap().len(), output_bytes + (sample + 1) * suffix.len());
                }
                let (entry, _) = feed.tree().get("/sessions/s/entries/1999").unwrap().as_value().unwrap();
                assert!(entry["chunks"].as_array().unwrap().last().unwrap()["content"]["text"].as_str().unwrap().ends_with(suffix));
            }
            times.sort_unstable();
            let bytes = allocations.iter().map(|stats| stats.requested_bytes).sum::<usize>() / allocations.len();
            let calls = allocations.iter().map(|stats| stats.allocation_calls).sum::<usize>() / allocations.len();
            eprintln!("feed_single_chunk entries=2000 turns=500 output={output_bytes} chunks={output_chunks} mode={mode} median_us={} p95_us={} allocation_bytes={bytes} allocation_calls={calls}", times[15], times[28]);
        }
    }
}
