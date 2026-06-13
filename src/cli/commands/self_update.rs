//! `ta self-update` - replace the running binary with the latest GitHub release.
//!
//! Downloads this platform's prebuilt `.tar.gz` from the newest release and swaps
//! it over `current_exe()` in place (via `self_replace`), so the binary you
//! actually run is the one that gets updated - not some other copy on PATH.
//! `--check` only reports current-vs-latest; `--force` reinstalls even when
//! already current. Platforms without a prebuilt asset are pointed at
//! `cargo install taska`.

use self_update::backends::github::{ReleaseList, Update};

use crate::error::DynError;

const REPO_OWNER: &str = "justpresident";
const REPO_NAME: &str = "taska";
const BIN: &str = "ta";
const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// The release-asset target triple for this platform, matching what the release
/// workflow ships (static musl on Linux `x86_64`, both arches on macOS). `None`
/// means there's no prebuilt binary - the caller falls back to `cargo install`.
fn release_target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        _ => None,
    }
}

/// The "no prebuilt for this platform" message, pointing at the cargo route.
fn no_prebuilt() -> DynError {
    format!(
        "no prebuilt `{BIN}` binary for this platform ({} {}); update with `cargo install {REPO_NAME}` instead",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
    .into()
}

/// `ta self-update [--check] [--force]`.
pub fn cmd_self_update(check: bool, force: bool) -> Result<(), DynError> {
    // Report the running binary first, so even an offline `--check` says what you
    // have before the network lookup of what's available.
    let exe = std::env::current_exe()?;
    println!("{BIN} {CURRENT} (running from {})", exe.display());

    let target = release_target().ok_or_else(no_prebuilt)?;

    // Resolve the latest release and its asset for this platform once - both
    // `--check` and the real update need the version, and the asset's own name
    // carries the path to the binary nested inside the tarball.
    let releases = ReleaseList::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .build()?
        .fetch()?;
    let latest = releases.first().ok_or("no published releases found")?;
    let asset = latest.asset_for(target, None).ok_or_else(|| -> DynError {
        format!(
            "the latest release (v{}) ships no asset for {target}; update with `cargo install {REPO_NAME}`",
            latest.version
        )
        .into()
    })?;
    let up_to_date = !self_update::version::bump_is_greater(CURRENT, &latest.version)?;

    if check {
        if up_to_date {
            println!("up to date - {} is the latest release", latest.version);
        } else {
            println!(
                "update available: {CURRENT} -> {} (run `{BIN} self-update`)",
                latest.version
            );
        }
        return Ok(());
    }

    if up_to_date && !force {
        println!("already up to date ({CURRENT}); use --force to reinstall");
        return Ok(());
    }

    // The release workflow nests the binary as `<asset-stem>/ta` (see
    // .github/workflows/release.yml); derive that path from the asset's own name
    // so the tag is never a second source of truth.
    let stem = asset.name.strip_suffix(".tar.gz").unwrap_or(&asset.name);
    let bin_path_in_archive = format!("{stem}/{BIN}");

    // `--force` re-downloads even at the same version by claiming we're older.
    let current = if force { "0.0.0" } else { CURRENT };

    println!("updating {CURRENT} -> {} ...", latest.version);
    let status = Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name(BIN)
        .target(target)
        .bin_path_in_archive(&bin_path_in_archive)
        .current_version(current)
        .show_download_progress(true)
        .no_confirm(true)
        .build()?
        .update()
        .map_err(|e| -> DynError {
            format!(
                "update failed: {e}\nIf {} is not writable, re-run with the right permissions \
                 (e.g. `sudo`), or reinstall via the install script / `cargo install {REPO_NAME}`.",
                exe.display()
            )
            .into()
        })?;

    if status.updated() {
        println!("updated `{BIN}` to {}", status.version());
        // A stale duplicate elsewhere on PATH would still shadow the fresh copy.
        crate::cli::warn_shadowed_binaries();
    } else {
        println!("already up to date ({})", status.version());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn release_target_matches_the_shipped_assets() {
        // The mapping must name a triple the release workflow actually builds.
        let shipped = [
            "x86_64-unknown-linux-musl",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
        ];
        if let Some(t) = release_target() {
            assert!(shipped.contains(&t), "unshipped target {t}");
        }
        // On this Linux x86_64 CI host the mapping is concrete.
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        assert_eq!(release_target(), Some("x86_64-unknown-linux-musl"));
    }

    #[test]
    fn bin_path_is_derived_from_the_asset_name() {
        // Mirrors the derivation in `cmd_self_update`: <asset-stem>/ta.
        let asset = "ta-v1.0.0-x86_64-unknown-linux-musl.tar.gz";
        let stem = asset.strip_suffix(".tar.gz").unwrap_or(asset);
        assert_eq!(
            format!("{stem}/{BIN}"),
            "ta-v1.0.0-x86_64-unknown-linux-musl/ta"
        );
    }
}
