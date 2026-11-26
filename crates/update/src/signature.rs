//! The identity and version of a verified macOS app bundle.

use std::path::Path;

use crate::{UpdateError, Version};

pub(crate) struct SignatureInfo {
    pub(crate) team_id: String,
    pub(crate) bundle_id: String,
    pub(crate) version: Version,
}

pub(crate) fn running_signature(
    bundle: &Path,
    signature: impl FnOnce(&Path) -> Option<SignatureInfo>,
) -> Result<SignatureInfo, UpdateError> {
    signature(bundle).ok_or(UpdateError::NoUpdateSigner)
}

pub(crate) fn require_newer(version: Version, current: Version) -> Result<(), UpdateError> {
    if version <= current {
        return Err(UpdateError::UpdateNotNewer { version, current });
    }
    Ok(())
}

pub(crate) fn verify_update(
    running: &SignatureInfo,
    bundle: &Path,
    expected: Version,
    signature: impl FnOnce(&Path) -> Option<SignatureInfo>,
) -> Result<(), UpdateError> {
    let candidate = signature(bundle).ok_or(UpdateError::InvalidUpdateSignature)?;
    if candidate.team_id != running.team_id {
        return Err(UpdateError::UpdateTeamMismatch);
    }
    if candidate.bundle_id != running.bundle_id {
        return Err(UpdateError::UpdateBundleMismatch);
    }
    require_newer(candidate.version, running.version)?;
    if candidate.version != expected {
        return Err(UpdateError::UpdateVersionMismatch);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn codesign_info(bundle: &Path, require_notarization: bool) -> Option<SignatureInfo> {
    use std::process::Command;

    let display = Command::new("/usr/bin/codesign")
        .args(["--display", "--verbose=4"])
        .arg(bundle)
        .output()
        .ok()?;
    if !display.status.success() {
        return None;
    }
    let display = std::str::from_utf8(&display.stderr).ok()?;
    let team_id = display
        .lines()
        .find_map(|line| line.strip_prefix("TeamIdentifier="))?;
    if team_id.len() != 10
        || !team_id
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    {
        return None;
    }

    if !verify_signature(
        bundle,
        &developer_id_requirement(team_id, require_notarization),
        true,
    ) {
        return None;
    }

    // Strict verification seals Info.plist. Read the OS bundle identifier, not the
    // independently configurable code-signing identifier.
    let plist = bundle.join("Contents/Info.plist");
    let plist_string = |key: &str| {
        let output = Command::new("/usr/bin/plutil")
            .args(["-extract", key, "raw", "-expect", "string", "-n", "-o", "-"])
            .arg(&plist)
            .output()
            .ok()?;
        output.status.success().then_some(())?;
        String::from_utf8(output.stdout).ok()
    };
    let bundle_id = plist_string("CFBundleIdentifier")?;
    if bundle_id.is_empty() {
        return None;
    }
    let version = plist_string("CFBundleShortVersionString")?
        .trim()
        .parse()
        .ok()?;
    Some(SignatureInfo {
        team_id: team_id.to_owned(),
        bundle_id,
        version,
    })
}

/// Bind the signature to an Apple-issued Developer ID Application certificate for this team.
#[cfg(target_os = "macos")]
fn developer_id_requirement(team_id: &str, require_notarization: bool) -> String {
    let mut requirement = format!(
        "=anchor apple generic \
        and certificate 1[field.1.2.840.113635.100.6.2.6] exists \
        and certificate leaf[field.1.2.840.113635.100.6.1.13] exists \
        and certificate leaf[subject.OU] = \"{team_id}\""
    );
    if require_notarization {
        requirement.push_str(" and notarized");
    }
    requirement
}

/// Strict verification covers the signed disk image. Apps also verify all nested code.
#[cfg(target_os = "macos")]
fn verify_signature(path: &Path, requirement: &str, deep: bool) -> bool {
    use std::process::Command;

    let mut command = Command::new("/usr/bin/codesign");
    command.args(["--verify", "--strict", "-R", requirement]);
    if deep {
        command.arg("--deep");
    }
    // Keep command output private: it can contain local paths and signer names.
    command
        .arg(path)
        .output()
        .is_ok_and(|output| output.status.success())
}

#[cfg(target_os = "macos")]
pub(crate) fn verify_disk_image(image: &Path, running: &SignatureInfo) -> bool {
    verify_signature(
        image,
        &developer_id_requirement(&running.team_id, true),
        false,
    )
}

#[cfg(target_os = "macos")]
pub(crate) fn gatekeeper_accepts(bundle: &Path) -> bool {
    std::process::Command::new("/usr/sbin/spctl")
        .args([
            "--assess",
            "--type",
            "execute",
            "--ignore-cache",
            "--no-cache",
        ])
        .arg(bundle)
        .output()
        .is_ok_and(|output| output.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn adds_notarization_to_the_developer_id_requirement_for_downloads() {
        let baseline = "=anchor apple generic \
            and certificate 1[field.1.2.840.113635.100.6.2.6] exists \
            and certificate leaf[field.1.2.840.113635.100.6.1.13] exists \
            and certificate leaf[subject.OU] = \"ABCDEFGHIJ\"";
        assert_eq!(developer_id_requirement("ABCDEFGHIJ", false), baseline);
        assert_eq!(
            developer_id_requirement("ABCDEFGHIJ", true),
            format!("{baseline} and notarized")
        );
    }

    fn info(team: &str, id: &str, version: &str) -> SignatureInfo {
        SignatureInfo {
            team_id: team.into(),
            bundle_id: id.into(),
            version: version.parse().unwrap(),
        }
    }

    fn check(candidate: Option<SignatureInfo>) -> Result<(), UpdateError> {
        verify_update(
            &info("TEAM", "app", "0.1.1"),
            Path::new("New.app"),
            "0.1.2".parse().unwrap(),
            |_| candidate,
        )
    }

    #[test]
    fn accepts_the_same_team_and_bundle_at_a_newer_version() {
        assert!(check(Some(info("TEAM", "app", "0.1.2"))).is_ok());
    }

    #[test]
    fn refuses_a_different_team() {
        assert!(matches!(
            check(Some(info("OTHER", "app", "0.1.2"))),
            Err(UpdateError::UpdateTeamMismatch)
        ));
    }

    #[test]
    fn refuses_an_unsigned_or_invalid_candidate() {
        assert!(matches!(
            check(None),
            Err(UpdateError::InvalidUpdateSignature)
        ));
    }

    #[test]
    fn only_offers_the_release_page_without_a_running_signer() {
        assert!(matches!(
            running_signature(Path::new("AdHoc.app"), |_| None),
            Err(UpdateError::NoUpdateSigner)
        ));
    }

    #[test]
    fn refuses_a_different_bundle_id() {
        assert!(matches!(
            check(Some(info("TEAM", "other", "0.1.2"))),
            Err(UpdateError::UpdateBundleMismatch)
        ));
    }

    #[test]
    fn refuses_a_relabelled_downgrade_or_same_version() {
        for version in ["0.1.0", "0.1.1"] {
            assert!(matches!(
                check(Some(info("TEAM", "app", version))),
                Err(UpdateError::UpdateNotNewer { .. })
            ));
        }
    }

    #[test]
    fn refuses_a_signed_version_that_disagrees_with_the_release() {
        assert!(matches!(
            check(Some(info("TEAM", "app", "0.1.3"))),
            Err(UpdateError::UpdateVersionMismatch)
        ));
    }
}
