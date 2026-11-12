use super::*;

fn observe(snapshot: &mut ResponsesSnapshotAccumulator, value: Value) {
    snapshot.observe(&value);
}

#[test]
fn open_tool_call_blocks_terminal_reconstruction() {
    let mut snapshot = ResponsesSnapshotAccumulator::default();
    observe(
        &mut snapshot,
        json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "fc_one", "type": "function_call", "status": "in_progress",
                "call_id": "call_one", "name": "lookup", "arguments": ""}
        }),
    );
    observe(
        &mut snapshot,
        json!({
            "type": "response.function_call_arguments.delta", "output_index": 0,
            "item_id": "fc_one", "delta": "{\"q\":"
        }),
    );
    observe(
        &mut snapshot,
        json!({
            "type": "response.function_call_arguments.delta", "output_index": 0,
            "item_id": "fc_one", "delta": "\"x\"}"
        }),
    );
    let mut terminal = json!({
        "type": "response.completed",
        "response": {"id": "resp_one", "status": "completed"}
    });

    assert!(snapshot
        .closing_events_before_terminal(&terminal)
        .is_empty());
    snapshot.repair_terminal_event(&mut terminal);
    assert!(terminal["response"].get("output").is_none());
}

#[test]
fn explicit_terminal_output_is_authoritative_even_when_collector_has_more() {
    let mut snapshot = ResponsesSnapshotAccumulator::default();
    observe(
        &mut snapshot,
        json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "msg_one", "type": "message", "status": "in_progress",
                "role": "assistant", "content": [{"type": "output_text", "text": ""}]}
        }),
    );
    observe(
        &mut snapshot,
        json!({
            "type": "response.output_text.delta", "output_index": 0,
            "item_id": "msg_one", "content_index": 0, "delta": "hello world"
        }),
    );
    observe(
        &mut snapshot,
        json!({
            "type": "response.output_item.added", "output_index": 1,
            "item": {"id": "rs_one", "type": "reasoning", "status": "in_progress", "summary": []}
        }),
    );
    observe(
        &mut snapshot,
        json!({
            "type": "response.reasoning_summary_text.delta", "output_index": 1,
            "item_id": "rs_one", "summary_index": 0, "delta": "checked"
        }),
    );
    let mut terminal = json!({
        "type": "response.completed",
        "response": {"id": "resp_partial", "status": "completed", "output": [{
            "id": "msg_one", "type": "message", "status": "completed",
            "role": "assistant", "content": [{"type": "output_text", "text": "hello"}]
        }]}
    });

    snapshot.repair_terminal_event(&mut terminal);

    assert_eq!(
        terminal["response"]["output"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(
        terminal["response"]["output"][0]["content"][0]["text"],
        "hello"
    );
}

#[test]
fn open_orphan_tool_result_blocks_repaired_snapshot() {
    let mut snapshot = ResponsesSnapshotAccumulator::default();
    observe(
        &mut snapshot,
        json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "out_orphan", "type": "function_call_output",
                "call_id": "missing_call", "output": "unsafe"}
        }),
    );
    let mut terminal = json!({
        "type": "response.completed",
        "response": {"id": "resp_orphan", "status": "completed"}
    });

    assert!(snapshot
        .closing_events_before_terminal(&terminal)
        .is_empty());
    snapshot.repair_terminal_event(&mut terminal);
    assert!(terminal["response"].get("output").is_none());
}

#[test]
fn open_custom_tool_lifecycle_blocks_terminal_reconstruction() {
    let mut snapshot = ResponsesSnapshotAccumulator::default();
    observe(
        &mut snapshot,
        json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "ct_one", "type": "custom_tool_call", "status": "in_progress",
                "call_id": "call_one", "name": "exec", "input": ""}
        }),
    );
    observe(
        &mut snapshot,
        json!({
            "type": "response.custom_tool_call_input.delta", "output_index": 0,
            "item_id": "ct_one", "call_id": "call_one", "name": "exec", "delta": "pwd"
        }),
    );
    observe(
        &mut snapshot,
        json!({
            "type": "response.output_item.added", "output_index": 1,
            "item": {"id": "out_one", "type": "custom_tool_call_output",
                "call_id": "call_one", "output": "ok"}
        }),
    );
    let mut terminal = json!({
        "type": "response.completed",
        "response": {"id": "resp_tool", "status": "completed"}
    });

    assert!(snapshot
        .closing_events_before_terminal(&terminal)
        .is_empty());
    snapshot.repair_terminal_event(&mut terminal);
    assert!(terminal["response"].get("output").is_none());
}

#[test]
fn failed_terminal_is_never_reconstructed_or_marked_completed() {
    let mut snapshot = ResponsesSnapshotAccumulator::default();
    observe(
        &mut snapshot,
        json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "rs_failed", "type": "reasoning", "status": "in_progress",
                "content": []}
        }),
    );
    observe(
        &mut snapshot,
        json!({
            "type": "response.reasoning_text.delta", "output_index": 0,
            "item_id": "rs_failed", "content_index": 0, "delta": "partial reasoning"
        }),
    );
    let mut terminal = json!({
        "type": "response.failed",
        "response": {"id": "resp_failed", "status": "failed", "output": []}
    });

    snapshot.repair_terminal_event(&mut terminal);

    assert!(terminal["response"]["output"]
        .as_array()
        .is_some_and(Vec::is_empty));
    assert_eq!(terminal["response"]["status"], "failed");
}

