//! Configuration file support for jcode
//!
//! Config is loaded from `~/.mona/config.toml` (or `$MONA_HOME/config.toml`)
//! Environment variables override config file settings.

pub use mona_config_types::{
    AgentsConfig, AmbientConfig, AuthConfig, AutoJudgeConfig, AutoReviewConfig, CompactionConfig,
    CompactionMode, CrossProviderFailoverMode, DiagramDisplayMode, DiagramPanePosition,
    DiffDisplayMode, DisplayConfig, FeatureConfig, GatewayConfig, HookCommands, HooksConfig,
    KeybindingsConfig, LatexRenderingMode, LaunchHotkeyEntry, LaunchHotkeysConfig,
    MarkdownSpacingMode, NamedProviderAuth, NamedProviderConfig, NamedProviderModelConfig,
    NamedProviderType, NativeScrollbarConfig, NotificationsConfig, OverscrollStatusMode,
    PowerConfig, ProviderConfig, ReasoningDisplayMode, SafetyConfig, SessionPickerResumeAction,
    SponsorsConfig, SwarmSpawnMode, SwarmStripLayout, TerminalConfig, UpdateChannel,
    WebSearchConfig, WebSearchEngine,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{LazyLock, RwLock};
use std::time::{Duration, Instant, SystemTime};

const CONFIG_CACHE_CHECK_INTERVAL: Duration = if cfg!(test) {
    Duration::ZERO
} else {
    Duration::from_millis(500)
};

const CONFIG_ENV_KEYS: &[&str] = &[
    "HOME",
    "MONA_ACP_PROFILE",
    "MONA_ACP_TOOL_PROFILE",
    "MONA_ACTIVE_SESSIONS_MANAGER",
    "MONA_EXTERNAL_SESSIONS",
    "MONA_AMBIENT_ENABLED",
    "MONA_AMBIENT_MAX_INTERVAL",
    "MONA_AMBIENT_MIN_INTERVAL",
    "MONA_AMBIENT_MODEL",
    "MONA_AMBIENT_PROACTIVE",
    "MONA_AMBIENT_PROVIDER",
    "MONA_AMBIENT_VISIBLE",
    "MONA_ANIMATION_FPS",
    "MONA_AUTO_POKE",
    "MONA_AUTOJUDGE_ENABLED",
    "MONA_AUTOJUDGE_MODEL",
    "MONA_AUTOREVIEW_ENABLED",
    "MONA_AUTOREVIEW_MODEL",
    "MONA_AUTO_POKE",
    "MONA_AUTO_SERVER_RELOAD",
    "MONA_CHECK_UPDATES",
    "MONA_BING_API_KEY",
    "MONA_BING_API_KEY_ENV",
    "MONA_BING_MARKET",
    "MONA_CENTERED_TOGGLE_KEY",
    "MONA_CHAT_NATIVE_SCROLLBAR",
    "MONA_COMPACT_NOTIFICATIONS",
    "MONA_COPY_BADGE_ALT_LABEL",
    "MONA_COPY_SELECTION_TOGGLE_KEY",
    "MONA_COPILOT_PREMIUM",
    "MONA_GEMINI_FORCE_OAUTH",
    "GOOGLE_CLOUD_PROJECT",
    "GOOGLE_CLOUD_PROJECT_ID",
    "MONA_WAKE_MODE",
    "MONA_CROSS_PROVIDER_FAILOVER",
    "MONA_DEBUG_SOCKET",
    "MONA_DEFAULT_REASONING_DISPLAY",
    "MONA_DICTATION_COMMAND",
    "MONA_DICTATION_KEY",
    "MONA_DICTATION_MODE",
    "MONA_DICTATION_TIMEOUT_SECS",
    "MONA_DIFF_LINE_WRAP",
    "MONA_DIFF_MODE",
    "MONA_DIFF_MODE_CYCLE_KEY",
    "MONA_DIAGRAM_PANE_TOGGLE_KEY",
    "MONA_DISABLE_BASE_TOOLS",
    "MONA_DISABLED_ANIMATIONS",
    "MONA_DISABLED_TOOLS",
    "MONA_DISCORD_BOT_TOKEN",
    "MONA_DISCORD_BOT_USER_ID",
    "MONA_DISCORD_CHANNEL_ID",
    "MONA_DISCORD_REPLY_ENABLED",
    "MONA_DISPLAY_CENTERED",
    "MONA_EFFORT_DECREASE_KEY",
    "MONA_EFFORT_INCREASE_KEY",
    "MONA_EMAIL_REPLY_ENABLED",
    "MONA_EMAIL_TO",
    "MONA_FOCUS_HOOK",
    "MONA_GATEWAY_BIND_ADDR",
    "MONA_GATEWAY_ENABLED",
    "MONA_GATEWAY_PORT",
    "MONA_HOME",
    "MONA_HOOK_PRE_TOOL",
    "MONA_HOOK_PRE_TOOL_TIMEOUT_MS",
    "MONA_HOOK_POST_TOOL",
    "MONA_HOOK_SESSION_END",
    "MONA_HOOK_SESSION_START",
    "MONA_HOOK_TURN_END",
    "MONA_HOOK_TURN_START",
    "MONA_IDLE_ANIMATION",
    "MONA_IMAP_HOST",
    "MONA_INFO_WIDGET_TOGGLE_KEY",
    "MONA_JADE_RELAY_API_BASE",
    "MONA_JADE_RELAY_ENABLED",
    "MONA_JADE_RELAY_LAUNCH_ENABLED",
    "MONA_JADE_RELAY_LAUNCH_WORKING_DIR",
    "MONA_JADE_RELAY_REPLY_ENABLED",
    "MONA_JADE_RELAY_SESSION_ID",
    "MONA_JADE_RELAY_TOKEN",
    "MONA_JADE_RELAY_TOKEN_ID",
    "MONA_JADE_RELAY_USER_ID",
    "MONA_KV_CACHE_MISS_NOTICES",
    "MONA_LATEX_RENDERING",
    "MONA_MARKDOWN_SPACING",
    "MONA_MEMORY_EMBEDDING_BACKEND",
    "MONA_MEMORY_EMBEDDING_BASE_URL",
    "MONA_MEMORY_EMBEDDING_DIM",
    "MONA_MEMORY_EMBEDDING_MODEL",
    "MONA_MEMORY_ENABLED",
    "MONA_ENABLE_MERMAID",
    "MONA_MEMORY_MODEL",
    "MONA_MEMORY_SIDECAR_ENABLED",
    "MONA_PERSIST_MEMORY_INJECTIONS",
    "MONA_MESSAGE_TIMESTAMPS",
    "MONA_MODEL",
    "MONA_MODEL_SWITCH_KEY",
    "MONA_MODEL_SWITCH_PREV_KEY",
    "MONA_MOUSE_CAPTURE",
    "MONA_NEW_TERMINAL_KEY",
    "MONA_NO_EMOJI",
    "MONA_NTFY_SERVER",
    "MONA_NTFY_TOPIC",
    "MONA_OPENAI_NATIVE_COMPACTION_MODE",
    "MONA_OPENAI_NATIVE_COMPACTION_THRESHOLD_TOKENS",
    "MONA_OPENAI_REASONING_EFFORT",
    "MONA_OPENAI_SERVICE_TIER",
    "MONA_OPENAI_TRANSPORT",
    "MONA_ANTHROPIC_REASONING_EFFORT",
    "MONA_PRESERVE_REASONING_CONTEXT",
    "MONA_PERFORMANCE",
    "MONA_PIN_IMAGES",
    "MONA_PIN_TODOS",
    "MONA_PREVENT_SLEEP_WHILE_STREAMING",
    "MONA_PROVIDER",
    "MONA_PROMPT_ENTRY_ANIMATION",
    "MONA_QUEUE_MODE",
    "MONA_REASONING_DISPLAY",
    "MONA_REDRAW_FPS",
    "MONA_SAME_PROVIDER_ACCOUNT_FAILOVER",
    "MONA_SCROLL_BOOKMARK_KEY",
    "MONA_SCROLL_DOWN_FALLBACK_KEY",
    "MONA_SCROLL_DOWN_KEY",
    "MONA_SCROLL_PAGE_DOWN_KEY",
    "MONA_SCROLL_PAGE_UP_KEY",
    "MONA_SCROLL_PROMPT_DOWN_KEY",
    "MONA_SCROLL_PROMPT_UP_KEY",
    "MONA_SCROLL_UP_FALLBACK_KEY",
    "MONA_SCROLL_UP_KEY",
    "MONA_SEARXNG_URL",
    "MONA_SHOW_AGENTGREP_OUTPUT",
    "MONA_SHOW_BASH_OUTPUT",
    "MONA_SHOW_DIFFS",
    "MONA_SHOW_THINKING",
    "MONA_SIDE_PANEL_TOGGLE_KEY",
    "MONA_SIDE_PANEL_NATIVE_SCROLLBAR",
    "MONA_SMTP_PASSWORD",
    "MONA_SPAWN_HOOK",
    "MONA_STREAM_IDLE_TIMEOUT_SECS",
    "MONA_MAX_RETRIES",
    "MONA_MCP_TOOLS",
    "MONA_MCP_TOOLS_TOKEN_THRESHOLD",
    "MONA_RETRY_BACKOFF_CAP_SECS",
    "MONA_SWARM_ENABLED",
    "MONA_SWARM_EFFORT",
    "MONA_SWARM_ROOT_EFFORT",
    "MONA_SWARM_DEEP_ROOT_EFFORT",
    "MONA_SWARM_MODEL",
    "MONA_SWARM_MAX_CONCURRENT_AGENTS",
    "MONA_SWARM_SPAWN_MODE",
    "MONA_SWARM_STRIP_LAYOUT",
    "MONA_TELEGRAM_BOT_TOKEN",
    "MONA_TELEGRAM_CHAT_ID",
    "MONA_TELEGRAM_REPLY_ENABLED",
    "MONA_TOOL_CALL_DETAILS",
    "MONA_TOOL_PROFILE",
    "MONA_TOOLS",
    "MONA_TRUSTED_EXTERNAL_AUTH_SOURCES",
    "MONA_TYPING_SCROLL_LOCK_TOGGLE_KEY",
    "MONA_UPDATE_CHANNEL",
    "MONA_WEBSEARCH_ENGINE",
    "MONA_WEBSEARCH_FALLBACK_ENGINES",
    "MONA_WORKSPACE_DOWN_KEY",
    "MONA_WORKSPACE_LEFT_KEY",
    "MONA_WORKSPACE_RIGHT_KEY",
    "MONA_WORKSPACE_UP_KEY",
    "XDG_CONFIG_HOME",
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfigCacheFingerprint {
    path: Option<PathBuf>,
    modified: Option<SystemTime>,
    len: Option<u64>,
    env: Vec<(String, String)>,
}

impl ConfigCacheFingerprint {
    fn current() -> Self {
        let path = Config::path();
        let metadata = path.as_ref().and_then(|path| std::fs::metadata(path).ok());
        Self {
            path,
            modified: metadata
                .as_ref()
                .and_then(|metadata| metadata.modified().ok()),
            len: metadata.as_ref().map(std::fs::Metadata::len),
            env: config_env_fingerprint(),
        }
    }
}

struct ConfigCache {
    config: &'static Config,
    fingerprint: ConfigCacheFingerprint,
    last_checked: Instant,
    force_reload: bool,
}

static CONFIG_CACHE: LazyLock<RwLock<ConfigCache>> = LazyLock::new(|| {
    let config = leak_config(Config::load());
    // Fingerprint after the load: applying env overrides may set env vars
    // (e.g. copilot_premium -> MONA_COPILOT_PREMIUM), and fingerprinting
    // first would guarantee a spurious full reload on the next check.
    let fingerprint = ConfigCacheFingerprint::current();
    // Seed the global context-limit cache from named provider configs on first
    // load so every codepath (TUI info widget, compaction budget, model
    // switching) sees user-configured `context_window` values from the start.
    // Read from the loaded config directly to avoid recursing into config(),
    // which would deadlock on the still-initializing CONFIG_CACHE.
    populate_context_limits_from_config_ref(config);
    RwLock::new(ConfigCache {
        config,
        fingerprint,
        last_checked: Instant::now(),
        force_reload: false,
    })
});

fn leak_config(config: Config) -> &'static Config {
    Box::leak(Box::new(config))
}

/// Seed the global context-limit cache from a config reference directly.
///
/// Used during CONFIG_CACHE initialization (where calling config() would
/// deadlock) and shares its logic with
/// `crate::provider::populate_context_limits_from_config`.
fn populate_context_limits_from_config_ref(cfg: &Config) {
    crate::provider::populate_context_limits_from_config_value(cfg);
}

/// Get the global config instance.
///
/// The returned reference is backed by a reloadable process cache. Calls check
/// the config file path/metadata and relevant environment overrides on a short
/// throttle, not every frame. When those inputs change, the next checked call
/// reloads config.toml and invalidates dependent auth/model caches. Older
/// references remain valid for the duration of any in-flight operation.
pub fn config() -> &'static Config {
    let now = Instant::now();
    if let Ok(cache) = CONFIG_CACHE.read()
        && !cache.force_reload
        && now.duration_since(cache.last_checked) < CONFIG_CACHE_CHECK_INTERVAL
    {
        return cache.config;
    }

    let mut reload_reason = None;
    let config = {
        let mut cache = CONFIG_CACHE
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let now = Instant::now();
        if !cache.force_reload
            && now.duration_since(cache.last_checked) < CONFIG_CACHE_CHECK_INTERVAL
        {
            return cache.config;
        }

        let fingerprint = ConfigCacheFingerprint::current();
        cache.last_checked = now;
        if cache.force_reload || cache.fingerprint != fingerprint {
            reload_reason = Some(describe_config_reload(
                cache.force_reload,
                &cache.fingerprint,
                &fingerprint,
            ));
            cache.config = leak_config(Config::load());
            // Loading applies env overrides that can themselves set env vars
            // (e.g. copilot_premium propagates config -> MONA_COPILOT_PREMIUM).
            // Re-fingerprint after the load so those self-inflicted env changes
            // don't trigger a guaranteed second reload on the next check.
            cache.fingerprint = ConfigCacheFingerprint::current();
            cache.force_reload = false;
        }
        cache.config
    };

    if let Some(reason) = reload_reason {
        crate::logging::info(&format!("CONFIG_RELOAD {}", reason));
        // A config reload can change config-derived system prompt sections
        // (feature toggles, sponsors, ...), which legitimately invalidates the
        // KV cache prefix of warm sessions. Document it so a subsequent
        // harness-attributed cache miss is surfaced with this cause instead of
        // as an unexplained prompt mutation.
        crate::cache_invalidation::record("config reload", &reason);
        notify_config_reloaded();
        // Re-seed the global context-limit cache so user edits to named
        // provider `context_window` values take effect without a restart.
        crate::provider::populate_context_limits_from_config();
    }

    config
}

