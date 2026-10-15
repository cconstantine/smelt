-- Whether a model takes adaptive thinking, as Anthropic's model listing
-- reports it (capabilities.thinking.types.adaptive.supported). Haiku 4.5
-- and older models refuse {"type": "adaptive"} and are sent a thinking
-- budget instead (B9, SME-122). Null: not reported, adaptive assumed.
ALTER TABLE provider_models
    ADD COLUMN reported_adaptive_thinking BOOLEAN;