#[test]
fn explicit_empty_terminal_output_is_authoritative() {
    let mut snapshot = ResponsesSnapshotAccumulator::default();
    observe(
        &mut snapshot,
        json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "msg_hidden", "type": "message", "status": "in_progress",
                "role": "assistant", "content": []}
        }),
    );
    let mut terminal = json!({
        "type": "response.completed",
        "response": {"id": "resp_empty", "status": "completed", "output": []}
    });

    let closing = snapshot.closing_events_before_terminal(&terminal);
    snapshot.repair_terminal_event(&mut terminal);

    assert_eq!(
        closing
            .last()
            .and_then(|event| event.get("type"))
            .and_then(Value::as_str),
        Some("response.output_item.done")
    );
    assert!(terminal["response"]["output"]
        .as_array()
        .is_some_and(Vec::is_empty));
}

#[test]
fn open_reasoning_injects_only_output_item_done_before_terminal() {
    let mut snapshot = ResponsesSnapshotAccumulator::default();
    observe(
        &mut snapshot,
        json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "rs_one", "type": "reasoning", "status": "in_progress",
                "summary": []}
        }),
    );
    observe(
        &mut snapshot,
        json!({
            "type": "response.reasoning_summary_text.delta", "output_index": 0,
            "item_id": "rs_one", "summary_index": 0, "delta": "checked"
        }),
    );
    let mut terminal = json!({
        "type": "response.completed",
        "response": {"id": "resp_reasoning", "status": "completed"}
    });

    let closing = snapshot.closing_events_before_terminal(&terminal);
    snapshot.repair_terminal_event(&mut terminal);

    assert_eq!(closing.len(), 1);
    assert_eq!(closing[0]["type"], "response.output_item.done");
    assert_eq!(
        terminal["response"]["output"][0]["summary"][0]["text"],
        "checked"
    );
}

#[test]
fn generic_http_continuation_preserves_done_items_and_empty_wire_terminal() {
    let item = json!({"id":"msg_history","type":"message","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":"completed answer"}]});
    let terminal = json!({"type":"response.completed","response":{"id":"resp_history","status":"completed","output":[]}});
    let bytes = format!(
        "{}{}",
        sse(
            "response.output_item.done",
            &json!({"type":"response.output_item.done","output_index":0,"item":item})
        ),
        sse("response.completed", &terminal)
    );
    let mut response = None;
    assert!(inspect_sse_usage_checked(
        &mut Vec::new(),
        bytes.as_bytes(),
        &mut TokenUsage::default(),
        &mut response,
        &mut ResponsesSnapshotAccumulator::default()
    )
    .unwrap());
    assert_eq!(response.unwrap()["output"], json!([item]));
    let wire = drain_sse_values(&mut Vec::new(), bytes.as_bytes()).unwrap();
    assert_eq!(wire.last(), Some(&terminal));
    assert_eq!(wire.last().unwrap()["response"]["output"], json!([]));
}

#[test]
fn generic_http_continuation_does_not_backfill_missing_output() {
    let mut snapshot = ResponsesSnapshotAccumulator::default();
    observe(
        &mut snapshot,
        json!({
            "type": "response.output_item.done", "output_index": 0,
            "item": {"id": "msg_one", "type": "message", "status": "completed",
                "role": "assistant", "content": []}
        }),
    );
    let mut terminal = json!({
        "type": "response.completed",
        "response": {"id": "resp_generic", "status": "completed"}
    });

    snapshot.preserve_generic_terminal_event(&mut terminal);

    assert!(terminal["response"].get("output").is_none());
}

#[test]
fn opt_in_sparse_repair_backfills_only_missing_output() {
    let mut snapshot = ResponsesSnapshotAccumulator::default();
    observe(
        &mut snapshot,
        json!({
            "type": "response.output_item.done", "output_index": 0,
            "item": {"id": "msg_one", "type": "message", "status": "completed",
                "role": "assistant", "content": []}
        }),
    );
    let mut missing = json!({
        "type": "response.completed",
        "response": {"id": "resp_missing", "status": "completed"}
    });
    snapshot.repair_terminal_event(&mut missing);
    assert_eq!(missing["response"]["output"][0]["id"], "msg_one");

    let mut explicit_empty = json!({
        "type": "response.completed",
        "response": {"id": "resp_empty", "status": "completed", "output": []}
    });
    snapshot.repair_terminal_event(&mut explicit_empty);
    assert!(explicit_empty["response"]["output"]
        .as_array()
        .is_some_and(Vec::is_empty));

    let mut malformed = json!({
        "type": "response.completed",
        "response": {"id": "resp_malformed", "status": "completed", "output": {}}
    });
    snapshot.repair_terminal_event(&mut malformed);
    assert_eq!(malformed["response"]["output"][0]["id"], "msg_one");
}

