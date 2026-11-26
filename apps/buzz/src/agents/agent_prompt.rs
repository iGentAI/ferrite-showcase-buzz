//! The Ferrite base agent instructions.

/// The default base instructions (no variables).
pub const BASE_INSTRUCTIONS: &str = include_str!("templates/agent/base_instructions.md");

/// The base instructions for a session, honoring an optional config override
/// (resolution order: config override → default).
pub fn base_instructions_for(config_override: Option<&str>) -> &str {
    match config_override {
        Some(s) if !s.trim().is_empty() => s,
        _ => BASE_INSTRUCTIONS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_takes_precedence_else_default() {
        assert_eq!(base_instructions_for(None), BASE_INSTRUCTIONS);
        assert_eq!(base_instructions_for(Some("  ")), BASE_INSTRUCTIONS);
        assert_eq!(base_instructions_for(Some("custom")), "custom");
    }

    #[test]
    fn retains_high_value_sections() {
        let p = BASE_INSTRUCTIONS;
        assert!(p.contains("## Planning"));
        assert!(p.contains("apply_patch"));
        assert!(p.contains("*** Begin Patch"));
        assert!(p.contains("do NOT have a shell"));
    }
}
