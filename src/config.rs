#![allow(dead_code)]

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CompositorConfig {
    pub(crate) visuals: VisualConfig,
    pub(crate) blur_enabled: bool,
    pub(crate) animation: AnimationConfig,
    pub(crate) rules: Arc<[WindowRule]>,
}

/// 3a3fa2b1/b2-r2/b3/b4-r1/b6-r1/b7 — user-selectable open-animation
/// effect. `Scale`, `Teleport`, `EnergyTear`, `Bubble`, `TeleportFlashy`,
/// and `Kamui` exist so far; the parser must reject any other token
/// (including future effect names) with a clear config error rather than
/// accept a non-functional effect — see `resolve_animation`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OpenAnimationEffect {
    Scale,
    Teleport,
    EnergyTear,
    Bubble,
    TeleportFlashy,
    Kamui,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct OpenAnimationConfig {
    pub(crate) effect: OpenAnimationEffect,
    pub(crate) duration: Duration,
}

/// 3a3fa2b5/b6-r1/b7 — separate enum from `OpenAnimationEffect` on
/// purpose: close-side lifecycle state must never semantically pretend
/// to be an open animation (see `ClosingAnimation` in scene.rs). `Scale`,
/// `TeleportFlashy`, and `Kamui` exist so far — `teleport`/`energy_tear`/
/// `bubble` remain explicitly out of scope as CLOSE effects for this
/// milestone (they remain valid OPEN effects, unrelated).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CloseAnimationEffect {
    Scale,
    TeleportFlashy,
    Kamui,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CloseAnimationConfig {
    pub(crate) enabled: bool,
    pub(crate) effect: CloseAnimationEffect,
    pub(crate) duration: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct AnimationConfig {
    pub(crate) enabled: bool,
    pub(crate) open: OpenAnimationConfig,
    pub(crate) close: CloseAnimationConfig,
}

impl Default for AnimationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            open: OpenAnimationConfig {
                effect: OpenAnimationEffect::Scale,
                duration: Duration::from_millis(180),
            },
            close: CloseAnimationConfig {
                enabled: false,
                effect: CloseAnimationEffect::Scale,
                duration: Duration::from_millis(180),
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConfigPathEnvironment {
    pub(crate) home: Option<String>,
    pub(crate) xdg_config_home: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ConfigPathError {
    NoUsableHome,
}

impl fmt::Display for ConfigPathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoUsableHome => write!(formatter, "neither XDG_CONFIG_HOME nor HOME is usable"),
        }
    }
}

impl std::error::Error for ConfigPathError {}

pub(crate) fn resolve_config_path(
    explicit: Option<&Path>,
    environment: &ConfigPathEnvironment,
) -> Result<PathBuf, ConfigPathError> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    if let Some(xdg) = non_empty(environment.xdg_config_home.as_deref()) {
        return Ok(PathBuf::from(xdg).join("xomposite/xomposite.conf"));
    }
    if let Some(home) = non_empty(environment.home.as_deref()) {
        return Ok(PathBuf::from(home).join(".config/xomposite/xomposite.conf"));
    }
    Err(ConfigPathError::NoUsableHome)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StartupConfigRequest {
    pub(crate) explicit_path: Option<PathBuf>,
    pub(crate) environment: ConfigPathEnvironment,
}

impl ConfigPathEnvironment {
    pub(crate) fn from_process() -> Self {
        Self {
            home: std::env::var("HOME").ok(),
            xdg_config_home: std::env::var("XDG_CONFIG_HOME").ok(),
        }
    }
}

#[derive(Debug)]
pub(crate) enum ConfigLoadError {
    ExplicitPathMissing(PathBuf),
    Read { path: PathBuf, source: std::io::Error },
    Parse { path: PathBuf, source: ParseError },
    Validate { path: PathBuf, source: ParseError },
}

impl fmt::Display for ConfigLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExplicitPathMissing(path) => write!(formatter, "config file not found: {}", path.display()),
            Self::Read { path, source } => write!(formatter, "could not read {}: {source}", path.display()),
            Self::Parse { path, source } => write!(formatter, "invalid config {}: {source}", path.display()),
            Self::Validate { path, source } => write!(formatter, "invalid config {}: {source}", path.display()),
        }
    }
}

impl std::error::Error for ConfigLoadError {}

#[derive(Debug)]
pub(crate) enum ConfigLoadOutcome {
    DefaultsBecauseMissingImplicitFile,
    Loaded { path: PathBuf, config: Arc<ValidatedConfig> },
}

pub(crate) fn load_startup_config(request: StartupConfigRequest) -> Result<ConfigLoadOutcome, ConfigLoadError> {
    let explicit = request.explicit_path.is_some();
    let path = match resolve_config_path(request.explicit_path.as_deref(), &request.environment) {
        Ok(path) => path,
        Err(ConfigPathError::NoUsableHome) if !explicit => {
            return Ok(ConfigLoadOutcome::DefaultsBecauseMissingImplicitFile);
        }
        Err(error) => return Err(ConfigLoadError::Read { path: PathBuf::new(), source: std::io::Error::new(std::io::ErrorKind::InvalidInput, error) }),
    };
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound && !explicit => {
            return Ok(ConfigLoadOutcome::DefaultsBecauseMissingImplicitFile);
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Err(ConfigLoadError::ExplicitPathMissing(path));
        }
        Err(source) => return Err(ConfigLoadError::Read { path, source }),
    };
    let parsed = ParsedConfig::parse(&contents).map_err(|source| ConfigLoadError::Parse { path: path.clone(), source })?;
    let config = parsed.validate().map_err(|source| ConfigLoadError::Validate { path: path.clone(), source })?;
    Ok(ConfigLoadOutcome::Loaded { path, config: Arc::new(config) })
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ParseError {
    pub(crate) line: usize,
    pub(crate) section: Option<String>,
    pub(crate) message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.section {
            Some(section) => write!(formatter, "line {} [{}]: {}", self.line, section, self.message),
            None => write!(formatter, "line {}: {}", self.line, self.message),
        }
    }
}