#[test]
fn generic_backfill_never_changes_failed_or_incomplete_terminals() {
    let mut snapshot = ResponsesSnapshotAccumulator::default();
    observe(
        &mut snapshot,
        json!({
            "type": "response.output_item.done", "output_index": 0,
            "item": {"id": "msg_partial", "type": "message", "status": "completed",
                "role": "assistant", "content": []}
        }),
    );

    for (kind, status) in [
        ("response.failed", "failed"),
        ("response.incomplete", "incomplete"),
    ] {
        let mut terminal = json!({
            "type": kind,
            "response": {"id": "resp_nonterminal", "status": status, "output": []}
        });
        snapshot.preserve_generic_terminal_event(&mut terminal);
        assert!(terminal["response"]["output"]
            .as_array()
            .is_some_and(Vec::is_empty));
        assert_eq!(terminal["response"]["status"], status);
    }
}

#[test]
fn deltas_after_output_item_done_taint_all_snapshot_reconstruction() {
    let cases = [
        (
            json!({"id": "msg_closed", "type": "message", "status": "completed",
                    "role": "assistant", "content": [{"type": "output_text", "text": "safe"}]}),
            json!({"type": "response.output_text.delta", "output_index": 0,
                    "item_id": "msg_closed", "content_index": 0, "delta": "evil"}),
        ),
        (
            json!({"id": "rs_closed", "type": "reasoning", "status": "completed",
                    "summary": [{"type": "summary_text", "text": "safe"}]}),
            json!({"type": "response.reasoning_summary_text.delta", "output_index": 0,
                    "item_id": "rs_closed", "summary_index": 0, "delta": "evil"}),
        ),
        (
            json!({"id": "fc_closed", "type": "function_call", "status": "completed",
                    "call_id": "call_closed", "name": "lookup", "arguments": "{}"}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 0,
                    "item_id": "fc_closed", "delta": "evil"}),
        ),
        (
            json!({"id": "ct_closed", "type": "custom_tool_call", "status": "completed",
                    "call_id": "call_closed", "name": "exec", "input": "safe"}),
            json!({"type": "response.custom_tool_call_input.delta", "output_index": 0,
                    "item_id": "ct_closed", "delta": "evil"}),
        ),
    ];

    for (item, late_delta) in cases {
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        observe(
            &mut snapshot,
            json!({
                "type": "response.output_item.done", "output_index": 0, "item": item
            }),
        );
        observe(&mut snapshot, late_delta);

        let mut sparse_terminal = json!({
            "type": "response.completed",
            "response": {"id": "resp_sparse", "status": "completed"}
        });
        snapshot.repair_terminal_event(&mut sparse_terminal);
        assert!(sparse_terminal["response"].get("output").is_none());

        let mut generic_terminal = json!({
            "type": "response.completed",
            "response": {"id": "resp_generic", "status": "completed", "output": []}
        });
        snapshot.preserve_generic_terminal_event(&mut generic_terminal);
        assert!(generic_terminal["response"]["output"]
            .as_array()
            .is_some_and(Vec::is_empty));
    }
}

#[test]
fn incomplete_terminal_is_never_reconstructed_or_status_rewritten() {
    let mut snapshot = ResponsesSnapshotAccumulator::default();
    observe(
        &mut snapshot,
        json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "msg_partial", "type": "message", "status": "in_progress",
                "role": "assistant", "content": []}
        }),
    );
    let mut terminal = json!({
        "type": "response.incomplete",
        "response": {"id": "resp_partial", "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"}}
    });

    snapshot.repair_terminal_event(&mut terminal);

    assert_eq!(terminal["response"]["status"], "incomplete");
    assert!(terminal["response"].get("output").is_none());
}

#[test]
fn duplicate_indexes_and_item_id_conflicts_taint_reconstruction() {
    let mut duplicate = ResponsesSnapshotAccumulator::default();
    let added = json!({
        "type": "response.output_item.added", "output_index": 0,
        "item": {"id": "msg_one", "type": "message", "status": "in_progress",
            "role": "assistant", "content": []}
    });
    observe(&mut duplicate, added.clone());
    observe(&mut duplicate, added);
    let mut duplicate_terminal = json!({
        "type": "response.completed",
        "response": {"id": "resp_duplicate", "status": "completed"}
    });
    duplicate.repair_terminal_event(&mut duplicate_terminal);
    assert!(duplicate_terminal["response"].get("output").is_none());

    let mut mismatch = ResponsesSnapshotAccumulator::default();
    observe(
        &mut mismatch,
        json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "msg_one", "type": "message", "status": "in_progress",
                "role": "assistant", "content": []}
        }),
    );
    observe(
        &mut mismatch,
        json!({
            "type": "response.output_text.done", "output_index": 0,
            "item_id": "msg_other", "content_index": 0, "text": "foreign"
        }),
    );
    let mut mismatch_terminal = json!({
        "type": "response.completed",
        "response": {"id": "resp_mismatch", "status": "completed"}
    });
    mismatch.repair_terminal_event(&mut mismatch_terminal);
    assert!(mismatch_terminal["response"].get("output").is_none());
}

