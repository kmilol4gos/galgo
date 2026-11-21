//! Las estructuras de serde solo conservan números, enums y hashes de IDs; ignoran contenido.
use serde::{de::{self, Visitor}, Deserialize, Deserializer};
use std::{fmt, hash::{Hash, Hasher}, collections::hash_map::DefaultHasher};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Model { ClaudeOpus55, Gpt56Sol, Gpt56Terra, Gpt56Luna, Gpt55, Gpt54, Gpt62Sol, Gpt62Astra, Unknown }
impl Model {
    pub fn name(self) -> Option<&'static str> { Some(match self {
        Self::ClaudeOpus55 => "claude-opus-5-5", Self::Gpt56Sol => "gpt-5.6-sol", Self::Gpt56Terra => "gpt-5.6-terra", Self::Gpt56Luna => "gpt-5.6-luna", Self::Gpt55 => "gpt-5.5", Self::Gpt54 => "gpt-5.4", Self::Gpt62Sol => "gpt-6.2-sol", Self::Gpt62Astra => "gpt-6.2-astra", Self::Unknown => return None,
    }) }
}
impl<'de> Deserialize<'de> for Model {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Model;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result { f.write_str("modelo") }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<Model, E> { Ok(match s {
                "claude-opus-5-5" => Model::ClaudeOpus55, "gpt-5.6-sol" => Model::Gpt56Sol, "gpt-5.6-terra" => Model::Gpt56Terra, "gpt-5.6-luna" => Model::Gpt56Luna, "gpt-5.5" => Model::Gpt55, "gpt-5.4" => Model::Gpt54, "gpt-6.2-sol" => Model::Gpt62Sol, "gpt-6.2-astra" => Model::Gpt62Astra, _ => Model::Unknown,
            }) }
        }
        d.deserialize_str(V)
    }
}
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub struct Id(pub u64);
impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Id;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result { f.write_str("identificador") }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<Id,E> { let mut h = DefaultHasher::new(); s.hash(&mut h); Ok(Id(h.finish())) }
        }
        d.deserialize_str(V)
    }
}
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all="snake_case")]
pub enum Kind { Assistant, Message, EventMsg, TokenCount, TurnContext, #[serde(other)] Other }
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all="lowercase")]
pub enum Role { Assistant, #[serde(other)] Other }
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all="lowercase")]
pub enum Plan { Plus, Pro, Team, Enterprise, Free, #[serde(other)] Other }
#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub struct ClaudeCache { #[serde(default)] pub ephemeral_5m_input_tokens: u64, #[serde(default)] pub ephemeral_1h_input_tokens: u64 }
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all="lowercase")]
pub enum Speed { Standard, #[serde(other)] Other }
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all="lowercase")]
pub enum Geo { Global, #[serde(other)] Other }
#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub struct ServerTools { #[serde(default)] pub web_search_requests: u64, #[serde(default)] pub web_fetch_requests: u64 }
#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub struct ClaudeOutputDetails { #[serde(default)] pub thinking_tokens: u64 }
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct ClaudeUsage {
    pub input_tokens: u64, pub output_tokens: u64,
    #[serde(default)] pub cache_read_input_tokens: u64,
    #[serde(default)] pub cache_creation_input_tokens: u64,
    #[serde(default)] pub cache_creation: Option<ClaudeCache>,
    #[serde(default)] pub output_tokens_details: Option<ClaudeOutputDetails>,
    #[serde(default)] pub speed: Option<Speed>,
    #[serde(default)] pub inference_geo: Option<Geo>,
    #[serde(default)] pub server_tool_use: Option<ServerTools>,
}
#[derive(Debug, Deserialize)]
pub struct ClaudeMessage { pub id: Option<Id>, pub model: Option<Model>, pub usage: Option<ClaudeUsage> }
#[derive(Debug, Deserialize)]
pub struct ClaudeLine {
    #[serde(rename="type")] pub kind: Kind,
    #[serde(rename="isSidechain")] pub sidechain: Option<bool>,
    #[serde(rename="requestId")] pub request_id: Option<Id>,
    pub message: Option<ClaudeMessage>,
}
#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub struct CodexUsage {
    pub input_tokens: u64, #[serde(default)] pub cached_input_tokens: u64,
    #[serde(default)] pub cache_write_input_tokens: u64,
    pub output_tokens: u64, #[serde(default)] pub reasoning_output_tokens: u64,
}
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct CodexInfo { pub total_token_usage: Option<CodexUsage>, pub last_token_usage: Option<CodexUsage>, pub model_context_window: Option<u64> }
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct Window { pub used_percent: Option<f32>, pub window_minutes: Option<u64>, pub resets_at: Option<u64> }
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct RateLimits { pub plan_type: Option<Plan>, pub primary: Option<Window>, pub secondary: Option<Window> }
#[derive(Debug, Deserialize)]
pub struct Payload { #[serde(rename="type")] pub kind: Option<Kind>, pub model: Option<Model>, pub info: Option<CodexInfo>, pub rate_limits: Option<RateLimits> }
#[derive(Debug, Deserialize)]
pub struct CodexLine { #[serde(rename="type")] pub kind: Kind, pub payload: Option<Payload> }
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct PiCost { pub total: Option<f64>, #[serde(rename="totalFromProvider")] pub total_from_provider: Option<bool> }
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct PiUsage { pub input: u64, pub output: u64, #[serde(default)] pub reasoning: u64, #[serde(rename="cacheRead", default)] pub cache_read: u64, #[serde(rename="cacheWrite", default)] pub cache_write: u64, pub cost: Option<PiCost> }
#[derive(Debug, Deserialize)]
pub struct PiMessage { pub role: Role, pub model: Option<Model>, pub usage: Option<PiUsage> }
#[derive(Debug, Deserialize)]
pub struct PiLine { #[serde(rename="type")] pub kind: Kind, pub message: Option<PiMessage> }

pub fn contains(line: &[u8], pattern: &[u8]) -> bool { line.windows(pattern.len()).any(|w| w == pattern) }
