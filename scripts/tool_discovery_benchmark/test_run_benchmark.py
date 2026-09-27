import asyncio
import importlib.util
import sys
from pathlib import Path

import pytest


SCRIPT = Path(__file__).with_name("run_benchmark.py")
SPEC = importlib.util.spec_from_file_location("tool_discovery_benchmark", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
BENCH = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = BENCH
SPEC.loader.exec_module(BENCH)


def test_catalog_is_deterministic_bounded_and_fair():
    first = BENCH.generate_catalog(1000)
    second = BENCH.generate_catalog(1000)

    assert first == second
    assert len(first) == 20
    assert sum(len(bucket["tools"]) for bucket in first) == 1000
    assert max(len(bucket["tools"]) for bucket in first) - min(
        len(bucket["tools"]) for bucket in first
    ) <= 1
    namespace_by_tool = {
        tool["name"]: bucket["namespace"]
        for bucket in first
        for tool in bucket["tools"]
    }
    assert namespace_by_tool["github__get_pull_request"] == "github"
    assert namespace_by_tool["gmail__search_messages"] == "gmail"
    assert namespace_by_tool["google_calendar__create_event"] == "google-calendar"


def test_catalog_rejects_tool_count_below_fixed_corpus():
    corpus_size = len(BENCH.json.loads(BENCH.CORPUS_PATH.read_text())["tools"])

    with pytest.raises(ValueError, match=f"at least {corpus_size}"):
        BENCH.generate_catalog(corpus_size - 1)


def test_catalog_rejects_empty_namespace_packages():
    corpus_size = len(BENCH.json.loads(BENCH.CORPUS_PATH.read_text())["tools"])

    with pytest.raises(ValueError, match="empty benchmark namespace"):
        BENCH.generate_catalog(corpus_size)


def test_score_requires_complete_workflow_and_no_match_silence():
    workflow = next(task for task in BENCH.TASKS if task["id"] == "cross-namespace-workflow")
    no_match = next(task for task in BENCH.TASKS if task["id"] == "no-match")

    partial = BENCH.score_task(
        workflow,
        [{"name": "gmail__search_messages", "arguments": {"query": "Project Aurora"}}],
        [],
    )
    complete = BENCH.score_task(
        workflow,
        [
            {"name": "gmail__search_messages", "arguments": {"query": "Project Aurora"}},
            {
                "name": "google_calendar__create_event",
                "arguments": {
                    "schedule": {
                        "start_at": "2026-08-12T10:00:00Z",
                        "end_at": "2026-08-12T10:30:00Z",
                    }
                },
            },
        ],
        [],
    )
    reversed_calls = BENCH.score_task(
        workflow,
        list(reversed([
            {"name": "gmail__search_messages", "arguments": {"query": "Project Aurora"}},
            {
                "name": "google_calendar__create_event",
                "arguments": {
                    "schedule": {
                        "start_at": "2026-08-12T10:00:00Z",
                        "end_at": "2026-08-12T10:30:00Z",
                    }
                },
            },
        ])),
        [],
    )

    assert not partial["completed"]
    assert complete["completed"]
    assert not reversed_calls["completed"]
    assert BENCH.score_task(no_match, [], [{"name": "tool_search", "arguments": {}}])[
        "completed"
    ]
    assert not BENCH.score_task(
        no_match, [], [{"name": "builtin__write_file", "arguments": {}}]
    )["completed"]


def test_score_checks_required_arguments_and_unauthorized_attempts():
    upload = next(task for task in BENCH.TASKS if task["id"] == "nested-argument-vocabulary")
    denied = next(task for task in BENCH.TASKS if task["id"] == "denied-capability")
    wrong_upload = BENCH.score_task(
        upload,
        [{
            "name": "google_drive__upload_file",
            "arguments": {"name": "report.csv", "content": "wrong", "mime_type": "text/csv"},
        }],
        [],
    )
    denied_attempt = BENCH.score_task(
        denied,
        [],
        [{"name": "builtin__spawn_subagent", "arguments": {}}],
    )

    assert not wrong_upload["completed"]
    assert not denied_attempt["completed"]
    assert denied_attempt["unauthorized_tool_leaks"] == 1


def test_first_correct_tool_latency_skips_unrelated_calls():
    calls = [
        {"name": "unrelated", "monotonic_ns": 1_100_000_000},
        {"name": "expected", "monotonic_ns": 1_400_000_000},
    ]

    assert BENCH.first_correct_tool_call_latency_ms(("expected",), calls, 1.0) == 400
    assert BENCH.first_correct_tool_call_latency_ms((), calls, 1.0) is None


def test_discovery_turns_count_model_steps_not_calls():
    calls = [
        {"name": "tool_search", "model_turn": 1},
        {"name": "tool_describe", "model_turn": 1},
        {"name": "tool_search", "model_turn": 3},
        {"name": "github__get_repo", "model_turn": 4},
    ]

    assert BENCH.discovery_turn_count(calls) == 2


def test_result_read_count_includes_bridged_targets_in_either_spelling():
    calls = [
        {"name": "builtin__result_read", "arguments": {"result_id": "a"}},
        {"name": "tool_call", "arguments": {"name": "builtin.result_read"}},
        {"name": "tool_call", "arguments": {"name": "builtin__result_read"}},
        {"name": "tool_call", "arguments": {"name": "github__get_repo"}},
        {"name": "tool_search", "arguments": {"query": "result_read"}},
        {"name": "builtin__result_reader", "arguments": {}},
    ]

    assert BENCH.result_read_call_count(calls) == 3
    assert BENCH.result_read_call_count([]) == 0


def test_bridged_tool_call_count_counts_only_the_bridge():
    calls = [
        {"name": "tool_call", "arguments": {"name": "github__get_repo"}},
        {"name": "tool_call", "arguments": {}},
        {"name": "github__get_repo", "arguments": {}},
        {"name": "tool_search", "arguments": {"query": "tool_call"}},
    ]

    assert BENCH.bridged_tool_call_count(calls) == 2
    assert BENCH.bridged_tool_call_count([]) == 0


def test_tool_definition_signature_changes_counts_array_changes_only():
    tools_a = [{"type": "function", "function": {"name": "a", "parameters": {}}}]
    tools_b = [{"type": "function", "function": {"name": "b", "parameters": {}}}]
    sig_a = BENCH.tool_definitions_signature({"messages": [], "tools": tools_a})
    sig_b = BENCH.tool_definitions_signature({"messages": [], "tools": tools_b})
    reordered = BENCH.tool_definitions_signature(
        {"messages": [], "tools": tools_b + tools_a}
    )

    assert sig_a == BENCH.tool_definitions_signature({"tools": list(tools_a)})
    assert sig_a != sig_b
    assert reordered != BENCH.tool_definitions_signature({"tools": tools_a + tools_b})
    assert BENCH.tool_definitions_signature({"messages": []}) is None
    assert BENCH.tool_definitions_signature({"tools": []}) is None

    stable = [{"tools_signature": sig_a}] * 3
    changed = [
        {"tools_signature": sig_a},
        {"tools_signature": None},
        {"tools_signature": sig_b},
        {"tools_signature": sig_b},
        {"tools_signature": sig_a},
    ]
    assert BENCH.tool_definition_signature_changes(stable) == 0
    assert BENCH.tool_definition_signature_changes(changed) == 2
    assert BENCH.tool_definition_signature_changes([{"tools_signature": None}]) is None
    assert BENCH.tool_definition_signature_changes([]) is None


def test_request_recorder_relays_unchanged_and_records_tools_hash():
    received = []

    class Upstream(BENCH.BaseHTTPRequestHandler):
        def log_message(self, *_args):
            return

        def do_POST(self):
            length = int(self.headers.get("content-length", "0"))
            received.append({
                "path": self.path,
                "authorization": self.headers.get("authorization"),
                "body": self.rfile.read(length),
            })
            payload = b'data: {"ok": true}\n\ndata: [DONE]\n\n'
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("content-length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

    upstream = BENCH.ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    thread = BENCH.threading.Thread(target=upstream.serve_forever, daemon=True)
    thread.start()
    recorder = BENCH.LlmRequestRecorder(
        f"http://127.0.0.1:{upstream.server_address[1]}/api/v1/"
    )
    recorder.start()
    try:
        tools = [{"type": "function", "function": {"name": "a", "parameters": {}}}]
        body = BENCH.json.dumps({"model": "m", "messages": [], "tools": tools}).encode()
        request = BENCH.urllib.request.Request(
            f"{recorder.base_url}/chat/completions",
            data=body,
            headers={"authorization": "Bearer secret", "content-type": "application/json"},
            method="POST",
        )
        with BENCH.urllib.request.urlopen(request, timeout=10) as response:
            relayed = response.read()
    finally:
        recorder.stop()
        upstream.shutdown()
        upstream.server_close()

    assert relayed == b'data: {"ok": true}\n\ndata: [DONE]\n\n'
    assert received == [{
        "path": "/api/v1/chat/completions",
        "authorization": "Bearer secret",
        "body": body,
    }]
    assert len(recorder.requests) == 1
    assert recorder.requests[0]["tools_signature"] == BENCH.tool_definitions_signature(
        {"tools": tools}
    )
    assert "secret" not in BENCH.json.dumps(recorder.requests)


def test_model_base_url_resolution_mirrors_live_qa_config():
    assert BENCH.resolve_model_base_url({}) is None
    assert BENCH.resolve_model_base_url({
        "REBORN_WEBUI_V2_LIVE_QA_LLM_PROVIDER_ID": "openai_compatible",
        "LIVE_OPENAI_COMPATIBLE_BASE_URL": "https://api.example/v1",
    }) == "https://api.example/v1"
    assert BENCH.resolve_model_base_url({
        "REBORN_WEBUI_V2_LIVE_QA_LLM_BASE_URL": "https://override/v1",
    }) == "https://override/v1"


def test_git_head_is_nonempty_checked_provenance():
    head = asyncio.run(BENCH.git_head())

    assert len(head) == 40
    assert all(character in "0123456789abcdef" for character in head)


def test_upload_task_is_self_contained_and_does_not_require_a_workspace_fixture():
    upload = next(task for task in BENCH.TASKS if task["id"] == "nested-argument-vocabulary")

    assert "report.csv" in upload["prompt"]
    assert "benchmark-report" in upload["prompt"]
    assert "mime_type" in upload["prompt"]
    assert "text/csv" in upload["prompt"]


def test_observations_are_durable_and_resume_without_duplicates(tmp_path):
    path = tmp_path / "observations.jsonl"
    observation = {
        "schema_version": BENCH.OBSERVATION_SCHEMA_VERSION,
        "observation_id": "namespaces:100:no-match:0",
        "arm": "namespaces",
        "catalog": {"tool_count": 100},
        "run": {"repetition": 0},
        "task": {"id": "no-match"},
    }

    BENCH.append_observation(path, observation)
    BENCH.append_observation(path, observation)

    loaded = BENCH.load_observations(path)
    assert loaded == [observation]


def test_observation_resume_rejects_stale_schema_before_deduplication(tmp_path):
    path = tmp_path / "observations.jsonl"
    path.write_text(
        BENCH.json.dumps({
            "schema_version": BENCH.OBSERVATION_SCHEMA_VERSION - 1,
            "observation_id": "namespaces:100:no-match:0",
        }) + "\n",
        encoding="utf-8",
    )

    with pytest.raises(ValueError, match="schema_version"):
        BENCH.load_observations(path)


def test_aggregate_keeps_completion_and_latency_by_arm_and_size():
    observations = [
        {
            "arm": "bridged",
            "catalog": {"tool_count": 100},
            "task": {"completed": True, "unauthorized_tool_leaks": 0},
            "latency_ms": {"end_to_end": 10},
            "failure": None,
        },
        {
            "arm": "bridged",
            "catalog": {"tool_count": 100},
            "task": {"completed": False, "unauthorized_tool_leaks": 0},
            "latency_ms": {"end_to_end": 30},
            "failure": "task_incomplete",
        },
    ]
    assert BENCH.aggregate_observations(observations) == [
        {
            "arm": "bridged",
            "tool_count": 100,
            "observations": 2,
            "completion_rate": 0.5,
            "latency_ms_median": 20.0,
            "latency_ms_worst": 30,
            "latency_ms_spread": 20,
            "unauthorized_tool_leaks": 0,
            "failure_categories": {"task_incomplete": 1},
            "model_turns_mean": None,
            "model_turns_median": None,
            "model_turns_max": None,
            "tool_search_calls_total": None,
            "tool_search_calls_mean": None,
            "tool_describe_calls_total": None,
            "tool_describe_calls_mean": None,
            "result_read_calls_total": None,
            "bridged_tool_calls_total": None,
            "token_usage_observations": 0,
            "input_tokens_mean": None,
            "input_tokens_median": None,
            "cached_input_tokens_mean": None,
            "cached_input_tokens_median": None,
            "first_correct_tool_observations": 0,
            "time_to_first_correct_tool_ms_median": None,
            "time_to_first_correct_tool_ms_worst": None,
            "tool_definition_signature_changes_total": None,
            "observations_with_tool_definition_changes": None,
        }
    ]


def _observation(arm, tool_count, *, counts, tokens, first_correct, signature_changes):
    return {
        "arm": arm,
        "catalog": {"tool_count": tool_count},
        "task": {"completed": True, "unauthorized_tool_leaks": 0},
        "latency_ms": {"end_to_end": 100, "time_to_first_correct_tool_call": first_correct},
        "counts": counts,
        "tokens": tokens,
        "cache": {"tool_definition_signature_changes": signature_changes},
        "failure": None,
    }


def test_aggregate_reports_turns_discovery_tokens_and_first_correct_tool():
    counts = {
        "model_turns": 2, "tool_search_calls": 1, "tool_describe_calls": 0,
        "result_read_calls": 0, "bridged_tool_calls": 1,
    }
    observations = [
        _observation(
            "namespaces", 500,
            counts=counts,
            tokens={"input": 1000, "cached_input": 800},
            first_correct=300, signature_changes=0,
        ),
        _observation(
            "namespaces", 500,
            counts={**counts, "model_turns": 4, "tool_describe_calls": 2},
            tokens={"input": 3000, "cached_input": 0},
            first_correct=None, signature_changes=2,
        ),
        _observation(
            "namespaces", 500,
            counts={**counts, "model_turns": 3, "result_read_calls": 1},
            # Provider reported no usage: must not be averaged in as zero.
            tokens={"input": 0, "cached_input": None},
            first_correct=500, signature_changes=None,
        ),
    ]

    [aggregate] = BENCH.aggregate_observations(observations)

    assert aggregate["model_turns_mean"] == 3.0
    assert aggregate["model_turns_median"] == 3
    assert aggregate["model_turns_max"] == 4
    assert aggregate["tool_search_calls_total"] == 3
    assert aggregate["tool_search_calls_mean"] == 1.0
    assert aggregate["tool_describe_calls_total"] == 2
    assert aggregate["tool_describe_calls_mean"] == 0.67
    assert aggregate["result_read_calls_total"] == 1
    assert aggregate["bridged_tool_calls_total"] == 3
    assert aggregate["token_usage_observations"] == 2
    assert aggregate["input_tokens_mean"] == 2000.0
    assert aggregate["input_tokens_median"] == 2000.0
    assert aggregate["cached_input_tokens_mean"] == 400.0
    assert aggregate["first_correct_tool_observations"] == 2
    assert aggregate["time_to_first_correct_tool_ms_median"] == 400.0
    assert aggregate["time_to_first_correct_tool_ms_worst"] == 500
    assert aggregate["tool_definition_signature_changes_total"] == 2
    assert aggregate["observations_with_tool_definition_changes"] == 1


def test_run_cache_metadata_marks_first_resumed_execution_cold():
    assert BENCH.run_cache_metadata([2, 3], 0, 2) == {
        "thermal_class": "cold",
        "repetition": 2,
        "resumed_group": True,
    }
    assert BENCH.run_cache_metadata([2, 3], 1, 3) == {
        "thermal_class": "warm",
        "repetition": 3,
        "resumed_group": True,
    }