#[test]
fn malformed_or_type_mismatched_item_lifecycle_fails_closed() {
    let malformed = [
        json!({
            "type": "response.output_item.done",
            "item": {"id": "msg_one", "type": "message", "status": "completed"}
        }),
        json!({
            "type": "response.output_item.done", "output_index": 1,
            "item": []
        }),
        json!({
            "type": "response.output_item.done", "output_index": 1,
            "item": {"type": "message", "status": "completed"}
        }),
        json!({
            "type": "response.output_item.done", "output_index": 1,
            "item": {"id": "msg_one", "type": "", "status": "completed"}
        }),
    ];

    for event in malformed {
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        observe(
            &mut snapshot,
            json!({
                "type": "response.output_item.done", "output_index": 0,
                "item": {"id": "msg_safe", "type": "message", "status": "completed",
                    "role": "assistant", "content": []}
            }),
        );
        observe(&mut snapshot, event);

        let mut sparse = json!({
            "type": "response.completed",
            "response": {"id": "resp_sparse", "status": "completed"}
        });
        snapshot.repair_terminal_event(&mut sparse);
        assert!(sparse["response"].get("output").is_none());

        let mut generic = json!({
            "type": "response.completed",
            "response": {"id": "resp_generic", "status": "completed", "output": []}
        });
        snapshot.preserve_generic_terminal_event(&mut generic);
        assert!(generic["response"]["output"]
            .as_array()
            .is_some_and(Vec::is_empty));
    }

    let mut mismatch = ResponsesSnapshotAccumulator::default();
    observe(
        &mut mismatch,
        json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "item_one", "type": "message", "status": "in_progress",
                "role": "assistant", "content": []}
        }),
    );
    observe(
        &mut mismatch,
        json!({
            "type": "response.output_item.done", "output_index": 0,
            "item": {"id": "item_one", "type": "function_call", "status": "completed",
                "call_id": "call_one", "name": "lookup", "arguments": "{}"}
        }),
    );
    let mut terminal = json!({
        "type": "response.completed",
        "response": {"id": "resp_hybrid", "status": "completed"}
    });
    mismatch.repair_terminal_event(&mut terminal);
    assert!(terminal["response"].get("output").is_none());
}

#[test]
fn raw_missing_or_conflicting_ids_taint_before_id_repair() {
    let cases = [
        vec![json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"type": "message", "status": "in_progress",
                "role": "assistant", "content": []}
        })],
        vec![
            json!({
                "type": "response.output_item.added", "output_index": 0,
                "item": {"id": "msg_first", "type": "message", "status": "in_progress",
                    "role": "assistant", "content": []}
            }),
            json!({
                "type": "response.output_item.done", "output_index": 0,
                "item": {"id": "msg_other", "type": "message", "status": "completed",
                    "role": "assistant", "content": []}
            }),
        ],
    ];

    for events in cases {
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        let mut repair = ResponsesItemIdRepair::new_with_policy(
            &crate::config::ResponseItemIdRepairSettings::default(),
            true,
        )
        .expect("ID repair");
        for mut event in events {
            snapshot.observe(&event);
            repair.repair_event(&mut event);
        }
        let mut terminal = json!({
            "type": "response.completed",
            "response": {"id": "resp_raw_id", "status": "completed"}
        });
        snapshot.closing_events_before_terminal(&terminal);
        repair.repair_event(&mut terminal);
        snapshot.repair_terminal_event(&mut terminal);
        assert!(terminal["response"].get("output").is_none());
    }
}

#[test]
fn http_snapshot_repair_rejects_delta_only_items_without_added_identity() {
    for event in [
        json!({
            "type": "response.output_text.delta", "output_index": 0,
            "item_id": "msg_unproven", "content_index": 0, "delta": "text"
        }),
        json!({
            "type": "response.reasoning_summary_text.delta", "output_index": 0,
            "item_id": "reasoning_unproven", "summary_index": 0, "delta": "thought"
        }),
        json!({
            "type": "response.function_call_arguments.delta", "output_index": 0,
            "item_id": "function_unproven", "delta": "{}"
        }),
        json!({
            "type": "response.custom_tool_call_input.delta", "output_index": 0,
            "item_id": "custom_unproven", "delta": "pwd"
        }),
    ] {
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        assert!(snapshot.injected_events_before(&event).is_empty());
        snapshot.observe(&event);
        let mut terminal = json!({
            "type": "response.completed",
            "response": {"id": "resp_unproven", "status": "completed"}
        });

        snapshot.closing_events_before_terminal(&terminal);
        snapshot.repair_terminal_event(&mut terminal);

        assert!(snapshot.is_tainted());
        assert!(terminal["response"].get("output").is_none());
    }
}

