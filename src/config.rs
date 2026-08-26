#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CompositorConfig {
    pub(crate) visuals: VisualConfig,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct VisualConfig {
    pub(crate) corner_radius: f32,
    pub(crate) border: BorderConfig,
    pub(crate) shadow: ShadowConfig,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct BorderConfig {
    pub(crate) width: f32,
    pub(crate) color: [f32; 4],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ShadowConfig {
    pub(crate) enabled: bool,
    pub(crate) offset_x: f32,
    pub(crate) offset_y: f32,
    pub(crate) extent: f32,
    pub(crate) strength: f32,
}

impl Default for CompositorConfig {
    fn default() -> Self {
        Self {
            visuals: VisualConfig::default(),
        }
    }
}

impl Default for VisualConfig {
    fn default() -> Self {
        Self {
            corner_radius: 0.0,
            border: BorderConfig::default(),
            shadow: ShadowConfig::default(),
        }
    }
}

impl Default for BorderConfig {
    fn default() -> Self {
        Self { width: 0.0, color: [0.0, 0.0, 0.0, 1.0] }
    }
}

impl Default for ShadowConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            offset_x: 0.0,
            offset_y: 0.0,
            extent: 0.0,
            strength: 0.0,
        }
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

    pub(crate) fn with_border(
        mut self,
        width: f32,
        color: [f32; 4],
    ) -> Result<Self, &'static str> {
        if !width.is_finite() || width < 0.0 || color.iter().any(|value| !value.is_finite() || *value < 0.0 || *value > 1.0) {
            return Err("border width and color must be finite and within range");
        }
        self.visuals.border = BorderConfig { width, color };
        Ok(self)
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
    use super::CompositorConfig;

    #[test]
    fn defaults_preserve_current_visual_semantics() {
        let visuals = CompositorConfig::defaults().visuals;
        assert_eq!(visuals.corner_radius, 0.0);
        assert_eq!(visuals.border.width, 0.0);
        let shadow = visuals.shadow;
        assert!(!shadow.enabled);
        assert_eq!(shadow.offset_x, 0.0);
        assert_eq!(shadow.offset_y, 0.0);
        assert_eq!(shadow.extent, 0.0);
        assert_eq!(shadow.strength, 0.0);
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
        assert_eq!(CompositorConfig::defaults().visuals.border.color, [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(CompositorConfig::parse_color("#336699cc").unwrap(), [0.2, 0.4, 0.6, 0.8]);
        assert!(CompositorConfig::parse_color("xyz").is_err());
    }

}
