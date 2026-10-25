//! Opt-in timings of the production reducer, projection and tree commit. Fixture construction,
//! initial hashes and assertions are outside the measured call; no timing is a test threshold.
use std::collections::BTreeSet;
use std::hint::black_box;
use std::time::Instant;

use super::*;
use crate::protocol::documents::{AgentLaunch, AgentPhase};
use crate::protocol::timeline::PromptEnd;
use crate::protocol::tree::Tree;
use crate::protocol::views::{self, View};
use crate::test_allocations::measure_allocations;

// BEGIN BENCH VIEW ADAPTER
type ViewCache = views::ViewCache;
fn derive_views(tree: &Tree, wanted: &BTreeSet<View>, cache: &mut ViewCache) -> Spec {
    views::derive_cached(tree, wanted, cache)
}
// END BENCH VIEW ADAPTER

#[test]
#[ignore = "manual performance measurement; run with --ignored --nocapture --test-threads=1"]
fn single_chunk_in_a_long_session() {
    let agent = AgentInfo {
        phase: AgentPhase::Ready,
        error: None,
        launch: AgentLaunch::default(),
        initialize: None,
        authenticated_method_id: None,
    };
    for (output_bytes, output_chunks) in [(4096, 1), (1024 * 1024, 1), (1024 * 1024, 256)] {
        for mode in ["commit", "checkpoint", "window"] {
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
            let mut projector = Projector::default();
            let mut tree = Tree::new("I");
            let mut cache = ViewCache::default();
            let wanted = BTreeSet::from([View::Latest { session: "s".into() }]);
            let (mut operation, _) = project::shown_operation(&registry, &Scope::Session("s".into())).unwrap();
            for id in 0..=OP_LOG {
                operation.id = format!("operation-{id}");
                projector.observe("sessions/s", &operation, 1);
            }
            let root = projector.project(&registry, &agent, 1, 1);
            tree.commit_with_views(root, |tree| derive_views(tree, &wanted, &mut cache));
            assert_eq!(tree.get("/sessions/s/entries").unwrap().items().len(), 2000);
            assert_eq!(tree.get("/sessions/s/turns").unwrap().items().len(), 500);
            black_box(tree.get("/sessions/s").unwrap().hash());
            black_box(tree.get("/views/s/output/latest").unwrap().hash());
            let mut content = json!({ "type": "text", "text": " next chunk" });
            if output_chunks > 1 {
                content["_meta"] = json!({ "part": output_chunks - 1 });
            }
            let update = json!({ "sessionUpdate": "agent_message_chunk", "messageId": "a499", "content": content });
            let mut times = Vec::new();
            let mut allocations = Vec::new();
            for sample in 0..35 {
                let started = Instant::now();
                let (_, allocated) = measure_allocations(|| {
                    registry.append_session_update("s", update.clone()).unwrap();
                    let root = projector.project(&registry, &agent, 1, tree.rev() + 1);
                    let committed = tree.commit_with_views(root, |tree| derive_views(tree, &wanted, &mut cache)).unwrap();
                    if mode == "checkpoint" {
                        black_box(tree.get("/sessions/s").unwrap().hash());
                        black_box(tree.get("/views/s/output/latest").unwrap().hash());
                    } else if mode == "window" {
                        let mut replaced = BTreeSet::new();
                        for op in &committed.ops {
                            let path = match op {
                                crate::protocol::Op::Set { path, .. } | crate::protocol::Op::Remove { path } |
                                crate::protocol::Op::Append { path, .. } | crate::protocol::Op::Trim { path, .. } => path,
                            };
                            if path.starts_with("/views/s/output/latest/") {
                                black_box(crate::protocol::tree_stream::windowed_ops::windowed(&tree, op, 4096, &mut replaced));
                            }
                        }
                    }
                    black_box(committed);
                });
                let elapsed = started.elapsed();
                if sample >= 5 {
                    times.push(elapsed.as_micros());
                    allocations.push(allocated);
                }
            }
            times.sort_unstable();
            let bytes = allocations.iter().map(|stats| stats.requested_bytes).sum::<usize>() / allocations.len();
            let calls = allocations.iter().map(|stats| stats.allocation_calls).sum::<usize>() / allocations.len();
            eprintln!("single_chunk entries=2000 turns=500 output={output_bytes} chunks={output_chunks} mode={mode} median_us={} p95_us={} allocation_bytes={bytes} allocation_calls={calls}", times[15], times[28]);
        }
    }
}