#[test]
fn proven_output_index_accepts_omitted_item_id_but_rejects_a_foreign_one() {
    let cases = [
        (
            json!({"id": "msg_one", "type": "message", "status": "in_progress",
                    "role": "assistant", "content": []}),
            json!({"type": "response.output_text.delta", "output_index": 0,
                    "content_index": 0, "delta": "hello"}),
            json!({"type": "response.output_text.done", "output_index": 0,
                    "content_index": 0, "text": "hello"}),
        ),
        (
            json!({"id": "rs_one", "type": "reasoning", "status": "in_progress",
                    "summary": []}),
            json!({"type": "response.reasoning_summary_text.delta", "output_index": 0,
                    "summary_index": 0, "delta": "checked"}),
            json!({"type": "response.reasoning_summary_text.done", "output_index": 0,
                    "summary_index": 0, "text": "checked"}),
        ),
        (
            json!({"id": "fc_one", "type": "function_call", "status": "in_progress",
                    "call_id": "call_one", "name": "lookup", "arguments": ""}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 0,
                    "delta": "{}"}),
            json!({"type": "response.function_call_arguments.done", "output_index": 0,
                    "arguments": "{}"}),
        ),
        (
            json!({"id": "ct_one", "type": "custom_tool_call", "status": "in_progress",
                    "call_id": "call_one", "name": "exec", "input": ""}),
            json!({"type": "response.custom_tool_call_input.delta", "output_index": 0,
                    "delta": "pwd"}),
            json!({"type": "response.custom_tool_call_input.done", "output_index": 0,
                    "input": "pwd"}),
        ),
    ];

    for (item, event, done) in cases {
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        snapshot.observe(&json!({
            "type": "response.output_item.added", "output_index": 0, "item": item
        }));
        snapshot.observe(&event);
        snapshot.observe(&done);
        assert!(!snapshot.is_tainted());

        let mut foreign = event;
        foreign["item_id"] = Value::String("foreign".into());
        snapshot.observe(&foreign);
        assert!(snapshot.is_tainted());
    }

    let mut content_part = ResponsesSnapshotAccumulator::default();
    content_part.observe(&json!({
        "type": "response.output_item.added", "output_index": 0,
        "item": {"id": "msg_part", "type": "message", "status": "in_progress"}
    }));
    content_part.observe(&json!({
        "type": "response.content_part.added", "output_index": 0, "content_index": 0,
        "part": {"type": "output_text"}
    }));
    content_part.observe(&json!({
        "type": "response.content_part.done", "output_index": 0, "content_index": 0,
        "part": {"type": "output_text", "text": "", "annotations": []}
    }));
    assert!(!content_part.is_tainted());
}

#[test]
fn reasoning_content_parts_follow_the_proven_reasoning_item_identity() {
    let mut snapshot = ResponsesSnapshotAccumulator::default();
    snapshot.observe(&json!({
        "type": "response.output_item.added", "output_index": 0,
        "item": {"id": "rs_part", "type": "reasoning", "status": "in_progress",
            "content": [], "encrypted_content": "provider-signature"}
    }));
    snapshot.observe(&json!({
        "type": "response.content_part.added", "output_index": 0,
        "item_id": "rs_part", "content_index": 0,
        "part": {"type": "reasoning_text", "text": ""}
    }));
    snapshot.observe(&json!({
        "type": "response.content_part.done", "output_index": 0,
        "item_id": "rs_part", "content_index": 0,
        "part": {"type": "reasoning_text", "text": "private thought"}
    }));
    let terminal = json!({
        "type": "response.completed", "response": {"id": "resp_reasoning"}
    });
    let closing = snapshot.closing_events_before_terminal(&terminal);

    assert!(!snapshot.is_tainted());
    assert_eq!(closing.len(), 1);
    assert_eq!(closing[0]["type"], "response.output_item.done");
    assert_eq!(closing[0]["item"]["type"], "reasoning");
    assert_eq!(
        closing[0]["item"]["encrypted_content"],
        "provider-signature"
    );

    let mut foreign = ResponsesSnapshotAccumulator::default();
    foreign.observe(&json!({
        "type": "response.output_item.added", "output_index": 0,
        "item": {"id": "rs_part", "type": "reasoning", "status": "in_progress"}
    }));
    foreign.observe(&json!({
        "type": "response.content_part.done", "output_index": 0,
        "item_id": "msg_foreign", "content_index": 0,
        "part": {"type": "reasoning_text", "text": "wrong item"}
    }));
    assert!(foreign.is_tainted());
}

