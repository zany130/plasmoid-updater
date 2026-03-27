// SPDX-License-Identifier: GPL-3.0-or-later
//
// Installation logic based on Apdatifier (https://github.com/exequtic/apdatifier) - MIT License
// and KDE Discover (https://invent.kde.org/plasma/discover) -
// GPL-2.0-only OR GPL-3.0-only OR LicenseRef-KDE-Accepted-GPL

use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::installer::privilege;
use crate::{
    types::{ComponentType, InstalledComponent},
    {Error, Result},
};

const COLOR_SCHEME_EXTENSIONS: &[&str] = &[".colors", ".colorscheme"];
const IMAGE_EXTENSIONS: &[&str] = &[".jpg", ".jpeg", ".png", ".webp", ".svg"];

// --- Generic Recursive Directory Search ---

/// Recursively searches a directory tree for an entry matching the predicate.
///
/// The predicate is tested against each directory. If it returns `true`,
/// that directory's path is returned. Otherwise, subdirectories are searched.
fn find_in_dir<F>(dir: &Path, predicate: F) -> Option<PathBuf>
where
    F: Fn(&Path) -> bool + Copy,
{
    if predicate(dir) {
        return Some(dir.to_path_buf());
    }

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir()
                && let Some(found) = find_in_dir(&path, predicate)
            {
                return Some(found);
            }
        }
    }

    None
}

/// Recursively searches for files (not directories) matching a predicate.
fn find_file_in_dir<F>(dir: &Path, predicate: F) -> Option<PathBuf>
where
    F: Fn(&Path) -> bool + Copy,
{
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && predicate(&path) {
                return Some(path);
            } else if path.is_dir()
                && let Some(found) = find_file_in_dir(&path, predicate)
            {
                return Some(found);
            }
        }
    }
    None
}

// --- Utility Functions ---

fn replace_destination<F>(dest: &Path, action: F) -> Result<()>
where
    F: FnOnce() -> Result<()>,
{
    if dest.exists() {
        if dest.is_dir() {
            privilege::remove_dir_all(dest)?;
        } else {
            privilege::remove_file(dest)?;
        }
    }

    if let Some(parent) = dest.parent() {
        privilege::create_dir_all(parent)?;
    }

    action()
}

// --- Metadata ---

pub(super) fn find_package_dir(extract_dir: &Path) -> Option<PathBuf> {
    if let Some(dir) = find_in_dir(extract_dir, |d| d.join("metadata.json").exists()) {
        return Some(dir);
    }

    find_in_dir(extract_dir, |d| d.join("metadata.desktop").exists())
}

/// Patches a `metadata.json` file to update the version and KPackageStructure fields.
pub(super) fn patch_metadata(
    metadata_path: &Path,
    component_type: ComponentType,
    new_version: &str,
) -> Result<()> {
    let content = fs::read_to_string(metadata_path)?;
    let mut json: serde_json::Value =
        serde_json::from_str(&content).map_err(Error::MetadataParse)?;

    if let Some(kpackage_type) = component_type.kpackage_type() {
        json["KPackageStructure"] = serde_json::Value::String(kpackage_type.to_string());
    }

    if let Some(kplugin) = json.get_mut("KPlugin") {
        kplugin["Version"] = serde_json::Value::String(new_version.to_string());
    }

    let patched = serde_json::to_string_pretty(&json)?;
    privilege::write_file(metadata_path, patched.as_bytes())?;

    Ok(())
}