impl std::error::Error for ParseError {}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ParsedConfig {
    pub(crate) global: ParsedGlobalConfig,
    pub(crate) rules: Vec<ParsedRule>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ParsedGlobalConfig {
    pub(crate) corner_radius: Option<f32>,
    pub(crate) shadow_enabled: Option<bool>,
    pub(crate) shadow_color: Option<String>,
    pub(crate) shadow_extent: Option<f32>,
    pub(crate) shadow_offset_x: Option<f32>,
    pub(crate) shadow_offset_y: Option<f32>,
    pub(crate) shadow_strength: Option<f32>,
    pub(crate) border_width: Option<f32>,
    pub(crate) border_inactive_color: Option<String>,
    pub(crate) border_focused_color: Option<String>,
    pub(crate) border_urgent_color: Option<String>,
    pub(crate) opacity_focused: Option<f32>,
    pub(crate) opacity_inactive: Option<f32>,
    pub(crate) opacity_urgent: Option<f32>,
    pub(crate) blur_enabled: Option<bool>,
    pub(crate) animation_enabled: Option<bool>,
    pub(crate) animation_open_effect: Option<String>,
    pub(crate) animation_open_duration: Option<f32>,
    pub(crate) animation_close_enabled: Option<bool>,
    pub(crate) animation_close_effect: Option<String>,
    pub(crate) animation_close_duration: Option<f32>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ParsedRule {
    pub(crate) name: String,
    pub(crate) class: Option<String>,
    pub(crate) instance: Option<String>,
    pub(crate) window_type: Option<String>,
    pub(crate) blur: Option<bool>,
    pub(crate) shadow: Option<bool>,
    pub(crate) frame: Option<FrameRuleAction>,
    pub(crate) opacity: Option<f32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FrameRuleAction {
    Default,
    Request,
    Suppress,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WindowType {
    Normal,
    Dock,
    Desktop,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RuleMatch {
    pub(crate) class: Option<String>,
    pub(crate) instance: Option<String>,
    pub(crate) window_type: Option<WindowType>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RuleActions {
    pub(crate) blur: Option<bool>,
    pub(crate) shadow: Option<bool>,
    pub(crate) frame: Option<FrameRuleAction>,
    pub(crate) opacity: Option<f32>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WindowRule {
    pub(crate) name: String,
    pub(crate) matcher: RuleMatch,
    pub(crate) actions: RuleActions,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ValidatedConfig {
    pub(crate) visuals: VisualConfig,
    pub(crate) blur_enabled: bool,
    pub(crate) rules: Vec<WindowRule>,
    pub(crate) animation: AnimationConfig,
}

impl Default for ValidatedConfig {
    fn default() -> Self {
        let mut visuals = VisualConfig::default();
        visuals.shadow = ShadowConfig {
            enabled: false,
            color: [0, 0, 0],
            offset_x: -3.0,
            offset_y: 4.0,
            extent: 18.0,
            strength: 0.28,
        };
        Self { visuals, blur_enabled: true, rules: Vec::new(), animation: AnimationConfig::default() }
    }
}

impl ParsedConfig {
    pub(crate) fn parse(input: &str) -> Result<Self, ParseError> {
        let mut parsed = Self::default();
        let mut section = Section::None;
        let mut seen = std::collections::HashSet::new();
        for (index, raw_line) in input.lines().enumerate() {
            let line_number = index + 1;
            let line = raw_line.trim();
            if line.starts_with('#') { continue; }
            let line = line.trim();
            if line.is_empty() { continue; }
            if line == "[global]" {
                section = Section::Global;
                continue;
            }
            if let Some(name) = line.strip_prefix("[rule ").and_then(|value| value.strip_suffix(']')) {
                let name = quoted(name, line_number, None)?;
                parsed.rules.push(ParsedRule { name, ..ParsedRule::default() });
                section = Section::Rule(parsed.rules.len() - 1);
                seen.clear();
                continue;
            }
            if line.starts_with('[') {
                return Err(error(line_number, None, "unknown section"));
            }
            let (key, value) = line.split_once('=').ok_or_else(|| error(line_number, section.name(), "expected key = value"))?;
            let key = key.trim();
            let value = value.trim();
            if key.is_empty() || value.is_empty() {
                return Err(error(line_number, section.name(), "expected non-empty key and value"));
            }
            if !seen.insert(key.to_owned()) {
                return Err(error(line_number, section.name(), "duplicate key"));
            }
            match section {
                Section::None => return Err(error(line_number, None, "key is outside a section")),
                Section::Global => parse_global(&mut parsed.global, key, value, line_number)?,
                Section::Rule(rule) => parse_rule(&mut parsed.rules[rule], key, value, line_number)?,
            }
        }
        Ok(parsed)
    }

    pub(crate) fn validate(self) -> Result<ValidatedConfig, ParseError> {
        let mut config = ValidatedConfig::default();
        let global = self.global;
        config.visuals.corner_radius = global.corner_radius.unwrap_or(config.visuals.corner_radius);
        let shadow = config.visuals.shadow;
        config.visuals.shadow = ShadowConfig {
            enabled: global.shadow_enabled.unwrap_or(shadow.enabled),
            color: global.shadow_color.as_deref().map(Self::parse_rgb).transpose()?
                .unwrap_or(shadow.color),
            extent: global.shadow_extent.unwrap_or(shadow.extent),
            offset_x: global.shadow_offset_x.unwrap_or(shadow.offset_x),
            offset_y: global.shadow_offset_y.unwrap_or(shadow.offset_y),
            strength: global.shadow_strength.unwrap_or(shadow.strength),
        };
        let border = config.visuals.border;
        config.visuals.border = BorderConfig {
            width: global.border_width.unwrap_or(border.width),
            inactive_color: global.border_inactive_color.as_deref().map(Self::parse_color).transpose()?.unwrap_or(border.inactive_color),
            focused_color: global.border_focused_color.as_deref().map(Self::parse_color).transpose()?.unwrap_or(border.focused_color),
            urgent_color: global.border_urgent_color.as_deref().map(Self::parse_color).transpose()?.unwrap_or(border.urgent_color),
        };
        config.visuals.opacity = OpacityConfig {
            focused: global.opacity_focused.unwrap_or(1.0),
            inactive: global.opacity_inactive.unwrap_or(1.0),
            urgent: global.opacity_urgent.unwrap_or(1.0),
        };
        config.blur_enabled = global.blur_enabled.unwrap_or(true);
        config.animation = resolve_animation(&global)?;
        validate_visuals(&config.visuals)?;
        config.rules = self.rules.into_iter().map(validate_rule).collect::<Result<_, _>>()?;
        Ok(config)
    }

    fn parse_rgb(value: &str) -> Result<[u8; 3], ParseError> {
        CompositorConfig::parse_rgb_color(value).map_err(|message| error(0, None, message))
    }

    fn parse_color(value: &str) -> Result<[f32; 4], ParseError> {
        CompositorConfig::parse_color(value).map_err(|message| error(0, None, message))
    }
}

#[derive(Clone, Copy)]
enum Section { None, Global, Rule(usize) }

impl Section {
    fn name(self) -> Option<String> {
        match self { Self::None => None, Self::Global => Some("global".into()), Self::Rule(index) => Some(format!("rule {index}")) }
    }
}

fn error(line: usize, section: Option<String>, message: &str) -> ParseError {
    ParseError { line, section, message: message.to_owned() }
}

fn quoted(value: &str, line: usize, section: Option<String>) -> Result<String, ParseError> {
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        let inner = &value[1..value.len() - 1];
        if !inner.contains('"') { return Ok(inner.to_owned()); }
    }
    Err(error(line, section, "expected a quoted string"))
}

fn bool_value(value: &str, line: usize, section: Option<String>) -> Result<bool, ParseError> {
    match value { "true" => Ok(true), "false" => Ok(false), _ => Err(error(line, section, "expected true or false")) }
}

fn number(value: &str, line: usize, section: Option<String>) -> Result<f32, ParseError> {
    value.parse::<f32>().map_err(|_| error(line, section, "expected a finite decimal number"))
}

fn string_value(value: &str, line: usize, section: Option<String>) -> Result<String, ParseError> {
    quoted(value, line, section)
}

fn token_value(value: &str, line: usize, section: Option<String>) -> Result<String, ParseError> {
    if !value.is_empty() && !value.chars().any(char::is_whitespace) {
        Ok(value.to_owned())
    } else {
        Err(error(line, section, "expected a non-empty value"))
    }
}

fn parse_global(global: &mut ParsedGlobalConfig, key: &str, value: &str, line: usize) -> Result<(), ParseError> {
    let section = Some("global".to_owned());
    macro_rules! assign { ($field:ident, $parser:ident) => { global.$field = Some($parser(value, line, section.clone())?) } }
    match key {
        "shadow.enabled" => assign!(shadow_enabled, bool_value),
        "shadow.color" => assign!(shadow_color, token_value),
        "shadow.extent" => assign!(shadow_extent, number),
        "shadow.offset_x" => assign!(shadow_offset_x, number),
        "shadow.offset_y" => assign!(shadow_offset_y, number),
        "shadow.strength" => assign!(shadow_strength, number),
        "corner.radius" => assign!(corner_radius, number),
        "border.width" => assign!(border_width, number),
        "border.inactive_color" => assign!(border_inactive_color, token_value),
        "border.focused_color" => assign!(border_focused_color, token_value),
        "border.urgent_color" => assign!(border_urgent_color, token_value),
        "opacity.focused" => assign!(opacity_focused, number),
        "opacity.inactive" => assign!(opacity_inactive, number),
        "opacity.urgent" => assign!(opacity_urgent, number),
        "blur.enabled" => assign!(blur_enabled, bool_value),
        "animation.enabled" => assign!(animation_enabled, bool_value),
        "animation.open.effect" => assign!(animation_open_effect, token_value),
        "animation.open.duration" => assign!(animation_open_duration, number),
        "animation.close.enabled" => assign!(animation_close_enabled, bool_value),
        "animation.close.effect" => assign!(animation_close_effect, token_value),
        "animation.close.duration" => assign!(animation_close_duration, number),
        _ => return Err(error(line, section, "unknown key")),
    }
    Ok(())
}

/// 3a3fa2b1 — resolves and validates `animation.*`. Bounds are checked
/// unconditionally, regardless of `animation.enabled`, matching this
/// file's existing precedent for shadow's unconditional finite/range
/// checks (`validate_visuals`) versus its enabled-conditional ones.
fn resolve_animation(global: &ParsedGlobalConfig) -> Result<AnimationConfig, ParseError> {
    let section = Some("global".to_owned());
    let effect = match global.animation_open_effect.as_deref() {
        None => OpenAnimationEffect::Scale,
        Some("scale") => OpenAnimationEffect::Scale,
        Some("teleport") => OpenAnimationEffect::Teleport,
        Some("energy_tear") => OpenAnimationEffect::EnergyTear,
        Some("bubble") => OpenAnimationEffect::Bubble,
        Some("teleport_flashy") => OpenAnimationEffect::TeleportFlashy,
        Some("kamui") => OpenAnimationEffect::Kamui,
        Some(_) => return Err(error(0, section, "unknown animation effect")),
    };
    let duration_ms = global.animation_open_duration.unwrap_or(180.0);
    if !duration_ms.is_finite() || duration_ms < 50.0 || duration_ms > 3000.0 {
        return Err(error(0, section, "animation.open.duration must be between 50 and 3000 milliseconds"));
    }
    let close = resolve_close_animation(global)?;
    Ok(AnimationConfig {
        enabled: global.animation_enabled.unwrap_or(false),
        open: OpenAnimationConfig {
            effect,
            duration: Duration::from_millis(duration_ms.round() as u64),
        },
        close,
    })
}

/// 3a3fa2b5/b6-r1/b7 — resolves and validates `animation.close.*`.
/// Mirrors `resolve_animation`'s exact policy shape: duration bounds are
/// checked unconditionally, regardless of `animation.close.enabled` (same
/// unconditional-validation precedent as open's own duration, and as
/// shadow's finite/range checks in `validate_visuals`). R1 supported
/// exactly `scale`; b6-r1 added `teleport_flashy`; b7 adds `kamui`. Every
/// other token — including `teleport`/`energy_tear`/`bubble`, all valid
/// OPEN effects — is still rejected, since this milestone deliberately
/// does not expose those as CLOSE effects.
fn resolve_close_animation(global: &ParsedGlobalConfig) -> Result<CloseAnimationConfig, ParseError> {
    let section = Some("global".to_owned());
    let effect = match global.animation_close_effect.as_deref() {
        None => CloseAnimationEffect::Scale,
        Some("scale") => CloseAnimationEffect::Scale,
        Some("teleport_flashy") => CloseAnimationEffect::TeleportFlashy,
        Some("kamui") => CloseAnimationEffect::Kamui,
        Some(_) => return Err(error(0, section, "unknown close animation effect")),
    };
    let duration_ms = global.animation_close_duration.unwrap_or(180.0);
    if !duration_ms.is_finite() || duration_ms < 50.0 || duration_ms > 3000.0 {
        return Err(error(0, section, "animation.close.duration must be between 50 and 3000 milliseconds"));
    }
    Ok(CloseAnimationConfig {
        enabled: global.animation_close_enabled.unwrap_or(false),
        effect,
        duration: Duration::from_millis(duration_ms.round() as u64),
    })
}

fn parse_rule(rule: &mut ParsedRule, key: &str, value: &str, line: usize) -> Result<(), ParseError> {
    let section = Some(format!("rule {}", rule.name));
    match key {
        "class" => rule.class = Some(string_value(value, line, section)?),
        "instance" => rule.instance = Some(string_value(value, line, section)?),
        "window_type" => rule.window_type = Some(string_value(value, line, section)?),
        "blur" => rule.blur = Some(bool_value(value, line, section)?),
        "shadow" => rule.shadow = Some(bool_value(value, line, section)?),
        "frame" => rule.frame = Some(match value {
            "default" => FrameRuleAction::Default,
            "request" => FrameRuleAction::Request,
            "suppress" => FrameRuleAction::Suppress,
            _ => return Err(error(line, section, "expected default, request, or suppress")),
        }),
        "opacity" => rule.opacity = Some(number(value, line, section)?),
        _ => return Err(error(line, Some(format!("rule {}", rule.name)), "unknown key")),
    }
    Ok(())
}

fn validate_visuals(visuals: &VisualConfig) -> Result<(), ParseError> {
    let values = [visuals.shadow.extent, visuals.shadow.offset_x, visuals.shadow.offset_y, visuals.shadow.strength, visuals.border.width, visuals.opacity.focused, visuals.opacity.inactive, visuals.opacity.urgent];
    if values.iter().any(|value| !value.is_finite()) {
        return Err(error(0, Some("global".into()), "numeric values must be finite"));
    }
    if !visuals.corner_radius.is_finite() || visuals.corner_radius < 0.0 {
        return Err(error(0, Some("global".into()), "corner radius must be finite and non-negative"));
    }
    if visuals.shadow.extent < 0.0 || visuals.shadow.strength < 0.0 || visuals.shadow.strength > 1.0 {
        return Err(error(0, Some("global".into()), "shadow extent must be non-negative and strength must be within 0..=1"));
    }
    if visuals.shadow.enabled && (visuals.shadow.extent <= 0.0 || visuals.shadow.strength <= 0.0) {
        return Err(error(0, Some("global".into()), "enabled shadow requires positive extent and strength"));
    }
    if visuals.border.width < 0.0 || [visuals.opacity.focused, visuals.opacity.inactive, visuals.opacity.urgent].iter().any(|value| !(0.0..=1.0).contains(value)) {
        return Err(error(0, Some("global".into()), "border width must be non-negative and opacity must be within 0..=1"));
    }
    if visuals.border.inactive_color.iter().chain(visuals.border.focused_color.iter()).chain(visuals.border.urgent_color.iter()).any(|value| !value.is_finite() || !(0.0..=1.0).contains(value)) {
        return Err(error(0, Some("global".into()), "border colors must be within 0..=1"));
    }
    Ok(())
}

fn validate_rule(rule: ParsedRule) -> Result<WindowRule, ParseError> {
    if rule.class.is_none() && rule.instance.is_none() && rule.window_type.is_none() {
        return Err(error(0, Some(format!("rule {}", rule.name)), "rule must specify a match field"));
    }
    let window_type = rule.window_type.map(|value| match value.as_str() {
        "normal" => Ok(WindowType::Normal),
        "dock" => Ok(WindowType::Dock),
        "desktop" => Ok(WindowType::Desktop),
        _ => Err(error(0, Some(format!("rule {}", rule.name)), "unknown window_type")),
    }).transpose()?;
    if rule.blur.is_none() && rule.shadow.is_none() && rule.frame.is_none() && rule.opacity.is_none() {
        return Err(error(0, Some(format!("rule {}", rule.name)), "rule must specify an action"));
    }
    if let Some(opacity) = rule.opacity
        && (!opacity.is_finite() || !(0.0..=1.0).contains(&opacity))
    {
        return Err(error(0, Some(format!("rule {}", rule.name)), "opacity must be within 0..=1"));
    }
    Ok(WindowRule {
        name: rule.name,
        matcher: RuleMatch { class: rule.class, instance: rule.instance, window_type },
        actions: RuleActions { blur: rule.blur, shadow: rule.shadow, frame: rule.frame, opacity: rule.opacity },
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WindowMetadataForRule<'a> {
    pub(crate) class: Option<&'a str>,
    pub(crate) instance: Option<&'a str>,
    pub(crate) window_type: Option<WindowType>,
}

impl WindowRule {
    pub(crate) fn matches(&self, metadata: WindowMetadataForRule<'_>) -> bool {
        self.matcher.class.as_deref().is_none_or(|value| Some(value) == metadata.class)
            && self.matcher.instance.as_deref().is_none_or(|value| Some(value) == metadata.instance)
            && self.matcher.window_type.is_none_or(|value| Some(value) == metadata.window_type)
    }
}

pub(crate) fn resolve_blur(global_enabled: bool, rule_override: Option<bool>, application_requested: bool) -> bool {
    global_enabled && rule_override.unwrap_or(application_requested)
}

pub(crate) fn resolve_rule_actions(
    rules: &[WindowRule],
    metadata: WindowMetadataForRule<'_>,
) -> RuleActions {
    let mut result = RuleActions { blur: None, shadow: None, frame: None, opacity: None };
    for rule in rules.iter().filter(|rule| rule.matches(metadata)) {
        if rule.actions.blur.is_some() { result.blur = rule.actions.blur; }
        if rule.actions.shadow.is_some() { result.shadow = rule.actions.shadow; }
        if rule.actions.frame.is_some() { result.frame = rule.actions.frame; }
        if rule.actions.opacity.is_some() { result.opacity = rule.actions.opacity; }
    }
    result
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct VisualConfig {
    pub(crate) corner_radius: f32,
    pub(crate) border: BorderConfig,
    pub(crate) shadow: ShadowConfig,
    pub(crate) opacity: OpacityConfig,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct BorderConfig {
    pub(crate) width: f32,
    pub(crate) inactive_color: [f32; 4],
    pub(crate) focused_color: [f32; 4],
    pub(crate) urgent_color: [f32; 4],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ShadowConfig {
    pub(crate) enabled: bool,
    pub(crate) color: [u8; 3],
    pub(crate) offset_x: f32,
    pub(crate) offset_y: f32,
    pub(crate) extent: f32,
    pub(crate) strength: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct OpacityConfig {
    pub(crate) focused: f32,
    pub(crate) inactive: f32,
    pub(crate) urgent: f32,
}

impl Default for CompositorConfig {
    fn default() -> Self {
        Self {
            visuals: VisualConfig::default(),
            blur_enabled: true,
            animation: AnimationConfig::default(),
            rules: Arc::from([]),
        }
    }
}

impl Default for VisualConfig {
    fn default() -> Self {
        Self {
            corner_radius: 0.0,
            border: BorderConfig::default(),
            shadow: ShadowConfig::default(),
            opacity: OpacityConfig::default(),
        }
    }
}

impl Default for BorderConfig {
    fn default() -> Self {
        Self {
            width: 0.0,
            inactive_color: [0.0, 0.0, 0.0, 1.0],
            focused_color: [0.0, 0.0, 0.0, 1.0],
            urgent_color: [0.0, 0.0, 0.0, 1.0],
        }
    }
}

impl Default for ShadowConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            color: [0, 0, 0],
            offset_x: 0.0,
            offset_y: 0.0,
            extent: 0.0,
            strength: 0.0,
        }
    }
}

impl Default for OpacityConfig {
    fn default() -> Self {
        Self { focused: 1.0, inactive: 1.0, urgent: 1.0 }
    }
}

impl CompositorConfig {
    pub(crate) fn defaults() -> Self {
        Self::default()
    }

    pub(crate) fn with_corner_radius(corner_radius: f32) -> Result<Self, &'static str> {
        if !corner_radius.is_finite() || corner_radius < 0.0 {
            return Err("corner radius must be finite and non-negative");
        }
        let mut config = Self::default();
        config.visuals.corner_radius = corner_radius;
        Ok(config)
    }

    pub(crate) fn with_border_colors(
        mut self,
        width: f32,
        inactive_color: [f32; 4],
        focused_color: [f32; 4],
        urgent_color: [f32; 4],
    ) -> Result<Self, &'static str> {
        let colors = [inactive_color, focused_color, urgent_color];
        if !width.is_finite()
            || width < 0.0
            || colors.iter().flatten().any(|value| !value.is_finite() || *value < 0.0 || *value > 1.0)
        {
            return Err("border width and colors must be finite and within range");
        }
        self.visuals.border = BorderConfig {
            width,
            inactive_color,
            focused_color,
            urgent_color,
        };
        Ok(self)
    }

    pub(crate) fn with_shadow(
        mut self,
        enabled: bool,
        color: [u8; 3],
        extent: f32,
        offset_x: f32,
        offset_y: f32,
        strength: f32,
    ) -> Result<Self, &'static str> {
        if !extent.is_finite()
            || extent < 0.0
            || !offset_x.is_finite()
            || !offset_y.is_finite()
            || !strength.is_finite()
            || strength < 0.0
            || strength > 1.0
            || (enabled && (extent <= 0.0 || strength <= 0.0))
        {
            return Err("shadow extent, offsets, and strength must be finite; extent and strength must be positive, with strength at most one");
        }
        self.visuals.shadow = ShadowConfig {
            enabled,
            color,
            offset_x,
            offset_y,
            extent,
            strength,
        };
        Ok(self)
    }

    pub(crate) fn with_opacity(
        mut self,
        focused: f32,
        inactive: f32,
        urgent: f32,
    ) -> Result<Self, &'static str> {
        let values = [focused, inactive, urgent];
        if values.iter().any(|value| !value.is_finite() || *value < 0.0 || *value > 1.0) {
            return Err("focused, inactive, and urgent opacity must be finite and within range");
        }
        self.visuals.opacity = OpacityConfig { focused, inactive, urgent };
        Ok(self)
    }

    pub(crate) fn parse_rgb_color(value: &str) -> Result<[u8; 3], &'static str> {
        let hex = value.strip_prefix('#').unwrap_or(value);
        if hex.len() != 6 {
            return Err("shadow color must be RRGGBB");
        }
        let parse = |part: &str| u8::from_str_radix(part, 16).map_err(|_| "shadow color must be hexadecimal");
        Ok([parse(&hex[0..2])?, parse(&hex[2..4])?, parse(&hex[4..6])?])
    }

    pub(crate) fn parse_color(value: &str) -> Result<[f32; 4], &'static str> {
        let hex = value.strip_prefix('#').unwrap_or(value);
        if hex.len() != 6 && hex.len() != 8 {
            return Err("border color must be RRGGBB or RRGGBBAA");
        }
        let parse = |part: &str| u8::from_str_radix(part, 16).map(|value| f32::from(value) / 255.0)
            .map_err(|_| "border color must be hexadecimal");
        let alpha = if hex.len() == 8 { parse(&hex[6..8])? } else { 1.0 };
        Ok([parse(&hex[0..2])?, parse(&hex[2..4])?, parse(&hex[4..6])?, alpha])
    }
}

#[cfg(test)]
mod tests {
    use super::{CompositorConfig, ConfigLoadError, ConfigLoadOutcome, ConfigPathEnvironment,
        FrameRuleAction, OpacityConfig, ParsedConfig, RuleActions, StartupConfigRequest, ValidatedConfig,
        WindowMetadataForRule, WindowType, load_startup_config, resolve_blur,
        resolve_config_path, resolve_rule_actions,
        AnimationConfig, OpenAnimationConfig, OpenAnimationEffect,
        CloseAnimationConfig, CloseAnimationEffect};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    #[test]
    fn defaults_preserve_current_visual_semantics() {
        let visuals = CompositorConfig::defaults().visuals;
        assert_eq!(visuals.corner_radius, 0.0);
        assert_eq!(visuals.border.width, 0.0);
        assert_eq!(visuals.border.inactive_color, [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(visuals.border.focused_color, [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(visuals.border.urgent_color, [0.0, 0.0, 0.0, 1.0]);
        let shadow = visuals.shadow;
        assert!(!shadow.enabled);
        assert_eq!(shadow.color, [0, 0, 0]);
        assert_eq!(shadow.offset_x, 0.0);
        assert_eq!(shadow.offset_y, 0.0);
        assert_eq!(shadow.extent, 0.0);
        assert_eq!(shadow.strength, 0.0);
        assert_eq!(visuals.opacity, super::OpacityConfig { focused: 1.0, inactive: 1.0, urgent: 1.0 });
    }

    #[test]
    fn defaults_are_deterministic() {
        assert_eq!(CompositorConfig::defaults(), CompositorConfig::default());
    }

    #[test]
    fn corner_radius_probe_rejects_invalid_values() {
        assert!(CompositorConfig::with_corner_radius(-1.0).is_err());
        assert!(CompositorConfig::with_corner_radius(f32::NAN).is_err());
        assert_eq!(CompositorConfig::with_corner_radius(8.0).unwrap().visuals.corner_radius, 8.0);
    }

    #[test]
    fn border_defaults_and_color_parser_are_deterministic() {
        assert_eq!(CompositorConfig::defaults().visuals.border.inactive_color, [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(CompositorConfig::parse_color("#336699cc").unwrap(), [0.2, 0.4, 0.6, 0.8]);
        assert!(CompositorConfig::parse_color("xyz").is_err());
    }

    #[test]
    fn border_state_colors_are_owned_by_one_typed_config() {
        let config = CompositorConfig::defaults()
            .with_border_colors(2.0, [0.1, 0.1, 0.1, 1.0], [0.2, 0.3, 0.4, 1.0], [1.0, 0.0, 0.0, 1.0])
            .unwrap();
        assert_eq!(config.visuals.border.width, 2.0);
        assert_eq!(config.visuals.border.focused_color, [0.2, 0.3, 0.4, 1.0]);
        assert!(CompositorConfig::defaults().with_border_colors(2.0, [2.0, 0.0, 0.0, 1.0], [0.0; 4], [0.0; 4]).is_err());
    }

    #[test]
    fn shadow_rgb_color_parser_accepts_only_rgb_hex() {
        assert_eq!(CompositorConfig::parse_rgb_color("000000").unwrap(), [0, 0, 0]);
        assert_eq!(CompositorConfig::parse_rgb_color("FF0000").unwrap(), [255, 0, 0]);
        assert_eq!(CompositorConfig::parse_rgb_color("4C7899").unwrap(), [0x4c, 0x78, 0x99]);
        assert!(CompositorConfig::parse_rgb_color("12345").is_err());
        assert!(CompositorConfig::parse_rgb_color("12345678").is_err());
        assert!(CompositorConfig::parse_rgb_color("GG0000").is_err());
    }

    #[test]
    fn shadow_values_are_validated_and_stored() {
        let config = CompositorConfig::defaults()
            .with_shadow(true, [0x4c, 0x78, 0x99], 18.0, -2.0, 4.0, 0.28)
            .unwrap();
        assert_eq!(config.visuals.shadow.color, [0x4c, 0x78, 0x99]);
        assert_eq!(config.visuals.shadow.extent, 18.0);
        assert_eq!(config.visuals.shadow.offset_x, -2.0);
        assert_eq!(config.visuals.shadow.offset_y, 4.0);
        assert_eq!(config.visuals.shadow.strength, 0.28);
        assert!(CompositorConfig::defaults().with_shadow(true, [0, 0, 0], 0.0, 0.0, 0.0, 0.28).is_err());
        assert!(CompositorConfig::defaults().with_shadow(true, [0, 0, 0], 18.0, 0.0, 0.0, 0.0).is_err());
        assert!(CompositorConfig::defaults().with_shadow(true, [0, 0, 0], 18.0, 0.0, 0.0, 1.01).is_err());
        assert!(CompositorConfig::defaults().with_shadow(true, [0, 0, 0], f32::NAN, 0.0, 0.0, 0.28).is_err());
    }

    #[test]
    fn visual_opacity_defaults_and_validation_are_deterministic() {
        let config = CompositorConfig::defaults()
            .with_opacity(1.0, 0.92, 1.0)
            .unwrap();
        assert_eq!(config.visuals.opacity.focused, 1.0);
        assert_eq!(config.visuals.opacity.inactive, 0.92);
        assert_eq!(config.visuals.opacity.urgent, 1.0);
        for value in [-0.1, 1.1, f32::NAN, f32::INFINITY] {
            assert!(CompositorConfig::defaults().with_opacity(value, 1.0, 1.0).is_err());
        }
    }

    #[test]
    fn path_resolution_is_explicit_and_deterministic() {
        let env = ConfigPathEnvironment { home: Some("/home/test".into()), xdg_config_home: Some("/xdg".into()) };
        assert_eq!(resolve_config_path(Some(Path::new("/explicit.conf")), &env).unwrap(), PathBuf::from("/explicit.conf"));
        assert_eq!(resolve_config_path(None, &env).unwrap(), PathBuf::from("/xdg/xomposite/xomposite.conf"));
        let home_only = ConfigPathEnvironment { home: Some("/home/test".into()), xdg_config_home: None };
        assert_eq!(resolve_config_path(None, &home_only).unwrap(), PathBuf::from("/home/test/.config/xomposite/xomposite.conf"));
        assert!(resolve_config_path(None, &ConfigPathEnvironment { home: Some("".into()), xdg_config_home: Some("".into()) }).is_err());
    }

    #[test]
    fn parser_accepts_comments_globals_colors_and_rule() {
        let parsed = ParsedConfig::parse("# comment\n[global]\nblur.enabled = true\nshadow.color = #112233\n\n[rule \"term\"]\nclass = \"Alacritty\"\nblur = true\n").unwrap();
        let validated = parsed.validate().unwrap();
        assert!(validated.blur_enabled);
        assert_eq!(validated.visuals.shadow.color, [0x11, 0x22, 0x33]);
        assert_eq!(validated.rules.len(), 1);
        assert_eq!(validated.rules[0].matcher.class.as_deref(), Some("Alacritty"));
    }

    #[test]
    fn parser_accepts_corner_radius_and_materializes_it() {
        let parsed = ParsedConfig::parse("[global]\ncorner.radius = 16").unwrap();
        assert_eq!(parsed.global.corner_radius, Some(16.0));
        assert_eq!(parsed.validate().unwrap().visuals.corner_radius, 16.0);
        assert!(ParsedConfig::parse("[global]\ncorner.radius = -1").unwrap().validate().is_err());
    }

    #[test]
    fn parser_accepts_the_daily_driver_global_profile() {
        let config = ParsedConfig::parse(
            "[global]\ncorner.radius = 16\nborder.width = 2\nborder.inactive_color = 555555\nborder.focused_color = 4C7899\nborder.urgent_color = FF3030\nblur.enabled = true",
        ).unwrap().validate().unwrap();
        assert_eq!(config.visuals.corner_radius, 16.0);
        assert_eq!(config.visuals.border.width, 2.0);
        assert_eq!(config.visuals.border.inactive_color, [0x55 as f32 / 255.0, 0x55 as f32 / 255.0, 0x55 as f32 / 255.0, 1.0]);
        assert_eq!(config.visuals.border.focused_color, [0x4C as f32 / 255.0, 0x78 as f32 / 255.0, 0x99 as f32 / 255.0, 1.0]);
        assert_eq!(config.visuals.border.urgent_color, [1.0, 0x30 as f32 / 255.0, 0x30 as f32 / 255.0, 1.0]);
        assert!(config.blur_enabled);
    }

    #[test]
    fn sparse_shadow_enable_inherits_valid_latent_defaults() {
        let config = ParsedConfig::parse("[global]\nshadow.enabled = true").unwrap().validate().unwrap();
        assert!(config.visuals.shadow.enabled);
        assert_eq!(config.visuals.shadow.color, [0, 0, 0]);
        assert!(config.visuals.shadow.extent > 0.0);
        assert!(config.visuals.shadow.strength > 0.0);
    }

    #[test]
    fn sparse_shadow_overrides_preserve_omitted_parameters() {
        let strength = ParsedConfig::parse("[global]\nshadow.enabled = true\nshadow.strength = 0.40").unwrap().validate().unwrap();
        assert_eq!(strength.visuals.shadow.extent, 18.0);
        assert_eq!(strength.visuals.shadow.strength, 0.40);
        let extent = ParsedConfig::parse("[global]\nshadow.enabled = true\nshadow.extent = 24").unwrap().validate().unwrap();
        assert_eq!(extent.visuals.shadow.extent, 24.0);
        assert_eq!(extent.visuals.shadow.strength, 0.28);
        let disabled = ParsedConfig::parse("[global]\nshadow.enabled = false").unwrap().validate().unwrap();
        assert!(!disabled.visuals.shadow.enabled);
        assert_eq!(disabled.visuals.shadow.extent, 18.0);
        let offset = ParsedConfig::parse("[global]\nshadow.offset_y = 7").unwrap().validate().unwrap();
        assert!(!offset.visuals.shadow.enabled);
        assert_eq!(offset.visuals.shadow.offset_y, 7.0);
        assert_eq!(offset.visuals.shadow.extent, 18.0);
    }

    #[test]
    fn explicit_invalid_shadow_values_still_fail_when_enabled() {
        assert!(ParsedConfig::parse("[global]\nshadow.enabled = true\nshadow.extent = -5").unwrap().validate().is_err());
        assert!(ParsedConfig::parse("[global]\nshadow.enabled = true\nshadow.strength = 0").unwrap().validate().is_err());
    }

    #[test]
    fn partial_global_and_empty_global_configs_remain_valid() {
        assert!(ParsedConfig::parse("[global]\nborder.width = 2").unwrap().validate().is_ok());
        assert!(ParsedConfig::parse("[global]").unwrap().validate().is_ok());
    }

    #[test]
    fn parser_rejects_unknown_sections_keys_duplicates_and_bad_values() {
        assert!(ParsedConfig::parse("[wat]\nx = true").is_err());
        assert!(ParsedConfig::parse("[global]\nshdaow.enabled = true").is_err());
        assert!(ParsedConfig::parse("[global]\ncorner_radius = 16").is_err());
        assert!(ParsedConfig::parse("[global]\nblur.enabled = true\nblur.enabled = false").is_err());
        assert!(ParsedConfig::parse("[global]\nblur.enabled = maybe").is_err());
        assert!(ParsedConfig::parse("[global]\nopacity.focused = nope").is_err());
        assert!(ParsedConfig::parse("[global]\nshadow.color = \"bad\"").unwrap().validate().is_err());
    }

    #[test]
    fn defaults_are_neutral_and_validated_config_is_arc_safe() {
        let defaults = ValidatedConfig::default();
        assert!(defaults.blur_enabled);
        assert!(defaults.rules.is_empty());
        assert!(!defaults.visuals.shadow.enabled);
        assert_eq!(defaults.visuals.opacity, OpacityConfig { focused: 1.0, inactive: 1.0, urgent: 1.0 });
        let _snapshot = std::sync::Arc::new(defaults);
    }

    // ========================================================
    // 3a3fa2b1 — animation config model.
    // ========================================================

    // --- A: no animation keys -> compiled defaults ---

    #[test]
    fn no_animation_keys_yields_disabled_scale_180ms_defaults() {
        let config = ParsedConfig::parse("[global]\nblur.enabled = true").unwrap().validate().unwrap();
        assert_eq!(
            config.animation,
            AnimationConfig {
                enabled: false,
                open: OpenAnimationConfig { effect: OpenAnimationEffect::Scale, duration: Duration::from_millis(180) },
                close: CloseAnimationConfig { enabled: false, effect: CloseAnimationEffect::Scale, duration: Duration::from_millis(180) },
            },
        );
    }

    #[test]
    fn empty_config_also_yields_the_same_animation_defaults() {
        let config = ParsedConfig::parse("[global]").unwrap().validate().unwrap();
        assert_eq!(config.animation, AnimationConfig::default());
    }

    // --- B: animation.enabled = true ---

    #[test]
    fn animation_enabled_true_is_honored() {
        let config = ParsedConfig::parse("[global]\nanimation.enabled = true").unwrap().validate().unwrap();
        assert!(config.animation.enabled);
        // omitted effect/duration still resolve to their own defaults
        assert_eq!(config.animation.open.effect, OpenAnimationEffect::Scale);
        assert_eq!(config.animation.open.duration, Duration::from_millis(180));
    }

    // --- C: effect = scale parses ---

    #[test]
    fn effect_scale_parses() {
        let config = ParsedConfig::parse("[global]\nanimation.enabled = true\nanimation.open.effect = scale").unwrap().validate().unwrap();
        assert_eq!(config.animation.open.effect, OpenAnimationEffect::Scale);
    }

    // --- D/E/F: teleport now parses (R2); bubble/fire still rejected ---

    #[test]
    fn effect_teleport_parses() {
        // 3a3fa2b2-r2: teleport is now a supported effect (was rejected
        // through b1/b2-r1).
        let config = ParsedConfig::parse("[global]\nanimation.enabled = true\nanimation.open.effect = teleport").unwrap().validate().unwrap();
        assert_eq!(config.animation.open.effect, OpenAnimationEffect::Teleport);
    }

    #[test]
    fn effect_energy_tear_parses() {
        // 3a3fa2b3: energy_tear is a new supported effect, additive
        // alongside scale/teleport.
        let config = ParsedConfig::parse("[global]\nanimation.enabled = true\nanimation.open.effect = energy_tear").unwrap().validate().unwrap();
        assert_eq!(config.animation.open.effect, OpenAnimationEffect::EnergyTear);
    }

    #[test]
    fn effect_bubble_parses() {
        // 3a3fa2b4-r1: bubble is a new supported effect, additive
        // alongside scale/teleport/energy_tear.
        let config = ParsedConfig::parse("[global]\nanimation.enabled = true\nanimation.open.effect = bubble").unwrap().validate().unwrap();
        assert_eq!(config.animation.open.effect, OpenAnimationEffect::Bubble);
    }

    #[test]
    fn effect_fire_rejected() {
        assert!(ParsedConfig::parse("[global]\nanimation.open.effect = fire").unwrap().validate().is_err());
    }

    // --- G: any other unknown effect token rejected ---

    #[test]
    fn unknown_effect_token_rejected() {
        assert!(ParsedConfig::parse("[global]\nanimation.open.effect = spin").unwrap().validate().is_err());
    }

    // --- H/I: duration bounds accepted ---

    #[test]
    fn duration_50_accepted() {
        let config = ParsedConfig::parse("[global]\nanimation.open.duration = 50").unwrap().validate().unwrap();
        assert_eq!(config.animation.open.duration, Duration::from_millis(50));
    }

    #[test]
    fn duration_3000_accepted() {
        let config = ParsedConfig::parse("[global]\nanimation.open.duration = 3000").unwrap().validate().unwrap();
        assert_eq!(config.animation.open.duration, Duration::from_millis(3000));
    }

    // --- J/K: duration bounds rejected ---

    #[test]
    fn duration_49_rejected() {
        assert!(ParsedConfig::parse("[global]\nanimation.open.duration = 49").unwrap().validate().is_err());
    }

    #[test]
    fn duration_3001_rejected() {
        assert!(ParsedConfig::parse("[global]\nanimation.open.duration = 3001").unwrap().validate().is_err());
    }

    #[test]
    fn duration_0_and_negative_rejected() {
        assert!(ParsedConfig::parse("[global]\nanimation.open.duration = 0").unwrap().validate().is_err());
        assert!(ParsedConfig::parse("[global]\nanimation.open.duration = -5").unwrap().validate().is_err());
    }

    // --- L: invalid duration rejected even when enabled=false ---

    #[test]
    fn invalid_duration_rejected_even_when_disabled() {
        assert!(ParsedConfig::parse("[global]\nanimation.enabled = false\nanimation.open.duration = 1").unwrap().validate().is_err());
        // b4-r1: "bubble" is now a valid effect too, so the invalid-value
        // probe here uses "fire" (still unsupported) to keep testing the
        // same thing this test always tested — an invalid value is
        // rejected even while animation.enabled = false.
        assert!(ParsedConfig::parse("[global]\nanimation.enabled = false\nanimation.open.effect = fire").unwrap().validate().is_err());
    }

    #[test]
    fn invalid_duration_number_is_a_parse_time_error() {
        assert!(ParsedConfig::parse("[global]\nanimation.open.duration = nope").is_err());
    }

    #[test]
    fn animation_daily_driver_profile_reproduces_temporary_exaggerated_values() {
        // Exact snippet a human would use post-apply to reproduce the
        // currently-running temporary 0.70/800ms validation behavior.
        let config = ParsedConfig::parse(
            "[global]\nanimation.enabled = true\nanimation.open.effect = scale\nanimation.open.duration = 800",
        ).unwrap().validate().unwrap();
        assert!(config.animation.enabled);
        assert_eq!(config.animation.open.effect, OpenAnimationEffect::Scale);
        assert_eq!(config.animation.open.duration, Duration::from_millis(800));
    }

    // ========================================================
    // 3a3fa2b5 — close animation config model.
    // ========================================================

    #[test]
    fn no_close_keys_yields_disabled_scale_180ms_defaults() {
        let config = ParsedConfig::parse("[global]\nblur.enabled = true").unwrap().validate().unwrap();
        assert_eq!(
            config.animation.close,
            CloseAnimationConfig { enabled: false, effect: CloseAnimationEffect::Scale, duration: Duration::from_millis(180) },
        );
    }

    #[test]
    fn empty_config_also_yields_the_same_close_defaults() {
        let config = ParsedConfig::parse("[global]").unwrap().validate().unwrap();
        assert_eq!(config.animation.close, AnimationConfig::default().close);
    }

    #[test]
    fn close_enabled_true_is_honored_independently_of_open() {
        let config = ParsedConfig::parse("[global]\nanimation.close.enabled = true").unwrap().validate().unwrap();
        assert!(config.animation.close.enabled);
        assert!(!config.animation.enabled);
        assert_eq!(config.animation.close.effect, CloseAnimationEffect::Scale);
        assert_eq!(config.animation.close.duration, Duration::from_millis(180));
    }

    #[test]
    fn close_effect_scale_parses() {
        let config = ParsedConfig::parse("[global]\nanimation.close.enabled = true\nanimation.close.effect = scale").unwrap().validate().unwrap();
        assert_eq!(config.animation.close.effect, CloseAnimationEffect::Scale);
    }

    #[test]
    fn close_effect_open_only_tokens_are_rejected() {
        // 3a3fa2b6-r1/b7: teleport_flashy and kamui are now valid CLOSE
        // effects too (see close_effect_teleport_flashy_parses /
        // close_effect_kamui_parses below) — removed from this rejection
        // list. teleport/energy_tear/bubble remain OPEN-only.
        for token in ["teleport", "energy_tear", "bubble", "fire", "spin"] {
            let source = format!("[global]\nanimation.close.effect = {token}");
            assert!(
                ParsedConfig::parse(&source).unwrap().validate().is_err(),
                "close effect must reject {token}",
            );
        }
    }

    #[test]
    fn close_effect_teleport_flashy_parses() {
        let config = ParsedConfig::parse("[global]\nanimation.close.enabled = true\nanimation.close.effect = teleport_flashy").unwrap().validate().unwrap();
        assert_eq!(config.animation.close.effect, CloseAnimationEffect::TeleportFlashy);
    }

    #[test]
    fn open_effect_teleport_flashy_parses() {
        let config = ParsedConfig::parse("[global]\nanimation.open.effect = teleport_flashy").unwrap().validate().unwrap();
        assert_eq!(config.animation.open.effect, OpenAnimationEffect::TeleportFlashy);
    }

    #[test]
    fn open_effects_scale_teleport_energy_tear_bubble_still_parse() {
        for (token, expected) in [
            ("scale", OpenAnimationEffect::Scale),
            ("teleport", OpenAnimationEffect::Teleport),
            ("energy_tear", OpenAnimationEffect::EnergyTear),
            ("bubble", OpenAnimationEffect::Bubble),
        ] {
            let source = format!("[global]\nanimation.open.effect = {token}");
            let config = ParsedConfig::parse(&source).unwrap().validate().unwrap();
            assert_eq!(config.animation.open.effect, expected, "token={token}");
        }
    }

    #[test]
    fn open_effect_kamui_parses() {
        let config = ParsedConfig::parse("[global]\nanimation.open.effect = kamui").unwrap().validate().unwrap();
        assert_eq!(config.animation.open.effect, OpenAnimationEffect::Kamui);
    }

    #[test]
    fn close_effect_kamui_parses() {
        let config = ParsedConfig::parse("[global]\nanimation.close.enabled = true\nanimation.close.effect = kamui").unwrap().validate().unwrap();
        assert_eq!(config.animation.close.effect, CloseAnimationEffect::Kamui);
    }

    #[test]
    fn open_effect_teleport_flashy_still_parses_alongside_kamui() {
        // 3a3fa2b7: confirms adding Kamui did not disturb the existing
        // TeleportFlashy token/variant.
        let config = ParsedConfig::parse("[global]\nanimation.open.effect = teleport_flashy").unwrap().validate().unwrap();
        assert_eq!(config.animation.open.effect, OpenAnimationEffect::TeleportFlashy);
    }

    #[test]
    fn close_duration_50_and_3000_accepted() {
        let low = ParsedConfig::parse("[global]\nanimation.close.duration = 50").unwrap().validate().unwrap();
        assert_eq!(low.animation.close.duration, Duration::from_millis(50));
        let high = ParsedConfig::parse("[global]\nanimation.close.duration = 3000").unwrap().validate().unwrap();
        assert_eq!(high.animation.close.duration, Duration::from_millis(3000));
    }

    #[test]
    fn close_duration_out_of_bounds_rejected() {
        assert!(ParsedConfig::parse("[global]\nanimation.close.duration = 49").unwrap().validate().is_err());
        assert!(ParsedConfig::parse("[global]\nanimation.close.duration = 3001").unwrap().validate().is_err());
        assert!(ParsedConfig::parse("[global]\nanimation.close.duration = 0").unwrap().validate().is_err());
        assert!(ParsedConfig::parse("[global]\nanimation.close.duration = -5").unwrap().validate().is_err());
    }

    #[test]
    fn invalid_close_duration_rejected_even_when_close_disabled() {
        assert!(ParsedConfig::parse("[global]\nanimation.close.enabled = false\nanimation.close.duration = 1").unwrap().validate().is_err());
        assert!(ParsedConfig::parse("[global]\nanimation.close.enabled = false\nanimation.close.effect = fire").unwrap().validate().is_err());
    }

    #[test]
    fn close_animation_requires_both_master_and_close_gate_documented_via_fields() {
        // No runtime gate function exists for this (the gate is a plain
        // `&&` at the call site in scene.rs::build_provisional_closing_state)
        // — this test only pins that both flags are independently
        // observable and independently default to false.
        let config = ParsedConfig::parse("[global]\nanimation.enabled = true").unwrap().validate().unwrap();
        assert!(config.animation.enabled);
        assert!(!config.animation.close.enabled);
        let config = ParsedConfig::parse("[global]\nanimation.close.enabled = true").unwrap().validate().unwrap();
        assert!(!config.animation.enabled);
        assert!(config.animation.close.enabled);
    }

    #[test]
    fn open_and_close_effect_are_independent_types_and_defaults() {
        let config = ParsedConfig::parse(
            "[global]\nanimation.enabled = true\nanimation.open.effect = bubble\nanimation.close.enabled = true\nanimation.close.effect = scale",
        ).unwrap().validate().unwrap();
        assert_eq!(config.animation.open.effect, OpenAnimationEffect::Bubble);
        assert_eq!(config.animation.close.effect, CloseAnimationEffect::Scale);
    }

    #[test]
    fn rules_match_exactly_and_all_specified_fields_are_required() {
        let config = ParsedConfig::parse("[rule \"term\"]\nclass = \"Alacritty\"\ninstance = \"main\"\nwindow_type = \"normal\"\nblur = true").unwrap().validate().unwrap();
        let rule = &config.rules[0];
        assert!(rule.matches(WindowMetadataForRule { class: Some("Alacritty"), instance: Some("main"), window_type: Some(WindowType::Normal) }));
        assert!(!rule.matches(WindowMetadataForRule { class: Some("Alacritty"), instance: Some("other"), window_type: Some(WindowType::Normal) }));
        assert!(!rule.matches(WindowMetadataForRule { class: Some("Alacritty"), instance: Some("main"), window_type: Some(WindowType::Dock) }));
    }

    #[test]
    fn rules_use_last_matching_fieldwise_precedence() {
        let config = ParsedConfig::parse("[rule \"broad\"]\nclass = \"App\"\nblur = true\nshadow = false\n\n[rule \"exception\"]\ninstance = \"special\"\nblur = false").unwrap().validate().unwrap();
        let metadata = WindowMetadataForRule { class: Some("App"), instance: Some("special"), window_type: Some(WindowType::Normal) };
        assert_eq!(resolve_rule_actions(&config.rules, metadata), RuleActions { blur: Some(false), shadow: Some(false), frame: None, opacity: None });
    }

    #[test]
    fn rules_parse_frame_and_opacity_actions() {
        let config = ParsedConfig::parse(
            "[rule \"conky\"]\nclass = \"conky\"\ninstance = \"conky\"\nframe = request\nopacity = 0.72",
        ).unwrap().validate().unwrap();
        assert_eq!(config.rules[0].actions.frame, Some(FrameRuleAction::Request));
        assert_eq!(config.rules[0].actions.opacity, Some(0.72));
        for value in ["default", "request", "suppress"] {
            let input = format!("[rule \"x\"]\nclass = \"x\"\nframe = {value}");
            assert!(ParsedConfig::parse(&input).unwrap().validate().is_ok());
        }
        for value in ["0.0", "1.0"] {
            let input = format!("[rule \"x\"]\nclass = \"x\"\nopacity = {value}");
            assert!(ParsedConfig::parse(&input).unwrap().validate().is_ok());
        }
    }

    #[test]
    fn rules_reject_invalid_frame_and_opacity_actions() {
        assert!(ParsedConfig::parse("[rule \"x\"]\nclass = \"x\"\nframe = invalid").is_err());
        assert!(ParsedConfig::parse("[rule \"x\"]\nclass = \"x\"\nopacity = 1.1").unwrap().validate().is_err());
        assert!(ParsedConfig::parse("[rule \"x\"]\nclass = \"x\"\nopacity = -0.1").unwrap().validate().is_err());
    }

    #[test]
    fn blur_precedence_is_independent_of_transparency() {
        assert!(!resolve_blur(false, Some(true), true));
        assert!(!resolve_blur(true, Some(false), true));
        assert!(resolve_blur(true, Some(true), false));
        assert!(resolve_blur(true, None, true));
        assert!(!resolve_blur(true, None, false));
    }

    #[test]
    fn semantic_validation_rejects_invalid_shadow_and_opacity_ranges() {
        assert!(ParsedConfig::parse("[global]\nshadow.enabled = true\nshadow.extent = 0\nshadow.strength = 0.5").unwrap().validate().is_err());
        assert!(ParsedConfig::parse("[global]\nopacity.inactive = 1.1").unwrap().validate().is_err());
        assert!(ParsedConfig::parse("[global]\nshadow.extent = -1").unwrap().validate().is_err());
    }

    #[test]
    fn startup_loader_uses_defaults_for_missing_implicit_file() {
        let outcome = load_startup_config(StartupConfigRequest {
            explicit_path: None,
            environment: ConfigPathEnvironment { home: Some("/definitely-missing-xomposite-home".into()), xdg_config_home: Some("/definitely-missing-xomposite-xdg".into()) },
        }).unwrap();
        assert!(matches!(outcome, ConfigLoadOutcome::DefaultsBecauseMissingImplicitFile));
    }

    #[test]
    fn startup_loader_rejects_missing_explicit_file() {
        let path = std::env::temp_dir().join(format!("xomposite-config-missing-{}", std::process::id()));
        let outcome = load_startup_config(StartupConfigRequest {
            explicit_path: Some(path.clone()),
            environment: ConfigPathEnvironment { home: None, xdg_config_home: None },
        });
        assert!(matches!(outcome, Err(ConfigLoadError::ExplicitPathMissing(found)) if found == path));
    }

    #[test]
    fn startup_loader_reads_validates_and_owns_valid_file() {
        let path = std::env::temp_dir().join(format!("xomposite-config-valid-{}", std::process::id()));
        std::fs::write(&path, "[global]\nblur.enabled = true\n").unwrap();
        let outcome = load_startup_config(StartupConfigRequest {
            explicit_path: Some(path.clone()),
            environment: ConfigPathEnvironment { home: None, xdg_config_home: None },
        }).unwrap();
        let ConfigLoadOutcome::Loaded { path: loaded, config } = outcome else { panic!("expected loaded config") };
        assert_eq!(loaded, path);
        assert!(config.blur_enabled);
        assert_eq!(std::sync::Arc::strong_count(&config), 1);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn startup_loader_rejects_existing_invalid_file() {
        let path = std::env::temp_dir().join(format!("xomposite-config-invalid-{}", std::process::id()));
        std::fs::write(&path, "[global]\nunknown = true\n").unwrap();
        let outcome = load_startup_config(StartupConfigRequest {
            explicit_path: Some(path.clone()),
            environment: ConfigPathEnvironment { home: None, xdg_config_home: None },
        });
        assert!(matches!(outcome, Err(ConfigLoadError::Parse { .. })));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn startup_loader_rejects_existing_semantically_invalid_file() {
        let path = std::env::temp_dir().join(format!("xomposite-config-invalid-value-{}", std::process::id()));
        std::fs::write(&path, "[global]\nopacity.focused = 2\n").unwrap();
        let outcome = load_startup_config(StartupConfigRequest {
            explicit_path: Some(path.clone()),
            environment: ConfigPathEnvironment { home: None, xdg_config_home: None },
        });
        assert!(matches!(outcome, Err(ConfigLoadError::Validate { .. })));
        std::fs::remove_file(path).unwrap();
    }

}