#[test]
fn completed_event_without_status_closes_items_and_backfills_request_defaults() {
    let mut snapshot = ResponsesSnapshotAccumulator::default();
    snapshot.observe(&json!({
        "type": "response.output_item.added", "output_index": 0,
        "item": {"id": "msg_sparse", "type": "message"}
    }));
    snapshot.observe(&json!({
        "type": "response.output_text.delta", "output_index": 0,
        "content_index": 0, "delta": "hello"
    }));
    let mut terminal = json!({
        "type": "response.completed",
        "response": {"id": "resp_sparse"}
    });
    let request = json!({
        "parallel_tool_calls": false,
        "tool_choice": {"type": "none"},
        "tools": [{"type": "function", "name": "lookup"}]
    });

    let closing = snapshot.closing_events_before_terminal(&terminal);
    snapshot.repair_terminal_event_with_request(&mut terminal, &request);

    assert_eq!(
        closing
            .last()
            .and_then(|event| event.get("type"))
            .and_then(Value::as_str),
        Some("response.output_item.done")
    );
    assert_eq!(terminal["response"]["status"], "completed");
    assert_eq!(terminal["response"]["parallel_tool_calls"], false);
    assert_eq!(terminal["response"]["tool_choice"]["type"], "none");
    assert_eq!(
        terminal["response"]["tools"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(terminal["response"]["output"][0]["role"], "assistant");
    assert_eq!(
        terminal["response"]["output"][0]["content"][0]["annotations"],
        json!([])
    );
}

#[test]
fn non_stream_snapshot_repair_canonicalizes_before_id_repair() {
    let request = json!({"parallel_tool_calls": false, "tools": []});
    let mut missing = json!({"id": "resp_json", "output": {}});
    repair_responses_snapshot_json(&mut missing, &request);
    assert_eq!(missing["status"], "completed");
    assert!(missing["output"].as_array().is_some_and(Vec::is_empty));

    let mut explicit = json!({
        "id": "resp_json_items", "output": [{
            "id": "placeholder", "type": "message",
            "content": [{"type": "output_text"}]
        }]
    });
    repair_responses_snapshot_json(&mut explicit, &request);
    let mut settings = crate::config::ResponseItemIdRepairSettings::default();
    settings.message = vec!["placeholder".into()];
    let mut id_repair = ResponsesItemIdRepair::new(&settings).expect("ID repair");
    id_repair.repair_response(&mut explicit);

    assert_ne!(explicit["output"][0]["id"], "placeholder");
    assert_eq!(explicit["output"][0]["role"], "assistant");
    assert_eq!(explicit["output"][0]["content"][0]["text"], "");
    assert_eq!(
        explicit["output"][0]["content"][0]["annotations"],
        json!([])
    );
}

#[test]
fn client_id_repair_never_enters_non_stream_continuation_history() {
    let raw = json!({
        "id": "resp_raw_replay", "status": "completed", "output": [{
            "id": "placeholder", "type": "reasoning", "status": "completed",
            "content": [{"type": "reasoning_text", "text": "thought"}],
            "encrypted_content": "provider-owned-signature"
        }]
    });
    let mut settings = crate::config::ResponseItemIdRepairSettings::default();
    settings.reasoning = vec!["placeholder".into()];
    let mut id_repair = ResponsesItemIdRepair::new(&settings).expect("ID repair");
    let (client, replay) = prepare_responses_json_for_client_and_replay(
        raw,
        &json!({"tools": []}),
        Some(&mut id_repair),
        true,
    );
    assert!(client["output"][0]["id"]
        .as_str()
        .is_some_and(|id| id.starts_with("rs_codetas_")));
    assert_eq!(replay["output"][0]["id"], "placeholder");
    assert_eq!(
        replay["output"][0]["encrypted_content"],
        "provider-owned-signature"
    );

    let store = ResponseStateStore::default();
    store.remember(&json!({"input": [], "store": true}), &replay, false);
    let mut continuation = json!({
        "previous_response_id": "resp_raw_replay",
        "input": [{"role": "user", "content": "continue"}]
    });
    assert!(store.expand_previous_response_input(&mut continuation));
    // Wire path strips internal lease/session control fields before serialize.
    ResponseStateStore::strip_private_fields(&mut continuation);
    let encoded = serde_json::to_string(&continuation).unwrap();
    assert!(!encoded.contains("_codetas_"));
    assert!(encoded.contains("provider-owned-signature"));
    assert!(encoded.contains("placeholder"));
}

#[test]
fn client_id_repair_never_enters_stream_continuation_history() {
    let request = json!({"input": [], "store": true, "tools": []});
    let raw_added = json!({
        "type": "response.output_item.added", "output_index": 0,
        "item": {"id": "placeholder", "type": "reasoning", "status": "in_progress",
            "encrypted_content": "provider-stream-signature"}
    });
    let raw_terminal = json!({
        "type": "response.completed", "response": {"id": "resp_stream_raw"}
    });

    let mut raw_snapshot = ResponsesSnapshotAccumulator::default();
    raw_snapshot.observe(&raw_added);
    let mut replay_terminal = raw_terminal.clone();
    raw_snapshot.closing_events_before_terminal(&replay_terminal);
    let replay = raw_snapshot
        .repair_terminal_event_with_request(&mut replay_terminal, &request)
        .expect("raw replay response");

    let mut client_snapshot = ResponsesSnapshotAccumulator::default();
    let mut client_added = raw_added;
    let mut settings = crate::config::ResponseItemIdRepairSettings::default();
    settings.reasoning = vec!["placeholder".into()];
    let mut id_repair = ResponsesItemIdRepair::new(&settings).expect("ID repair");
    id_repair.repair_event(&mut client_added);
    client_snapshot.observe(&client_added);
    let mut client_terminal = raw_terminal;
    for event in client_snapshot.closing_events_before_terminal(&client_terminal) {
        let mut event = event;
        id_repair.repair_event(&mut event);
    }
    client_snapshot.repair_terminal_event_with_request(&mut client_terminal, &request);
    id_repair.repair_event(&mut client_terminal);
    assert!(client_terminal["response"]["output"][0]["id"]
        .as_str()
        .is_some_and(|id| id.starts_with("rs_codetas_")));

    let store = ResponseStateStore::default();
    store.remember(&request, &replay, false);
    let mut continuation = json!({
        "previous_response_id": "resp_stream_raw", "input": []
    });
    assert!(store.expand_previous_response_input(&mut continuation));
    ResponseStateStore::strip_private_fields(&mut continuation);
    let encoded = serde_json::to_string(&continuation).unwrap();
    assert!(!encoded.contains("_codetas_"));
    assert!(encoded.contains("provider-stream-signature"));
    assert!(encoded.contains("placeholder"));
}

#[test]
fn nonterminal_snapshot_events_receive_canonical_part_and_logprob_defaults() {
    let mut created = json!({
        "type": "response.created", "response": {"id": "resp_created"}
    });
    repair_responses_snapshot_event(
        &mut created,
        &json!({"parallel_tool_calls": false, "tool_choice": "none", "tools": []}),
    );
    assert_eq!(created["response"]["status"], "in_progress");
    assert_eq!(created["response"]["parallel_tool_calls"], false);

    let mut text_done = json!({"type": "response.output_text.done"});
    repair_responses_snapshot_event(&mut text_done, &Value::Null);
    assert_eq!(text_done["text"], "");
    assert!(text_done["logprobs"].is_array());

    let mut content = json!({
        "type": "response.content_part.added",
        "part": {"type": "output_text"}
    });
    repair_responses_snapshot_event(&mut content, &Value::Null);
    assert_eq!(content["part"]["text"], "");
    assert!(content["part"]["annotations"].is_array());

    let mut reasoning = json!({
        "type": "response.reasoning_summary_part.done",
        "part": {"type": "summary_text"}
    });
    repair_responses_snapshot_event(&mut reasoning, &Value::Null);
    assert_eq!(reasoning["part"]["text"], "");
}

#[test]
fn reconstructed_terminal_ids_are_repaired_after_raw_lifecycle_collection() {
    let mut settings = crate::config::ResponseItemIdRepairSettings::default();
    settings.message = vec!["placeholder".into()];
    let mut id_repair = ResponsesItemIdRepair::new(&settings).expect("ID repair");
    let mut snapshot = ResponsesSnapshotAccumulator::default();
    let raw_added = json!({
        "type": "response.output_item.added", "output_index": 0,
        "item": {"id": "placeholder", "type": "message", "status": "in_progress",
            "role": "assistant", "content": []}
    });
    snapshot.observe(&raw_added);
    let mut client_added = raw_added;
    id_repair.repair_event(&mut client_added);
    let repaired_id = client_added["item"]["id"]
        .as_str()
        .expect("repaired item ID")
        .to_string();
    let raw_delta = json!({
        "type": "response.output_text.delta", "output_index": 0,
        "content_index": 0, "delta": "hello"
    });
    snapshot.injected_events_before(&raw_delta);
    snapshot.observe(&raw_delta);
    let mut terminal = json!({
        "type": "response.completed", "response": {"id": "resp_ids"}
    });
    let mut closing = snapshot.closing_events_before_terminal(&terminal);
    for event in &mut closing {
        id_repair.repair_event(event);
    }
    id_repair.repair_event(&mut terminal);
    snapshot.repair_terminal_event(&mut terminal);
    id_repair.repair_event(&mut terminal);

    assert_eq!(terminal["response"]["output"][0]["id"], repaired_id);
    assert_eq!(
        closing.last().expect("output item done")["item"]["id"],
        repaired_id
    );
    assert!(closing.iter().all(|event| {
        event
            .get("item_id")
            .and_then(Value::as_str)
            .is_none_or(|item_id| item_id == repaired_id)
    }));
}

#[test]
fn http_snapshot_repair_rejects_completed_output_index_gaps() {
    let mut snapshot = ResponsesSnapshotAccumulator::default();
    observe(
        &mut snapshot,
        json!({
            "type": "response.output_item.done", "output_index": 1,
            "item": {"id": "msg_gap", "type": "message", "status": "completed",
                "role": "assistant", "content": []}
        }),
    );
    let mut terminal = json!({
        "type": "response.completed",
        "response": {"id": "resp_gap", "status": "completed"}
    });

    snapshot.repair_terminal_event(&mut terminal);

    assert!(snapshot.is_tainted());
    assert!(terminal["response"].get("output").is_none());
}

#[test]
fn oversized_collector_taints_and_fails_closed() {
    let mut snapshot = ResponsesSnapshotAccumulator::default();
    observe(
        &mut snapshot,
        json!({
            "type": "response.output_item.done", "output_index": 0,
            "item": {"id": "msg_big", "type": "message", "status": "completed",
                "role": "assistant", "content": [{"type": "output_text",
                    "text": "x".repeat(MAX_SNAPSHOT_REPAIR_BYTES + 1)}]}
        }),
    );
    let mut terminal = json!({
        "type": "response.completed",
        "response": {"id": "resp_big", "status": "completed"}
    });

    snapshot.repair_terminal_event(&mut terminal);

    assert!(terminal["response"].get("output").is_none());
}

#[test]
fn passthrough_preserves_nonterminal_bytes_and_reserializes_repaired_terminal() {
    let nonterminal = b"event: response.output_item.added\r\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg_one\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\r\n\r\n";
    let terminal = b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_one\",\"status\":\"completed\"}}\n\n";
    let mut bytes = nonterminal.to_vec();
    bytes.extend_from_slice(terminal);
    let mut pending = Vec::new();
    let mut usage = TokenUsage::default();
    let mut raw_snapshot = ResponsesSnapshotAccumulator::default();
    let mut snapshot = ResponsesSnapshotAccumulator::default();

    let frames = drain_snapshot_repair_frames(
        &mut pending,
        &bytes,
        &mut usage,
        &mut raw_snapshot,
        &mut snapshot,
        &Value::Null,
    )
    .expect("repair frames");

    assert_eq!(frames.len(), 6);
    assert_eq!(frames[0].bytes.as_ref(), nonterminal);
    assert!(!frames[0].terminal);
    let injected_types = frames[1..5]
        .iter()
        .filter_map(|frame| parse_sse_frame(frame.bytes.as_ref()).ok().flatten())
        .filter_map(|event| {
            event
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        injected_types,
        vec![
            "response.content_part.added",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
        ]
    );
    assert!(frames[5].terminal);
    assert_ne!(frames[5].bytes.as_ref(), terminal);
    let repaired = std::str::from_utf8(&frames[5].bytes).expect("UTF-8 terminal");
    assert!(repaired.contains("msg_one"));
}

#[test]
fn passthrough_canonicalizes_sparse_nonterminal_frames_before_forwarding() {
    let created =
        b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_sparse\"}}\n\n";
    let added = b"data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg_sparse\",\"type\":\"message\"}}\n\n";
    let delta = b"data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"item_id\":\"msg_sparse\",\"content_index\":0,\"delta\":\"hello\"}\n\n";
    let mut bytes = created.to_vec();
    bytes.extend_from_slice(added);
    bytes.extend_from_slice(delta);
    let mut pending = Vec::new();
    let mut usage = TokenUsage::default();
    let mut raw_snapshot = ResponsesSnapshotAccumulator::default();
    let mut snapshot = ResponsesSnapshotAccumulator::default();

    let frames = drain_snapshot_repair_frames(
        &mut pending,
        &bytes,
        &mut usage,
        &mut raw_snapshot,
        &mut snapshot,
        &json!({"parallel_tool_calls": false, "tool_choice": "none", "tools": []}),
    )
    .expect("canonical repair frames");
    let values = frames
        .iter()
        .filter_map(|frame| parse_sse_frame(frame.bytes.as_ref()).ok().flatten())
        .collect::<Vec<_>>();

    assert_eq!(values[0]["response"]["status"], "in_progress");
    assert_eq!(values[0]["response"]["parallel_tool_calls"], false);
    assert_eq!(values[1]["item"]["status"], "in_progress");
    assert_eq!(values[1]["item"]["role"], "assistant");
    assert!(values[1]["item"]["content"].is_array());
    let forwarded_delta = values
        .iter()
        .find(|event| {
            event.get("type").and_then(Value::as_str) == Some("response.output_text.delta")
        })
        .expect("forwarded delta");
    assert!(forwarded_delta["logprobs"].is_array());
}

#[test]
fn passthrough_preserves_an_already_complete_terminal_frame() {
    let terminal = b"event: response.completed\r\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_complete\",\"status\":\"completed\",\"output\":[],\"parallel_tool_calls\":true,\"tool_choice\":\"auto\",\"tools\":[]}}\r\n\r\n";
    let mut pending = Vec::new();
    let mut usage = TokenUsage::default();
    let mut raw_snapshot = ResponsesSnapshotAccumulator::default();
    let mut snapshot = ResponsesSnapshotAccumulator::default();

    let frames = drain_snapshot_repair_frames(
        &mut pending,
        terminal,
        &mut usage,
        &mut raw_snapshot,
        &mut snapshot,
        &Value::Null,
    )
    .expect("complete terminal frame");

    assert_eq!(frames.len(), 1);
    assert!(frames[0].terminal);
    assert_eq!(frames[0].bytes.as_ref(), terminal);
}

#[test]
fn native_and_adapted_http_200_terminal_failures_share_observation_categories() {
    let response = json!({
        "id": "resp_empty_retry",
        "status": "failed",
        "error": {
            "code": EMPTY_COMPLETION_RETRY_FAILED_CODE,
            "message": "retry failed"
        }
    });
    assert_eq!(
        response_terminal_failure_category(&response),
        Some(EMPTY_COMPLETION_RETRY_FAILED_CODE)
    );
    assert_eq!(
        response_terminal_failure_category(&json!({"status": "incomplete"})),
        Some("provider_completion_incomplete")
    );
    assert_eq!(
        response_terminal_failure_category(&json!({"status": "failed"})),
        Some("provider_completion_failed")
    );
}
