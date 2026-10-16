use super::*;

fn message_with_blocks(id: i64, role: &str, blocks: Vec<ContentBlock>) -> Message {
    Message {
        id,
        conversation_id: 1,
        role: role.to_string(),
        content: serde_json::to_string(&blocks).expect("ContentBlock always serializes"),
        created_at: chrono::Utc::now().naive_utc(),
    }
}

fn test_message(id: i64) -> Message {
    Message {
        id,
        conversation_id: 1,
        role: "user".to_string(),
        content: r#"[{"type":"text","text":"hi"}]"#.to_string(),
        created_at: chrono::Utc::now().naive_utc(),
    }
}

fn user_text(id: i64, text: &str) -> Message {
    Message {
        id,
        conversation_id: 1,
        role: "user".to_string(),
        content: serde_json::to_string(&[ContentBlock::Text { text: text.to_string() }])
            .expect("serializes"),
        created_at: chrono::Utc::now().naive_utc(),
    }
}

fn no_mods() -> keyboard_types::Modifiers {
    keyboard_types::Modifiers::empty()
}

fn test_sandbox_terminal_entry(terminal_id: i64, pod_id: i64) -> SandboxTerminalPanelEntry {
    SandboxTerminalPanelEntry {
        terminal_id,
        pod_id,
        status: "connected".to_string(),
        commands: Vec::new(),
    }
}

fn test_sandbox_command_entry(command_id: &str, command: &str) -> SandboxCommandPanelEntry {
    SandboxCommandPanelEntry {
        command_id: command_id.to_string(),
        command: command.to_string(),
        status: "running".to_string(),
        exit_code: None,
        output: Vec::new(),
    }
}

fn preview(port: u16) -> SandboxPreview {
    SandboxPreview {
        port,
        host: None,
        url: format!("http://{port}-1.preview.localhost:8181"),
    }
}

mod formatting;
mod messages;
mod browser_input;
mod notices;
mod sandbox;
mod commands;
mod events;
mod reply_parts;
mod indicators;