/// Patches a `metadata.desktop` file to update the `X-KDE-PluginInfo-Version` field.
pub(super) fn patch_metadata_desktop(metadata_path: &Path, new_version: &str) -> Result<()> {
    let content = fs::read_to_string(metadata_path)?;
    let mut found = false;
    let patched: String = content
        .lines()
        .map(|line| {
            if line.starts_with("X-KDE-PluginInfo-Version=") {
                found = true;
                format!("X-KDE-PluginInfo-Version={new_version}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    // Preserve trailing newline if original had one
    let patched = if content.ends_with('\n') && !patched.ends_with('\n') {
        patched + "\n"
    } else {
        patched
    };

    if !found {
        log::debug!(target: "patch", "no X-KDE-PluginInfo-Version field in {}", metadata_path.display());
        return Ok(());
    }

    privilege::write_file(metadata_path, patched.as_bytes())?;
    Ok(())
}

/// Returns `true` when `error` is an `InstallFailed` whose entire kpackagetool6
/// output consists exclusively of `KPackageStructure … does not match requested
/// format` lines.  These messages are emitted for *other* packages in the same
/// directory and indicate that sibling plasmoids have missing or wrong
/// `KPackageStructure` entries — not that the target package itself is bad.
///
/// The check relies on a substring of kpackagetool6's output. If a future
/// kpackagetool6 release changes this wording, the detection will simply not
/// trigger and the original error will be reported instead — a safe default.
fn is_sibling_kpackage_structure_error(error: &Error) -> bool {
    let Error::InstallFailed(msg) = error else {
        return false;
    };

    let body = msg
        .strip_prefix("kpackagetool6 failed: ")
        .unwrap_or(msg.as_str());

    let mut has_lines = false;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        has_lines = true;
        if !trimmed.contains("does not match requested format") {
            return false;
        }
    }
    has_lines
}

/// Attempts to add a missing `KPackageStructure` field to `metadata.json`.
///
/// Only writes to disk when the field is clearly absent (not present, or
/// null/empty). Returns `Ok(true)` if the file was patched, `Ok(false)` if
/// the field was already present and no change was needed.
fn add_kpackage_structure_if_missing(
    metadata_path: &Path,
    kpackage_type: &str,
    component_name: &str,
) -> Result<bool> {
    let content = fs::read_to_string(metadata_path)?;
    let mut json: serde_json::Value =
        serde_json::from_str(&content).map_err(Error::MetadataParse)?;

    let needs_patch = match json.get("KPackageStructure") {
        None => true,
        Some(v) => v.as_str().map(|s| s.is_empty()).unwrap_or(true),
    };

    if !needs_patch {
        return Ok(false);
    }

    json["KPackageStructure"] = serde_json::Value::String(kpackage_type.to_string());
    let patched = serde_json::to_string_pretty(&json)?;
    privilege::write_file(metadata_path, patched.as_bytes())?;

    log::info!(
        target: "repair",
        "added KPackageStructure = {kpackage_type:?} to {} ({})",
        metadata_path.display(),
        component_name,
    );

    Ok(true)
}

/// Scans a list of installed components and adds the correct `KPackageStructure`
/// field to any `metadata.json` that is missing it.
///
/// This is the implementation for **repair mode** — it must not be called during
/// the normal update flow.  Only components that use `kpackagetool6` (i.e., have
/// a `kpackage_type()`) are considered.  The field is only added when it is
/// clearly absent; existing values are never overwritten.
///
/// Returns a pair of `(patched_names, error_pairs)` where `patched_names` is the
/// list of component names whose files were changed, and `error_pairs` is a list
/// of `(component_name, error_message)` for files that could not be read or written.
pub(crate) fn repair_kpackage_structures(
    components: &[crate::types::InstalledComponent],
) -> (Vec<String>, Vec<(String, String)>) {
    let mut patched = Vec::new();
    let mut errors = Vec::new();

    for component in components {
        let Some(kpackage_type) = component.component_type.kpackage_type() else {
            continue;
        };

        let metadata_path = component.path.join("metadata.json");
        if !metadata_path.exists() {
            continue;
        }

        match add_kpackage_structure_if_missing(&metadata_path, kpackage_type, &component.name) {
            Ok(true) => patched.push(component.name.clone()),
            Ok(false) => {}
            Err(e) => errors.push((
                component.name.clone(),
                format!("failed to patch {}: {e}", metadata_path.display()),
            )),
        }
    }

    (patched, errors)
}

/// Installs or updates a component package using `kpackagetool6`.
fn install_via_kpackagetool(
    package_dir: &Path,
    component_type: ComponentType,
    global: bool,
) -> Result<()> {
    let kpackage_type = component_type
        .kpackage_type()
        .ok_or_else(|| Error::install(format!("{component_type} has no kpackage type")))?;

    let mut cmd = if global {
        privilege::sudo_command("kpackagetool6")
    } else {
        std::process::Command::new("kpackagetool6")
    };
    cmd.args(["-t", kpackage_type]);

    if global {
        cmd.arg("--global");
    }

    cmd.args(["-u", &package_dir.to_string_lossy()]);

    let output = cmd
        .output()
        .map_err(|e| Error::install(format!("failed to run kpackagetool6: {e}")))?;

    if !output.status.success() {
        // kpackagetool6 sends KPackageStructure warnings to stdout on some versions,
        // stderr on others. Combine both so the caller can inspect the full output.
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = stdout.trim();
        let stderr = stderr.trim();
        let combined = if !stdout.is_empty() && !stderr.is_empty() {
            format!("{stdout}\n{stderr}")
        } else if !stdout.is_empty() {
            stdout.to_string()
        } else if !stderr.is_empty() {
            stderr.to_string()
        } else {
            "(no output)".to_string()
        };
        return Err(Error::install(format!("kpackagetool6 failed: {combined}")));
    }

    Ok(())
}

/// Installs or updates a component using kpackagetool, with metadata patching.
pub(super) fn install_via_kpackage(
    extract_dir: &Path,
    component: &InstalledComponent,
    new_version: &str,
) -> Result<()> {
    let package_dir = find_package_dir(extract_dir).ok_or(Error::MetadataNotFound)?;

    let metadata_json = package_dir.join("metadata.json");
    let metadata_desktop = package_dir.join("metadata.desktop");

    if metadata_json.exists()
        && let Err(e) = patch_metadata(&metadata_json, component.component_type, new_version)
    {
        log::warn!(target: "patch", "failed for {}: {e}", component.name);
    }

    if metadata_desktop.exists()
        && let Err(e) = patch_metadata_desktop(&metadata_desktop, new_version)
    {
        log::warn!(target: "patch", "failed to patch metadata.desktop for {}: {e}", component.name);
    }

    // Intentionally NOT scanning sibling packages here: silently rewriting files
    // that belong to other plasmoids the user did not ask to update is unexpected
    // behaviour.  If kpackagetool6 reports KPackageStructure errors for sibling
    // packages, a clear error is returned below instead.  Use
    // `plasmoid-updater repair` to fix broken packages explicitly.

    let is_global = privilege::is_system_path(&component.path);
    match install_via_kpackagetool(&package_dir, component.component_type, is_global) {
        Ok(()) => Ok(()),
        Err(e) if is_sibling_kpackage_structure_error(&e) => {
            // kpackagetool6 rejected the update solely because one or more
            // *other* installed packages in the same directory are missing the
            // KPackageStructure field.  Rather than silently patching unrelated
            // plasmoids, we surface a descriptive error so the user can decide
            // whether to run repair mode.
            let err_str = e.to_string();
            let kpackage_output = err_str
                .strip_prefix("installation failed: ")
                .unwrap_or(&err_str);
            Err(Error::install(format!(
                "update blocked: one or more installed plasmoids in your plasma install \
                 directory have a missing or incorrect KPackageStructure field in their \
                 metadata.json. kpackagetool6 validates all packages in the same directory \
                 and rejects updates when any of them are malformed. \
                 Run 'plasmoid-updater repair' to add the missing field to affected \
                 packages, then retry the update.\n\
                 kpackagetool6 output: {kpackage_output}"
            )))
        }
        Err(e) => Err(e),
    }
}

// --- Component Locators ---

/// Locates a color scheme file in an archive directory.
fn locate_color_scheme_file(dir: &Path) -> Option<PathBuf> {
    find_file_in_dir(dir, |path| {
        path.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| {
                COLOR_SCHEME_EXTENSIONS
                    .iter()
                    .any(|ext| name.ends_with(ext))
            })
    })
}

/// Finds the root directory of a component within an extracted archive.
fn find_component_root_in_archive(
    extract_dir: &Path,
    component_type: ComponentType,
) -> Option<PathBuf> {
    if has_component_structure(extract_dir, component_type) {
        return Some(extract_dir.to_path_buf());
    }

    if let Ok(entries) = fs::read_dir(extract_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && has_component_structure(&path, component_type) {
                return Some(path);
            }
        }
    }

    None
}

fn has_component_structure(dir: &Path, component_type: ComponentType) -> bool {
    match component_type {
        ComponentType::AuroraeDecoration => {
            dir.join("decoration.svg").exists() || dir.join("aurorae").exists()
        }
        ComponentType::GlobalTheme | ComponentType::SplashScreen => {
            dir.join("metadata.json").exists() || dir.join("metadata.desktop").exists()
        }
        ComponentType::PlasmaStyle => {
            dir.join("colors").exists()
                || dir.join("widgets").exists()
                || dir.join("metadata.desktop").exists()
        }
        ComponentType::SddmTheme => {
            dir.join("theme.conf").exists() || dir.join("Main.qml").exists()
        }
        ComponentType::KWinSwitcher => {
            dir.join("metadata.json").exists() || dir.join("contents").exists()
        }
        _ => false,
    }
}

fn find_icon_theme_dir(extract_dir: &Path) -> Option<PathBuf> {
    find_in_dir(extract_dir, |d| d.join("index.theme").exists())
}

fn find_wallpaper_source(extract_dir: &Path) -> Option<PathBuf> {
    // directory-based wallpaper (with contents/ or metadata.json)
    if let Some(dir) = find_in_dir(extract_dir, |d| {
        d.join("contents").exists() || d.join("metadata.json").exists()
    }) {
        return Some(dir);
    }

    // single-file wallpaper (image file)
    if let Ok(entries) = fs::read_dir(extract_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file()
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
            {
                let lower = name.to_lowercase();
                if IMAGE_EXTENSIONS.iter().any(|ext| lower.ends_with(ext)) {
                    return Some(path);
                }
            }
        }
    }

    None
}