fn describe_config_reload(
    forced: bool,
    previous: &ConfigCacheFingerprint,
    next: &ConfigCacheFingerprint,
) -> String {
    let mut parts = Vec::new();
    if forced {
        parts.push("forced=true".to_string());
    }
    if previous.path != next.path {
        parts.push(format!(
            "path={:?}->{:?}",
            previous.path.as_ref().map(|p| p.display().to_string()),
            next.path.as_ref().map(|p| p.display().to_string())
        ));
    }
    if previous.modified != next.modified {
        parts.push("modified_changed=true".to_string());
    }
    if previous.len != next.len {
        parts.push(format!("len={:?}->{:?}", previous.len, next.len));
    }
    let env_changes = describe_env_changes(&previous.env, &next.env);
    if !env_changes.is_empty() {
        parts.push(format!("env=[{}]", env_changes.join(", ")));
    }
    if parts.is_empty() {
        "unchanged".to_string()
    } else {
        parts.join(" ")
    }
}

fn describe_env_changes(previous: &[(String, String)], next: &[(String, String)]) -> Vec<String> {
    let previous_map: BTreeMap<&str, &str> = previous
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let next_map: BTreeMap<&str, &str> = next
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let keys: BTreeSet<&str> = previous_map
        .keys()
        .chain(next_map.keys())
        .copied()
        .collect();

    keys.into_iter()
        .filter_map(|key| match (previous_map.get(key), next_map.get(key)) {
            (Some(previous), Some(next)) if previous != next => Some(format!(
                "{}:changed({}->{})",
                key,
                env_value_fingerprint(previous),
                env_value_fingerprint(next)
            )),
            (None, Some(next)) => Some(format!("{}:added({})", key, env_value_fingerprint(next))),
            (Some(previous), None) => Some(format!(
                "{}:removed({})",
                key,
                env_value_fingerprint(previous)
            )),
            _ => None,
        })
        .collect()
}

