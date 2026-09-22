//! The OpenCode model catalogue module against `Config::try_load_from`: the
//! generic, any-provider-name half of `default_models`'s tier 1.
//!
//! `node/src/config.rs`'s own test module already proves the pure tiered
//! lookup (`default_models_tiered`) and the three built-in providers'
//! empty-models fill (`built_in_providers_declare_models_unless_the_operator_does`).
//! What is left is the case the catalogue rework is actually for: a *custom*
//! provider name, one none of the three built-ins name, still gets real
//! defaults when the catalogue happens to know it.
//!
//! This lives in its own file rather than alongside
//! `opencode_providers.rs`'s `the_pinned_catalogue_is_exactly_the_declared_models`
//! deliberately: `models_catalogue`'s in-memory cache is a
//! `std::sync::OnceLock` global to the whole test binary, and seeding it here
//! would otherwise race every other test in that much larger file that also
//! calls `default_models`/`Config::try_load_from` expecting the hardcoded
//! fallback. Kept to itself, this file is free to install a fake catalogue
//! without any such interference.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::collections::BTreeMap;

use tracon::config::{Config, ModelDecl};
use tracon::models_catalogue::{self, Catalogue};

#[test]
fn a_custom_providers_empty_models_are_filled_from_the_catalogue() {
    state::isolate();

    let mut providers = BTreeMap::new();
    providers.insert(
        "openrouter".to_string(),
        vec![ModelDecl {
            id: "meta/llama".into(),
            name: "Llama".into(),
            context: 128_000,
            output: 8_000,
            reasoning: false,
            attachment: false,
        }],
    );
    models_catalogue::install_for_test(Catalogue {
        fetched_ms: Some(1_700_000_000_000),
        providers,
    });

    let dir = std::env::temp_dir().join(format!("tracon-cfg-catalogue-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("node.toml");
    std::fs::write(
        &path,
        r#"
[providers.openrouter]
credential = "openrouter"
upstream = "https://openrouter.ai/api/v1"
shape = "openai"
"#,
    )
    .unwrap();

    let config = Config::try_load_from(&path).unwrap();
    let models = &config.providers["openrouter"].models;
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "meta/llama");
    assert_eq!(models[0].name, "Llama");

    // A provider the catalogue does not know at all still falls through to
    // tier 3 — empty, not a panic or a borrowed default from another entry.
    std::fs::write(
        &path,
        r#"
[providers.totally-unknown]
credential = "x"
upstream = "https://example.com"
shape = "openai"
"#,
    )
    .unwrap();
    let config = Config::try_load_from(&path).unwrap();
    assert!(config.providers["totally-unknown"].models.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}
