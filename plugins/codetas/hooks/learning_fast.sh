#!/bin/sh
# CODETAS learning-loop fast path for UserPromptSubmit / PostToolUse / Stop.
#
# Codex invokes this script on every prompt, tool result, and stop. A Python
# cold start that then imports profile_learning.py / memory_store.py from
# PLUGIN_ROOT (often an external volume) costs hundreds of milliseconds per
# spawn. A turn with a few tool calls was paying that tax ~5 times.
#
# This POSIX script never execs python3. SessionStart writes a tiny key=value
# sidecar plus preformatted review JSON. No-op increments and Stop are then
# open/read/write of those files. Reviews inject additionalContext on the
# user turn and never set Stop decision=block.
#
# Fail-closed: missing sidecar, unresolved identity, or empty scope_token
# exits 0 without incrementing or writing outside the learning state dir.

set -eu

EVENT_NAME="${1:-}"
STATE_DIR="${CODETAS_LEARNING_STATE_DIR:-${HOME}/.codex/codetas-learning}"

EVENT=$(cat || true)

json_str() {
    key=$1
    printf '%s' "$EVENT" | tr '\n' ' ' | sed -n 's/.*"'"$key"'"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1
}

sanitize_sid() {
    printf '%s' "$1" | sed 's/[^A-Za-z0-9._-]/-/g' | cut -c1-120 | sed 's/^-*//;s/-*$//'
}

SESSION_RAW=$(json_str session_id)
[ -n "$SESSION_RAW" ] || SESSION_RAW=$(json_str sessionId)
[ -n "$SESSION_RAW" ] || SESSION_RAW=$(json_str thread_id)
[ -n "$SESSION_RAW" ] || SESSION_RAW=$(json_str threadId)
SID=$(sanitize_sid "$SESSION_RAW")
[ -n "$SID" ] || SID=unknown

FAST="$STATE_DIR/${SID}.fast"
TOOLS="$STATE_DIR/${SID}.tools"

# No SessionStart sidecar yet, or identity never resolved: cheap no-op.
if [ ! -f "$FAST" ]; then
    exit 0
fi

# Parse key=value sidecar. Values are restricted by the Python writer.
kind=
profile_name=
scope_token=
user_turn_count=0
turns_since_memory=0
observed_tool_units=0
user_turns_since_skill_review=0
checkpoint_done=0
memory_review=
skill_review=
missed_flush=0
memory_nudge_interval=10
skill_nudge_interval=15
flush_min_turns=6

while IFS= read -r line || [ -n "$line" ]; do
    case $line in
        ''|\#*) continue ;;
    esac
    key=${line%%=*}
    value=${line#*=}
    case $key in
        kind) kind=$value ;;
        profile_name) profile_name=$value ;;
        scope_token) scope_token=$value ;;
        user_turn_count) user_turn_count=$value ;;
        turns_since_memory) turns_since_memory=$value ;;
        observed_tool_units) observed_tool_units=$value ;;
        user_turns_since_skill_review) user_turns_since_skill_review=$value ;;
        checkpoint_done) checkpoint_done=$value ;;
        memory_review) memory_review=$value ;;
        skill_review) skill_review=$value ;;
        missed_flush) missed_flush=$value ;;
        memory_nudge_interval) memory_nudge_interval=$value ;;
        skill_nudge_interval) skill_nudge_interval=$value ;;
        flush_min_turns) flush_min_turns=$value ;;
    esac
done < "$FAST"

if [ "$kind" = "unresolved" ] || [ -z "$scope_token" ]; then
    exit 0
fi

review_status() {
    printf '%s' "$1" | cut -d: -f1
}

review_reason() {
    printf '%s' "$1" | cut -d: -f2
}

review_id() {
    printf '%s' "$1" | cut -d: -f3-
}

mark_review_due() {
    current=$1
    reason=$2
    status=$(review_status "$current")
    case $status in
        due|dispatched) printf '%s' "$current" ;;
        *) printf 'due:%s:%s' "$reason" "$(awk 'BEGIN{srand(); printf "%08x", int(rand()*4294967295)}')" ;;
    esac
}

write_fast() {
    tmp="$FAST.tmp.$$"
    {
        printf 'version=2\n'
        printf 'kind=%s\n' "$kind"
        printf 'profile_name=%s\n' "$profile_name"
        printf 'scope_token=%s\n' "$scope_token"
        printf 'user_turn_count=%s\n' "$user_turn_count"
        printf 'turns_since_memory=%s\n' "$turns_since_memory"
        printf 'observed_tool_units=%s\n' "$observed_tool_units"
        printf 'user_turns_since_skill_review=%s\n' "$user_turns_since_skill_review"
        printf 'checkpoint_done=%s\n' "$checkpoint_done"
        printf 'memory_review=%s\n' "$memory_review"
        printf 'skill_review=%s\n' "$skill_review"
        printf 'missed_flush=%s\n' "$missed_flush"
        printf 'memory_nudge_interval=%s\n' "$memory_nudge_interval"
        printf 'skill_nudge_interval=%s\n' "$skill_nudge_interval"
        printf 'flush_min_turns=%s\n' "$flush_min_turns"
    } > "$tmp"
    mv "$tmp" "$FAST"
}

