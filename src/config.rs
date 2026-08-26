#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CompositorConfig {
    pub(crate) visuals: VisualConfig,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct VisualConfig {
    pub(crate) corner_radius: f32,
    pub(crate) shadow: ShadowConfig,
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
            shadow: ShadowConfig::default(),
        }
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
}

#[cfg(test)]
mod tests {
    use super::CompositorConfig;

    #[test]
    fn defaults_preserve_current_visual_semantics() {
        let visuals = CompositorConfig::defaults().visuals;
        assert_eq!(visuals.corner_radius, 0.0);
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

}