fn env_value_fingerprint(value: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    format!("len:{} hash:{:016x}", value.len(), hasher.finish())
}

fn config_env_fingerprint() -> Vec<(String, String)> {
    let mut values = std::env::vars_os()
        .filter_map(|(key, value)| {
            let key = key.to_string_lossy().to_string();
            if CONFIG_ENV_KEYS.contains(&key.as_str()) {
                Some((key, value.to_string_lossy().to_string()))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    values.sort_by(|left, right| left.0.cmp(&right.0));
    values
}

pub fn invalidate_config_cache() {
    let mut cache = CONFIG_CACHE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.force_reload = true;
    drop(cache);
    notify_config_reloaded();
}

fn notify_config_reloaded() {
    CONFIG_RELOAD_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    for listener in CONFIG_RELOAD_LISTENERS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
    {
        listener();
    }
}

/// Monotonic counter bumped every time the config cache reloads.
///
/// Callers that snapshot config-derived state (e.g. the TUI's parsed
/// keybindings) can poll this cheaply and re-derive their snapshot when the
/// generation changes, giving instant hot-reload of config edits without a
/// restart.
static CONFIG_RELOAD_GENERATION: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Current config reload generation. Increments after every cache reload.
pub fn config_reload_generation() -> u64 {
    CONFIG_RELOAD_GENERATION.load(std::sync::atomic::Ordering::Relaxed)
}

/// Listeners invoked after the config cache reloads.
///
/// Config is a foundational module, so instead of reaching up into higher-level
/// subsystems (auth cache, event bus) on reload, those subsystems register a
/// reaction here at startup. This keeps config free of upward dependencies and
/// breaks the config -> auth / config -> bus cycle edges.
/// Type of a config reload listener callback.
type ConfigReloadListener = fn();

static CONFIG_RELOAD_LISTENERS: LazyLock<RwLock<Vec<ConfigReloadListener>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a callback to run after the config cache reloads.
///
/// Callbacks must be cheap and non-blocking; they run on whichever thread
/// triggers the reload. Intended to be called once per subsystem during
/// process startup.
pub fn on_config_reloaded(listener: fn()) {
    CONFIG_RELOAD_LISTENERS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(listener);
}

/// Main configuration struct
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    /// Daemon behavior for autonomous wake requests.
    pub server: ServerConfig,

    /// Keybinding configuration
    pub keybindings: KeybindingsConfig,

    /// External dictation / speech-to-text integration
    pub dictation: DictationConfig,

    /// Display/UI configuration
    pub display: DisplayConfig,

    /// Feature toggles
    pub features: FeatureConfig,

    /// Web search tool configuration
    pub websearch: WebSearchConfig,

    /// Built-in tool exposure configuration
    pub tools: ToolConfig,

    /// Agent Client Protocol adapter configuration
    pub acp: AcpConfig,

    /// Auth trust / consent configuration
    pub auth: AuthConfig,

    /// Provider configuration
    pub provider: ProviderConfig,

    /// Named provider profiles, keyed by profile name.
    ///
    /// Example:
    /// [providers.my-gateway]
    /// type = "openai-compatible"
    /// base_url = "https://llm.example.com/v1"
    /// api_key_env = "MY_GATEWAY_API_KEY"
    pub providers: BTreeMap<String, NamedProviderConfig>,

    /// Agent-specific model defaults
    pub agents: AgentsConfig,

    /// Terminal window/pane spawning configuration
    pub terminal: TerminalConfig,

    /// Lifecycle hooks (external commands at turn/session/tool boundaries)
    pub hooks: HooksConfig,

    /// Ambient mode configuration
    pub ambient: AmbientConfig,

    /// Safety / notification configuration
    pub safety: SafetyConfig,

    /// Desktop notifications for interactive sessions (e.g. turn completion)
    pub notifications: NotificationsConfig,

    /// WebSocket gateway configuration (for iOS/web clients)
    pub gateway: GatewayConfig,

    /// Compaction configuration
    pub compaction: CompactionConfig,

    /// Power-management configuration (prevent sleep while streaming)
    pub power: PowerConfig,

    /// Auto-review configuration
    pub autoreview: AutoReviewConfig,

    /// Auto-judge configuration
    pub autojudge: AutoJudgeConfig,

    /// Partner discovery configuration. Skipped when it matches the shipped
    /// default so saving config never bakes today's default into the file (see
    /// [`sponsors_is_default`]).
    #[serde(skip_serializing_if = "sponsors_is_default")]
    pub sponsors: SponsorsConfig,

    /// Global "launch a new jcode" hotkeys (macOS). Baked once by auto-import.
    pub launch_hotkeys: LaunchHotkeysConfig,
}

/// Controls who owns autonomous wake execution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WakeMode {
    /// The daemon starts idle turns and interrupts running turns itself.
    #[default]
    Internal,
    /// The daemon emits a wake request and leaves turn scheduling to its operator.
    External,
}

impl WakeMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "internal" => Some(Self::Internal),
            "external" => Some(Self::External),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Ownership model for autonomous wake requests.
    pub wake_mode: WakeMode,
}