emit_review() {
    name=$1
    file="$STATE_DIR/${SID}.hook.${name}.json"
    if [ -f "$file" ]; then
        cat "$file"
    fi
}

review_is_pending() {
    status=$(review_status "$1")
    [ "$status" = "due" ] || [ "$status" = "dispatched" ]
}

looks_like_exit() {
    prompt=$(json_str prompt)
    [ -n "$prompt" ] || prompt=$(json_str user_prompt)
    [ -n "$prompt" ] || prompt=$(json_str userPrompt)
    [ -n "$prompt" ] || prompt=$(json_str text)
    folded=$(printf '%s' "$prompt" | tr '[:upper:]' '[:lower:]' | tr -d '\r')
    case $folded in
        /new|/reset|/exit|/clear|/new\ *|/reset\ *) return 0 ;;
        *) return 1 ;;
    esac
}

case $EVENT_NAME in
    UserPromptSubmit)
        user_turn_count=$((user_turn_count + 1))
        turns_since_memory=$((turns_since_memory + 1))
        user_turns_since_skill_review=$((user_turns_since_skill_review + 1))
        if looks_like_exit; then
            memory_review=$(mark_review_due "$memory_review" exit)
        fi
        if [ "$checkpoint_done" != "1" ] && [ "$user_turn_count" -ge "$flush_min_turns" ]; then
            memory_review=$(mark_review_due "$memory_review" checkpoint)
        fi
        if [ "$turns_since_memory" -ge "$memory_nudge_interval" ]; then
            memory_review=$(mark_review_due "$memory_review" nudge)
            turns_since_memory=0
        fi
        if [ "$observed_tool_units" -ge "$skill_nudge_interval" ] || [ "$user_turns_since_skill_review" -ge "$skill_nudge_interval" ]; then
            skill_review=$(mark_review_due "$skill_review" nudge)
        fi
        memory_pending=0
        skill_pending=0
        review_is_pending "$memory_review" && memory_pending=1
        review_is_pending "$skill_review" && skill_pending=1
        if [ "$memory_pending" -eq 1 ]; then
            memory_review="dispatched:$(review_reason "$memory_review"):$(review_id "$memory_review")"
        fi
        if [ "$skill_pending" -eq 1 ]; then
            skill_review="dispatched:$(review_reason "$skill_review"):$(review_id "$skill_review")"
        fi
        write_fast
        if [ "$memory_pending" -eq 1 ] && [ "$skill_pending" -eq 1 ]; then
            emit_review combined
        elif [ "$memory_pending" -eq 1 ]; then
            reason=$(review_reason "$memory_review")
            case $reason in
                exit) emit_review exit ;;
                checkpoint) emit_review checkpoint ;;
                *) emit_review memory ;;
            esac
        elif [ "$skill_pending" -eq 1 ]; then
            emit_review skill
        fi
        exit 0
        ;;
    PostToolUse)
        tool_name=$(json_str tool_name)
        [ -n "$tool_name" ] || tool_name=$(json_str toolName)
        [ -n "$tool_name" ] || tool_name=$(json_str tool)
        tool_name=$(printf '%s' "$tool_name" | tr '[:upper:]' '[:lower:]')
        case $tool_name in
            memory|skill_manage) exit 0 ;;
        esac
        tool_id=$(json_str call_id)
        [ -n "$tool_id" ] || tool_id=$(json_str callId)
        [ -n "$tool_id" ] || tool_id=$(json_str tool_call_id)
        [ -n "$tool_id" ] || tool_id=$(json_str toolCallId)
        [ -n "$tool_id" ] || tool_id=$(json_str id)
        if [ -n "$tool_id" ] && [ -f "$TOOLS" ] && grep -F -x -q -- "$tool_id" "$TOOLS"; then
            exit 0
        fi
        if [ -n "$tool_id" ]; then
            printf '%s\n' "$tool_id" >> "$TOOLS"
            tail_count=200
            if [ "$(wc -l < "$TOOLS")" -gt "$tail_count" ]; then
                tail -n "$tail_count" "$TOOLS" > "$TOOLS.tmp.$$"
                mv "$TOOLS.tmp.$$" "$TOOLS"
            fi
        fi
        observed_tool_units=$((observed_tool_units + 1))
        if [ "$observed_tool_units" -ge "$skill_nudge_interval" ]; then
            skill_review=$(mark_review_due "$skill_review" tools)
        fi
        write_fast
        exit 0
        ;;
    Stop)
        changed=0
        mem_status=$(review_status "$memory_review")
        skill_status=$(review_status "$skill_review")
        if [ "$mem_status" = "dispatched" ]; then
            memory_review="acknowledged:$(review_reason "$memory_review"):$(review_id "$memory_review")"
            reason=$(review_reason "$memory_review")
            case $reason in
                checkpoint|exit) checkpoint_done=1 ;;
            esac
            changed=1
        fi
        if [ "$skill_status" = "dispatched" ]; then
            skill_review="acknowledged:$(review_reason "$skill_review"):$(review_id "$skill_review")"
            observed_tool_units=0
            user_turns_since_skill_review=0
            changed=1
        fi
        if [ "$changed" -eq 1 ]; then
            write_fast
        fi
        exit 0
        ;;
    *)
        exit 0
        ;;
esac
