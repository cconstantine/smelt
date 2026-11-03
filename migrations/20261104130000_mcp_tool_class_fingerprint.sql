-- SME-295: an override remembers which tool it was set for. The fingerprint
-- of the tool the server listed under that name when the user saved it
-- (`mcp::tool_fingerprint`: its description and input schema); a read whose
-- listed tool no longer matches it ignores the override, and the server's own
-- class applies until the user saves it again. NULL for an override saved
-- before this column, or for a tool the server didn't list at the save: those
-- apply as before.
ALTER TABLE mcp_tool_classes ADD COLUMN tool_fingerprint TEXT;