/// Agent Client Protocol adapter configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AcpConfig {
    /// Client compatibility profile: "standard" (default), "extended", or "full".
    pub profile: String,
    /// Tool profile to request when `jcode acp` starts a daemon itself.
    pub tool_profile: String,
}

impl Default for AcpConfig {
    fn default() -> Self {
        Self {
            profile: "standard".to_string(),
            tool_profile: "acp".to_string(),
        }
    }
}

/// Controls how MCP server tools are exposed to the model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpToolsMode {
    /// Expose individual tools until their serialized definitions exceed the
    /// configured threshold, then use the fixed search/call surface.
    #[default]
    Auto,
    /// Always expose every MCP server tool as a top-level tool definition.
    Eager,
    /// Expose only the fixed `mcp_search` and `mcp_call` tools.
    Deferred,
}

impl McpToolsMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Eager => "eager",
            Self::Deferred => "deferred",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "eager" => Some(Self::Eager),
            "deferred" => Some(Self::Deferred),
            _ => None,
        }
    }
}

/// Controls which tools are sent to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolConfig {
    /// Tool profile: "full" (default), "acp", "minimal"/"lite", or "none".
    pub profile: String,
    /// Explicit allow-list. When set, only these tools are exposed.
    /// Use "*" or "all" to expose all tools without an allow-list.
    pub enabled: Vec<String>,
    /// Tools to remove after applying profile/enabled.
    pub disabled: Vec<String>,
    /// Disable all built-in tools unless `enabled` is provided.
    pub disable_base_tools: bool,
    /// MCP tool exposure mode: auto (default), eager, or deferred.
    pub mcp_tools: McpToolsMode,
    /// In auto mode, defer MCP tools when their definitions exceed this token estimate.
    #[serde(
        alias = "mcp_tools_threshold",
        alias = "mcp_tools_auto_threshold",
        alias = "mcp_tools_auto_threshold_tokens"
    )]
    pub mcp_tools_token_threshold: usize,
}