// --- Direct Installation Methods ---

/// Installs a component using direct file operations (not kpackagetool).
pub(super) fn install_direct(extract_dir: &Path, component: &InstalledComponent) -> Result<()> {
    match component.component_type {
        ComponentType::ColorScheme => install_color_scheme(extract_dir, &component.path),
        ComponentType::IconTheme => install_icon_theme(extract_dir, &component.path),
        ComponentType::Wallpaper => install_wallpaper(extract_dir, component),
        ComponentType::AuroraeDecoration
        | ComponentType::GlobalTheme
        | ComponentType::PlasmaStyle
        | ComponentType::SplashScreen
        | ComponentType::SddmTheme => {
            install_theme_dir(extract_dir, &component.path, component.component_type)
        }
        _ => Err(Error::install(format!(
            "{} should use kpackagetool",
            component.component_type
        ))),
    }
}

fn install_color_scheme(extract_dir: &Path, dest_path: &Path) -> Result<()> {
    let color_file = locate_color_scheme_file(extract_dir)
        .ok_or_else(|| Error::install("no color scheme file found in archive"))?;

    replace_destination(dest_path, || {
        privilege::copy_file(&color_file, dest_path)?;
        log::debug!(target: "install", "copied color scheme to {}", dest_path.display());
        Ok(())
    })
}

