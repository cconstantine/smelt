//! The model providers pages (`/providers`, SME-72) and the model picker
//! above a conversation's message box.

use dioxus::prelude::*;

use crate::api::chat::{get_conversation_model, set_conversation_model};
use crate::api::providers::{
    add_provider_model, create_provider, delete_provider, get_default_model, get_provider, get_recent_spend, list_price_sources,
    list_provider_models, list_providers, probe_llama_cpp, refresh_model_details, set_default_model, set_model_settings,
    update_provider,
};
use crate::frontend::Route;
use crate::providers::{
    suggest_price_source, AuthKind, ConversationModel, LlamaServerInfo, ModelChoice, ModelInfo, ModelSettings, PriceSource, ProviderInput,
    ProviderKind, ProviderSummary, ASSUMED_CONTEXT_WINDOW, LLAMA_CPP_EFFORTS,
};

use super::server_error_message;

#[cfg(test)]
mod tests;

mod picker;
mod index;
mod form;
mod models;

pub use picker::*;
pub use index::*;
pub use form::*;
pub use models::*;
