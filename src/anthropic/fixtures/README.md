Model-listing responses the parsers in `../models.rs` are tested against.

- `llama_cpp_v1_models.json`: a real `GET /v1/models` from a llama.cpp server (2026-09-29).
- `anthropic_v1_models.json`: the example response in Anthropic's List Models docs (https://platform.claude.com/docs/en/api/models/list), verbatim. Its `max_input_tokens` of 0 is the docs' placeholder.
- `ollama_api_*.json`: the examples in Ollama's OpenAPI spec (https://docs.ollama.com/openapi.yaml) for `/api/tags`, `/api/show` (`model_info` trimmed) and `/api/ps`, converted from YAML.