fn install_icon_theme(extract_dir: &Path, dest_dir: &Path) -> Result<()> {
    let source_dir = find_icon_theme_dir(extract_dir)
        .ok_or_else(|| Error::install("no icon theme (index.theme) found in archive"))?;

    replace_destination(dest_dir, || {
        privilege::create_dir_all(dest_dir)?;
        privilege::copy_dir(&source_dir, dest_dir)?;
        log::debug!(target: "install", "copied icon theme to {}", dest_dir.display());
        Ok(())
    })
}

fn install_wallpaper(extract_dir: &Path, component: &InstalledComponent) -> Result<()> {
    let source = find_wallpaper_source(extract_dir)
        .ok_or_else(|| Error::install("no wallpaper found in archive"))?;

    let dest = &component.path;

    if source.is_file() {
        replace_destination(dest, || {
            privilege::copy_file(&source, dest)?;
            log::debug!(target: "install", "copied wallpaper to {}", dest.display());
            Ok(())
        })
    } else {
        replace_destination(dest, || {
            privilege::create_dir_all(dest)?;
            privilege::copy_dir(&source, dest)?;
            log::debug!(target: "install", "copied wallpaper dir to {}", dest.display());
            Ok(())
        })
    }
}

fn install_theme_dir(
    extract_dir: &Path,
    dest_dir: &Path,
    component_type: ComponentType,
) -> Result<()> {
    let source_dir =
        find_component_root_in_archive(extract_dir, component_type).ok_or_else(|| {
            Error::install(format!(
                "no valid {component_type} structure found in archive"
            ))
        })?;

    replace_destination(dest_dir, || {
        privilege::create_dir_all(dest_dir)?;
        privilege::copy_dir(&source_dir, dest_dir)?;
        log::debug!(target: "install", "copied {} to {}", component_type, dest_dir.display());
        Ok(())
    })
}

