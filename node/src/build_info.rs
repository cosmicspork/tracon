//! Which build of a release this binary is. The version alone does not say:
//! an unreleased build of `main` carries the version of the release before it.

/// The commit this node was built from, or `None` for a release (and for a
/// build from a tree without git). See `build.rs`.
pub fn build() -> Option<&'static str> {
    Some(env!("TRACON_BUILD")).filter(|id| !id.is_empty())
}
