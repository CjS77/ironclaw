# Real-model tool-discovery benchmark

This runner exercises the shipping `ironclaw serve` binary and configured live
model against deterministic 100-, 500-, and 1,000-tool catalogs. Twenty local
MCP integrations use stable semantic identities such as `github`, `gmail`, and
`google-calendar`; distractors are distributed as evenly as the fixed relevance
corpus permits. They provide read-only synthetic side effects and record the
exact tools and arguments invoked. IronClaw still performs normal authorization,
approval, hooks, safety, and MCP dispatch.

```bash
cargo build -p ironclaw
# Requires NEARAI_API_KEY or LIVE_OPENAI_COMPATIBLE_API_KEY.
export NEARAI_API_KEY=...
python3 scripts/tool_discovery_benchmark/run_benchmark.py \
  --output-dir /tmp/ironclaw-tool-discovery-benchmark
```

The default matrix runs all five disclosure arms, all three catalog sizes, all
required scenario classes, and four repetitions (one cold plus three warm).
Use repeated `--arm`, `--tool-count`, or `--task` flags only for diagnosis.

Each observation is appended and synced as soon as it completes, and an
interrupted run resumes by stable observation id. Scoring checks required call
order and arguments, detects forbidden attempts in model traces, and measures
latency to the first correct tool rather than the first tool of any kind.

`cache.tool_definition_signature_changes` is the #6986 check: the advertised
tools array must stay byte-identical across the model calls of one run. The
recorded LLM trace does not keep request bodies, so when a model base URL is
configured (`REBORN_WEBUI_V2_LIVE_QA_LLM_BASE_URL`, or
`LIVE_OPENAI_COMPATIBLE_BASE_URL` for a non-`nearai` provider) the runner puts a
loopback relay in front of it. The relay forwards requests and streamed
responses unchanged and keeps only a SHA-256 of each request's `tools` array; it
never stores headers or credentials. The metric counts how often that hash
changes between consecutive tool-bearing requests in one observation (0 is
compliant; null means no tool-bearing request was seen or the relay was off).
`--no-request-recorder` disables the relay.

`summary.json` aggregates each arm and catalog size: completion rate, latency,
leaks and failure categories, plus model turns, `tool_search`/`tool_describe`/
`result_read`/bridged `tool_call` counts, input and cached-input tokens,
time to the first correct tool, and tool-definition signature changes. Token
aggregates use only observations where the provider reported usage, and
`token_usage_observations` says how many that was.

The output contains per-observation JSONL, aggregate JSON, model traces, browser
diagnostics, and server logs. Provider token/cache fields are retained only when
the provider reports them. Zero or unavailable usage must not be replaced with
estimates derived from JSON bytes.