/// Returns `true` if the path is a single-file component (e.g., color scheme file, image).
pub(super) fn is_single_file_component(path: &Path, component_type: ComponentType) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lower = name.to_lowercase();

    match component_type {
        ComponentType::ColorScheme => COLOR_SCHEME_EXTENSIONS
            .iter()
            .any(|ext| lower.ends_with(ext)),
        ComponentType::Wallpaper => IMAGE_EXTENSIONS.iter().any(|ext| lower.ends_with(ext)),
        _ => false,
    }
}

pub(super) fn install_raw_file(downloaded: &Path, component: &InstalledComponent) -> Result<()> {
    let dest = &component.path;

    replace_destination(dest, || {
        privilege::copy_file(downloaded, dest)?;
        log::debug!(target: "install", "copied raw file to {}", dest.display());
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::types::{ComponentType, InstalledComponent};

    fn make_component(name: &str, dir: &std::path::Path, ct: ComponentType) -> InstalledComponent {
        InstalledComponent {
            name: name.to_string(),
            directory_name: name.to_string(),
            version: "1.0".to_string(),
            component_type: ct,
            path: dir.to_path_buf(),
            is_system: false,
            release_date: String::new(),
        }
    }

    // --- is_sibling_kpackage_structure_error ---

    #[test]
    fn sibling_error_detected_single_line() {
        let err = Error::install(
            "kpackagetool6 failed: Package type \"Plasma/Applet\" does not match requested format",
        );
        assert!(is_sibling_kpackage_structure_error(&err));
    }

    #[test]
    fn sibling_error_detected_multi_line() {
        let err = Error::install(
            "kpackagetool6 failed: \
             Package type \"KWin/Script\" does not match requested format\n\
             Package type \"Plasma/Applet\" does not match requested format",
        );
        assert!(is_sibling_kpackage_structure_error(&err));
    }

    #[test]
    fn sibling_error_not_detected_for_other_errors() {
        let err = Error::install("kpackagetool6 failed: some other error");
        assert!(!is_sibling_kpackage_structure_error(&err));
    }

    #[test]
    fn sibling_error_not_detected_for_mixed_output() {
        // When kpackagetool6 output contains BOTH a KPackageStructure line AND
        // an unrelated error, we should NOT identify this as a sibling-only error.
        let err = Error::install(
            "kpackagetool6 failed: \
             Package type \"Plasma/Applet\" does not match requested format\n\
             Error: Could not install package",
        );
        assert!(!is_sibling_kpackage_structure_error(&err));
    }

    #[test]
    fn sibling_error_not_detected_for_empty_body() {
        let err = Error::install("kpackagetool6 failed: ");
        assert!(!is_sibling_kpackage_structure_error(&err));
    }

    #[test]
    fn sibling_error_not_detected_for_wrong_variant() {
        let err = Error::DownloadFailed(
            "does not match requested format".to_string(),
        );
        assert!(!is_sibling_kpackage_structure_error(&err));
    }

    // --- add_kpackage_structure_if_missing ---

    #[test]
    fn adds_kpackage_structure_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata.json");
        std::fs::write(&path, r#"{"KPlugin": {"Name": "Test"}}"#).unwrap();

        let patched = add_kpackage_structure_if_missing(&path, "Plasma/Applet", "Test").unwrap();
        assert!(patched);

        let content = std::fs::read_to_string(&path).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(json["KPackageStructure"].as_str(), Some("Plasma/Applet"));
    }

    #[test]
    fn adds_kpackage_structure_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata.json");
        std::fs::write(&path, r#"{"KPackageStructure": ""}"#).unwrap();

        let patched = add_kpackage_structure_if_missing(&path, "Plasma/Applet", "Test").unwrap();
        assert!(patched);
    }

    #[test]
    fn skips_when_kpackage_structure_already_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata.json");
        std::fs::write(&path, r#"{"KPackageStructure": "Plasma/Applet"}"#).unwrap();

        let patched = add_kpackage_structure_if_missing(&path, "Plasma/Applet", "Test").unwrap();
        assert!(!patched);
    }

    #[test]
    fn skips_when_different_kpackage_structure_already_present() {
        // An existing (even "wrong") value should never be overwritten in repair mode.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata.json");
        std::fs::write(&path, r#"{"KPackageStructure": "KWin/Script"}"#).unwrap();

        let patched =
            add_kpackage_structure_if_missing(&path, "Plasma/Applet", "Test").unwrap();
        assert!(!patched);

        let content = std::fs::read_to_string(&path).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(json["KPackageStructure"].as_str(), Some("KWin/Script"));
    }

    // --- repair_kpackage_structures ---

    #[test]
    fn repair_patches_components_missing_field() {
        let dir = tempfile::tempdir().unwrap();

        // PlasmaWidget with missing KPackageStructure
        let widget_dir = dir.path().join("my.widget");
        std::fs::create_dir_all(&widget_dir).unwrap();
        std::fs::write(
            widget_dir.join("metadata.json"),
            r#"{"KPlugin": {"Name": "My Widget", "Version": "1.0"}}"#,
        )
        .unwrap();

        let component = make_component("My Widget", &widget_dir, ComponentType::PlasmaWidget);
        let (patched, errors) = repair_kpackage_structures(&[component]);

        assert_eq!(patched, vec!["My Widget"]);
        assert!(errors.is_empty());

        let content =
            std::fs::read_to_string(widget_dir.join("metadata.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(json["KPackageStructure"].as_str(), Some("Plasma/Applet"));
    }

    #[test]
    fn repair_skips_non_kpackage_components() {
        let dir = tempfile::tempdir().unwrap();

        // ColorScheme has no kpackage_type(), should be skipped entirely.
        let scheme_path = dir.path().join("MyTheme.colors");
        std::fs::write(&scheme_path, "[ColorEffects:Inactive]\n").unwrap();

        let component = make_component("MyTheme", dir.path(), ComponentType::ColorScheme);
        let (patched, errors) = repair_kpackage_structures(&[component]);

        assert!(patched.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn repair_skips_component_with_no_metadata_json() {
        let dir = tempfile::tempdir().unwrap();
        let widget_dir = dir.path().join("no.metadata");
        std::fs::create_dir_all(&widget_dir).unwrap();
        // No metadata.json created.

        let component = make_component("NoMeta", &widget_dir, ComponentType::PlasmaWidget);
        let (patched, errors) = repair_kpackage_structures(&[component]);

        assert!(patched.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn repair_does_not_overwrite_existing_value() {
        let dir = tempfile::tempdir().unwrap();
        let widget_dir = dir.path().join("has.field");
        std::fs::create_dir_all(&widget_dir).unwrap();
        std::fs::write(
            widget_dir.join("metadata.json"),
            r#"{"KPackageStructure": "Plasma/Applet"}"#,
        )
        .unwrap();

        let component = make_component("HasField", &widget_dir, ComponentType::PlasmaWidget);
        let (patched, errors) = repair_kpackage_structures(&[component]);

        assert!(patched.is_empty());
        assert!(errors.is_empty());
    }
}
