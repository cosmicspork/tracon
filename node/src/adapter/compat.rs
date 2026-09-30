//! Which harness a model's credential can run on. The harness is a
//! consequence of the credential, not a setting: an Anthropic subscription
//! is Claude Code's alone, every other provider is OpenCode's, and only an
//! Anthropic API key runs on either, where `[harness] id` breaks the tie.

use crate::broker::KIND_OAUTH;
use crate::config::{SHAPE_ANTHROPIC, SHAPE_OPENAI_CODEX};

use super::{claude::ClaudeAdapter, opencode::OpenCodeAdapter, KNOWN};

/// What a model runs on, as far as choosing a harness goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialClass {
    /// An Anthropic sign-in (`anthropic` shape, `oauth` kind).
    AnthropicSubscription,
    /// An Anthropic API key.
    AnthropicKey,
    /// A ChatGPT/Codex sign-in.
    CodexSubscription,
    /// Any other provider: an OpenAI key, a self-hosted server.
    OtherProvider,
    /// A model named without a provider (`sonnet`, `claude-opus-5`): Claude
    /// Code's own spelling, which OpenCode cannot address.
    BareAlias,
}

impl CredentialClass {
    pub fn classify(shape: &str, kind: &str) -> Self {
        match shape {
            SHAPE_ANTHROPIC if kind == KIND_OAUTH => Self::AnthropicSubscription,
            SHAPE_ANTHROPIC => Self::AnthropicKey,
            SHAPE_OPENAI_CODEX => Self::CodexSubscription,
            _ => Self::OtherProvider,
        }
    }

    /// Whether a session on `harness_id` may run on this credential, and the
    /// reason it may not. A harness this table does not know (a test's fake)
    /// is not second-guessed.
    pub fn fits(self, harness_id: &str) -> Result<(), &'static str> {
        match (self, harness_id) {
            (Self::AnthropicSubscription, OpenCodeAdapter::ID) => Err(
                "an Anthropic subscription runs on Claude Code only; through any other \
                 client Anthropic bills it as extra usage",
            ),
            (Self::BareAlias, OpenCodeAdapter::ID) => {
                Err("OpenCode needs a provider-qualified model (`provider/model`)")
            }
            (Self::CodexSubscription | Self::OtherProvider, ClaudeAdapter::ID) => {
                Err("Claude Code runs Anthropic models only")
            }
            _ => Ok(()),
        }
    }

    /// The supported harnesses this credential runs on, in `KNOWN` order.
    pub fn harnesses(self) -> Vec<&'static str> {
        KNOWN
            .iter()
            .copied()
            .filter(|id| self.fits(id).is_ok())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::KIND_API_KEY;
    use crate::config::SHAPE_OPENAI;

    #[test]
    fn the_credential_decides_the_harness_except_for_an_anthropic_key() {
        use CredentialClass::*;
        for (class, harnesses) in [
            (AnthropicSubscription, vec!["claude"]),
            (AnthropicKey, vec!["opencode", "claude"]),
            (CodexSubscription, vec!["opencode"]),
            (OtherProvider, vec!["opencode"]),
            (BareAlias, vec!["claude"]),
        ] {
            assert_eq!(class.harnesses(), harnesses, "{class:?}");
        }
    }

    #[test]
    fn a_class_comes_from_the_shape_and_the_kind() {
        use CredentialClass::*;
        assert_eq!(
            CredentialClass::classify(SHAPE_ANTHROPIC, KIND_OAUTH),
            AnthropicSubscription
        );
        assert_eq!(
            CredentialClass::classify(SHAPE_ANTHROPIC, KIND_API_KEY),
            AnthropicKey
        );
        assert_eq!(
            CredentialClass::classify(SHAPE_OPENAI_CODEX, KIND_OAUTH),
            CodexSubscription
        );
        assert_eq!(
            CredentialClass::classify(SHAPE_OPENAI, KIND_API_KEY),
            OtherProvider
        );
    }

    #[test]
    fn an_unknown_harness_is_not_second_guessed() {
        assert!(CredentialClass::AnthropicSubscription.fits("fake").is_ok());
        assert!(CredentialClass::OtherProvider.fits("fake").is_ok());
    }
}