impl Default for ToolConfig {
    fn default() -> Self {
        Self {
            profile: String::new(),
            enabled: Vec::new(),
            disabled: Vec::new(),
            disable_base_tools: false,
            mcp_tools: McpToolsMode::Auto,
            mcp_tools_token_threshold: 8_000,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolSelection {
    pub allowed_tools: Option<HashSet<String>>,
    pub disabled_tools: HashSet<String>,
}

impl ToolConfig {
    pub fn selection(&self) -> ToolSelection {
        let mut allowed_tools = self.base_allowed_tools();
        let disabled_tools: HashSet<String> = self
            .disabled
            .iter()
            .map(|name| normalize_tool_name(name))
            .filter(|name| !name.is_empty())
            .collect();

        if let Some(allowed) = allowed_tools.as_mut() {
            for name in &disabled_tools {
                allowed.remove(name);
            }
        }

        ToolSelection {
            allowed_tools,
            disabled_tools,
        }
    }

    pub fn allowed_tools(&self) -> Option<HashSet<String>> {
        self.selection().allowed_tools
    }

    pub fn apply_to_allowed_set(&self, allowed: &mut HashSet<String>) {
        let selection = self.selection();
        if let Some(global_allowed) = selection.allowed_tools {
            allowed.retain(|name| global_allowed.contains(name));
        }
        for disabled in selection.disabled_tools {
            allowed.remove(&disabled);
        }
    }

    fn base_allowed_tools(&self) -> Option<HashSet<String>> {
        let (explicit, enables_all_tools) = self.normalized_enabled_tools();

        let profile = self.profile.trim().to_ascii_lowercase();
        if enables_all_tools {
            None
        } else if !explicit.is_empty() {
            Some(explicit)
        } else if self.disable_base_tools || matches!(profile.as_str(), "none" | "off" | "disabled")
        {
            Some(HashSet::new())
        } else if matches!(profile.as_str(), "acp") {
            Some(
                [
                    "bash",
                    "read",
                    "write",
                    "edit",
                    "multiedit",
                    "apply_patch",
                    "patch",
                    "agentgrep",
                    "ls",
                    "batch",
                    "mcp",
                ]
                .into_iter()
                .map(|name| name.to_string())
                .collect(),
            )
        } else if matches!(profile.as_str(), "minimal" | "lite" | "small") {
            Some(
                [
                    "bash",
                    "read",
                    "write",
                    "edit",
                    "multiedit",
                    "apply_patch",
                    "patch",
                    "agentgrep",
                    "ls",
                ]
                .into_iter()
                .map(|name| name.to_string())
                .collect(),
            )
        } else {
            None
        }
    }

    fn normalized_enabled_tools(&self) -> (HashSet<String>, bool) {
        let mut enabled = HashSet::new();
        let mut enables_all_tools = false;

        for name in &self.enabled {
            let normalized = normalize_tool_name(name);
            if normalized.is_empty() {
                continue;
            }
            if normalized == "*" || normalized.eq_ignore_ascii_case("all") {
                enables_all_tools = true;
            } else {
                enabled.insert(normalized);
            }
        }

        (enabled, enables_all_tools)
    }
}

fn normalize_tool_name(name: &str) -> String {
    let trimmed = name.trim().trim_matches('"');
    mona_tool_types::resolve_tool_name(trimmed).to_string()
}

/// External dictation / speech-to-text integration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DictationConfig {
    /// Shell command to run. Must print the transcript to stdout.
    pub command: String,
    /// How to apply the resulting transcript.
    pub mode: crate::protocol::TranscriptMode,
    /// Optional in-app hotkey to trigger dictation.
    pub key: String,
    /// Maximum time to wait for the command to finish (0 = no timeout).
    pub timeout_secs: u64,
}

impl Default for DictationConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            mode: crate::protocol::TranscriptMode::Send,
            key: "off".to_string(),
            timeout_secs: 90,
        }
    }
}

pub mod change_report;
mod config_file;
mod default_file;
mod display_summary;
mod env_overrides;

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "config_color_tests.rs"]
mod color_tests;

/// Whether integration discovery settings carry no information beyond the shipped
/// default, so `[sponsors]` can be left out of written config files.
///
/// Discovery originally shipped opt-in with `enabled = false`, and because
/// config saves serialize the whole struct, any save during that window froze
/// the old default into the user's file and permanently disabled discovery even
/// after the default flipped. Omitting default sections prevents a repeat.
fn sponsors_is_default(sponsors: &SponsorsConfig) -> bool {
    sponsors.enabled && is_default_discovery_endpoint(&sponsors.endpoint)
}

/// Endpoints used by shipped defaults. These may also be explicit user choices.
fn is_default_discovery_endpoint(endpoint: &str) -> bool {
    matches!(
        endpoint.trim_end_matches('/'),
        "https://api.jcode.sh/v1/discovery" | "https://api.solosystems.dev/v1/discovery"
    )
}
