Model-listing responses the parsers in `../models.rs` are tested against.

- `llama_cpp_v1_models.json`: a real `GET /v1/models` from a llama.cpp server (2026-09-29).
- `llama_cpp_props.json`: a real `GET /props` from the same llama.cpp server (build b11434, 2026-10-07), verbatim, chat template included.
- `anthropic_v1_models.json`: the example response in Anthropic's List Models docs (https://platform.claude.com/docs/en/api/models/list), verbatim. Its `max_input_tokens` of 0 is the docs' placeholder.
- `ollama_api_*.json`: the examples in Ollama's OpenAPI spec (https://docs.ollama.com/openapi.yaml) for `/api/tags`, `/api/show` (`model_info` trimmed) and `/api/ps`, converted from YAML.

A streamed `POST /v1/messages` reply that `../stream.rs` and `../../turn/tests.rs` are tested against (SME-112):

- `llama_cpp_messages_stream.sse`: llama.cpp's `llama-server` replying with thinking, then commentary, then a `todoread` call. It opens each block without closing the one before and sends every `content_block_stop` at the end. Built from llama.cpp's own serializer, not captured: `server_task_result_cmpl_partial::to_json_anthropic`, `server_task_result_cmpl_final::to_json_anthropic_stream` (`tools/server/server-task.cpp`) and `format_anthropic_sse` (`tools/server/server-common.cpp`) at master 448147d (2026-10-07): sorted keys, an `event:` and `data:` line per event, a thinking block's empty `signature_delta` just before its stop. Replace it with a real capture (`curl -sN <server>/v1/messages -H 'content-type: application/json' -d '{"model":"...","max_tokens":512,"stream":true,"tools":[{"name":"todoread","description":"Read the todo list","input_schema":{"type":"object","properties":{}}}],"messages":[{"role":"user","content":"Say what you will do, then read my todo list."}]}'`) and adjust the tests' expected text to it.
