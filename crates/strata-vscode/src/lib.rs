use anyhow::{Context, Result, bail};
use jsonc_parser::cst::{CstInputValue, CstObject, CstRootNode};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env, fs,
    io::Write,
    path::{Component, Path, PathBuf},
};
use strata_core::{Settings, Source, StateSnapshot, canonical_json, parse_source};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendInfo {
    pub product: String,
    pub channel: String,
    pub platform: String,
    pub user_data_dir: PathBuf,
    pub backend_version: u32,
    pub writable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub name: String,
    pub opaque_id: Option<String>,
    pub settings_path: PathBuf,
    pub uses_default_settings: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("VS Code user-data directory was not found; pass --user-data-dir")]
    NotFound,
    #[error("profile registry is missing or has an unknown format at {0}; refusing to guess")]
    UnknownRegistry(PathBuf),
    #[error("profile '{0}' reuses Default settings; explicit storage adoption is required")]
    SharedDefault(String),
    #[error("profile path escapes VS Code User directory: {0}")]
    UnsafePath(PathBuf),
}

pub trait VSCodeBackend {
    fn probe(&self) -> Result<BackendInfo>;
    fn list_profiles(&self) -> Result<Vec<Profile>>;
    fn read_settings(&self, profile: &Profile) -> Result<Settings>;
    fn write_settings(&self, profile: &Profile, settings: &Settings) -> Result<()>;
    fn snapshot(&self) -> Result<BTreeMap<String, Settings>> {
        let mut out = BTreeMap::new();
        for profile in self.list_profiles()? {
            out.insert(profile.name.clone(), self.read_settings(&profile)?);
        }
        Ok(out)
    }
}

#[derive(Debug, Clone)]
pub struct FileBackend {
    user_data_dir: PathBuf,
    product: String,
    channel: String,
}

impl FileBackend {
    pub fn discover(override_dir: Option<PathBuf>) -> Result<Self> {
        if let Some(path) = override_dir {
            return Self::at(path, "Visual Studio Code", "custom");
        }
        for (path, product, channel) in candidate_user_data_dirs() {
            if path.is_dir() {
                return Self::at(path, product, channel);
            }
        }
        Err(BackendError::NotFound.into())
    }

    pub fn at(
        path: PathBuf,
        product: impl Into<String>,
        channel: impl Into<String>,
    ) -> Result<Self> {
        let path = path
            .canonicalize()
            .with_context(|| format!("invalid user-data directory {}", path.display()))?;
        Ok(Self {
            user_data_dir: path,
            product: product.into(),
            channel: channel.into(),
        })
    }

    fn user_dir(&self) -> PathBuf {
        self.user_data_dir.join("User")
    }
}

#[derive(Debug, Deserialize)]
struct Registry {
    profiles: Vec<RegistryProfile>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegistryProfile {
    #[serde(alias = "location")]
    id: String,
    name: String,
    #[serde(default)]
    use_default_settings: bool,
    #[serde(default)]
    use_default_flags: DefaultFlags,
}

#[derive(Debug, Default, Deserialize)]
struct DefaultFlags {
    #[serde(default)]
    settings: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProfileArchiveManifest {
    version: u32,
    registry_kind: String,
    registry: serde_json::Value,
}

impl VSCodeBackend for FileBackend {
    fn probe(&self) -> Result<BackendInfo> {
        Ok(BackendInfo {
            product: self.product.clone(),
            channel: self.channel.clone(),
            platform: env::consts::OS.into(),
            user_data_dir: user_facing_path(&self.user_data_dir),
            backend_version: 1,
            writable: true,
        })
    }

    fn list_profiles(&self) -> Result<Vec<Profile>> {
        let user = self.user_dir();
        let fixture_registry = user.join("profiles").join("profiles.json");
        let storage_registry = user.join("globalStorage").join("storage.json");
        let mut profiles = vec![Profile {
            name: "Default".into(),
            opaque_id: None,
            settings_path: user.join("settings.json"),
            uses_default_settings: false,
        }];
        if !user.join("profiles").is_dir() {
            return Ok(profiles);
        }
        let registry = if storage_registry.is_file() {
            let text = fs::read_to_string(&storage_registry)?;
            let value: serde_json::Value = serde_json::from_str(&text)
                .map_err(|_| BackendError::UnknownRegistry(storage_registry.clone()))?;
            Registry {
                profiles: serde_json::from_value(
                    value
                        .get("userDataProfiles")
                        .cloned()
                        .ok_or_else(|| BackendError::UnknownRegistry(storage_registry.clone()))?,
                )
                .map_err(|_| BackendError::UnknownRegistry(storage_registry.clone()))?,
            }
        } else {
            let text = fs::read_to_string(&fixture_registry)
                .map_err(|_| BackendError::UnknownRegistry(fixture_registry.clone()))?;
            serde_json::from_str(&text)
                .map_err(|_| BackendError::UnknownRegistry(fixture_registry.clone()))?
        };
        for item in registry.profiles {
            if item.id.replace('\\', "/").starts_with("builtin/") {
                continue;
            }
            validate_profile_id(&item.id)?;
            let uses_default_settings =
                item.use_default_settings || item.use_default_flags.settings;
            let settings_path = if uses_default_settings {
                user.join("settings.json")
            } else {
                user.join("profiles").join(&item.id).join("settings.json")
            };
            profiles.push(Profile {
                name: item.name,
                opaque_id: Some(item.id),
                settings_path,
                uses_default_settings,
            });
        }
        Ok(profiles)
    }

    fn read_settings(&self, profile: &Profile) -> Result<Settings> {
        if profile.uses_default_settings {
            return Err(BackendError::SharedDefault(profile.name.clone()).into());
        }
        if !profile.settings_path.exists() {
            return Ok(Settings::new());
        }
        let text = fs::read_to_string(&profile.settings_path)
            .with_context(|| format!("read {}", profile.settings_path.display()))?;
        jsonc_parser::parse_to_serde_value(&text, &Default::default())
            .map_err(|e| anyhow::anyhow!("invalid settings JSONC for '{}': {e}", profile.name))
    }

    fn write_settings(&self, profile: &Profile, settings: &Settings) -> Result<()> {
        if profile.uses_default_settings {
            return Err(BackendError::SharedDefault(profile.name.clone()).into());
        }
        ensure_under(&profile.settings_path, &self.user_dir())?;
        atomic_write(&profile.settings_path, canonical_json(settings).as_bytes())
    }
}

impl FileBackend {
    /// Exports Default settings, the complete profile directory tree, and the
    /// profile registry needed to restore that tree. The output folder is new
    /// for every export, so an existing archive is never overwritten.
    pub fn export_profiles(&self, destination: &Path) -> Result<PathBuf> {
        if !destination.is_dir() {
            bail!(
                "export destination is not a directory: {}",
                destination.display()
            );
        }
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time is after Unix epoch")
            .as_nanos();
        let archive = destination.join(format!("strata-profiles-{timestamp}"));
        fs::create_dir(&archive)?;
        let archive_user = archive.join("User");
        fs::create_dir_all(&archive_user)?;
        copy_if_file(
            &self.user_dir().join("settings.json"),
            &archive_user.join("settings.json"),
        )?;
        copy_tree(
            &self.user_dir().join("profiles"),
            &archive_user.join("profiles"),
        )?;
        let (registry_kind, registry) = self.profile_registry()?;
        let manifest = ProfileArchiveManifest {
            version: 1,
            registry_kind,
            registry,
        };
        atomic_write(
            &archive.join("strata-profiles.json"),
            &serde_json::to_vec_pretty(&manifest)?,
        )?;
        Ok(archive)
    }

    /// Restores an archive produced by `export_profiles`. Imported files retain
    /// their `User/profiles/<id>/...` layout; only the profile registry field is
    /// merged into global storage, leaving unrelated VS Code global state intact.
    pub fn import_profiles(&self, archive: &Path) -> Result<usize> {
        let manifest: ProfileArchiveManifest =
            serde_json::from_slice(&fs::read(archive.join("strata-profiles.json"))?)
                .context("invalid Strata profile archive manifest")?;
        if manifest.version != 1 {
            bail!(
                "unsupported Strata profile archive version {}",
                manifest.version
            );
        }
        validate_archive_registry(&manifest.registry_kind, &manifest.registry)?;
        let archive_user = archive.join("User");
        if !archive_user.is_dir() {
            bail!("profile archive has no User directory");
        }
        let user = self.user_dir();
        copy_if_file(
            &archive_user.join("settings.json"),
            &user.join("settings.json"),
        )?;
        copy_tree(&archive_user.join("profiles"), &user.join("profiles"))?;
        self.write_profile_registry(&manifest.registry_kind, manifest.registry)?;
        Ok(self.list_profiles()?.len())
    }

    fn profile_registry(&self) -> Result<(String, serde_json::Value)> {
        let user = self.user_dir();
        let storage = user.join("globalStorage").join("storage.json");
        if storage.is_file() {
            let document: serde_json::Value = serde_json::from_slice(&fs::read(&storage)?)
                .map_err(|_| BackendError::UnknownRegistry(storage.clone()))?;
            let profiles = document
                .get("userDataProfiles")
                .cloned()
                .ok_or_else(|| BackendError::UnknownRegistry(storage.clone()))?;
            validate_archive_registry("storage", &profiles)?;
            return Ok(("storage".into(), profiles));
        }
        let fixture = user.join("profiles").join("profiles.json");
        let document: serde_json::Value = serde_json::from_slice(&fs::read(&fixture)?)
            .map_err(|_| BackendError::UnknownRegistry(fixture.clone()))?;
        validate_archive_registry("profiles", &document)?;
        Ok(("profiles".into(), document))
    }

    fn write_profile_registry(&self, kind: &str, registry: serde_json::Value) -> Result<()> {
        let user = self.user_dir();
        match kind {
            "storage" => {
                let path = user.join("globalStorage").join("storage.json");
                let mut document = if path.is_file() {
                    serde_json::from_slice(&fs::read(&path)?)
                        .map_err(|_| BackendError::UnknownRegistry(path.clone()))?
                } else {
                    serde_json::json!({})
                };
                let object = document
                    .as_object_mut()
                    .context("VS Code storage registry must be an object")?;
                object.insert("userDataProfiles".into(), registry);
                atomic_write(&path, &serde_json::to_vec_pretty(&document)?)
            }
            "profiles" => atomic_write(
                &user.join("profiles").join("profiles.json"),
                &serde_json::to_vec_pretty(&registry)?,
            ),
            _ => bail!("unknown profile archive registry kind '{kind}'"),
        }
    }
}

fn validate_profile_id(id: &str) -> Result<()> {
    if id.is_empty()
        || Path::new(id).is_absolute()
        || Path::new(id).components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(BackendError::UnsafePath(PathBuf::from(id)).into());
    }
    Ok(())
}

fn validate_archive_registry(kind: &str, registry: &serde_json::Value) -> Result<()> {
    let profiles = match kind {
        "storage" => registry.clone(),
        "profiles" => registry
            .get("profiles")
            .cloned()
            .context("profile archive registry has no profiles field")?,
        _ => bail!("unknown profile archive registry kind '{kind}'"),
    };
    let registry: Registry = serde_json::from_value(serde_json::json!({ "profiles": profiles }))
        .context("profile archive registry has an unknown format")?;
    for profile in registry.profiles {
        validate_profile_id(&profile.id)?;
    }
    Ok(())
}

fn copy_if_file(source: &Path, destination: &Path) -> Result<()> {
    if !source.is_file() {
        return Ok(());
    }
    atomic_write(destination, &fs::read(source)?)
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    if !source.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if entry.file_type()?.is_file() {
            atomic_write(&target, &fs::read(entry.path())?)?;
        }
    }
    Ok(())
}

pub fn candidate_user_data_dirs() -> Vec<(PathBuf, &'static str, &'static str)> {
    let mut out = Vec::new();
    if cfg!(target_os = "windows") {
        if let Some(root) = env::var_os("APPDATA") {
            let root = PathBuf::from(root);
            out.push((root.join("Code"), "Visual Studio Code", "stable"));
            out.push((
                root.join("Code - Insiders"),
                "Visual Studio Code Insiders",
                "insiders",
            ));
        }
    } else if cfg!(target_os = "macos") {
        if let Some(home) = env::var_os("HOME") {
            let root = PathBuf::from(home).join("Library/Application Support");
            out.push((root.join("Code"), "Visual Studio Code", "stable"));
            out.push((
                root.join("Code - Insiders"),
                "Visual Studio Code Insiders",
                "insiders",
            ));
        }
    } else if let Some(root) = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    {
        out.push((root.join("Code"), "Visual Studio Code", "stable"));
        out.push((
            root.join("Code - Insiders"),
            "Visual Studio Code Insiders",
            "insiders",
        ));
    }
    out
}

/// `std::fs::canonicalize` returns `\\?\`-prefixed paths on Windows. Keep that
/// form internally for filesystem operations, but never expose it in user-facing
/// JSON such as Doctor output.
fn user_facing_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let value = path.to_string_lossy();
        if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        if let Some(normal) = value.strip_prefix(r"\\?\") {
            return PathBuf::from(normal);
        }
    }
    path.to_path_buf()
}

#[derive(Debug, Clone)]
pub struct Home {
    pub root: PathBuf,
}

impl Home {
    pub fn discover(override_dir: Option<PathBuf>) -> Result<Self> {
        let root = override_dir
            .or_else(|| env::var_os("STRATA_HOME").map(PathBuf::from))
            .or_else(|| {
                directories::ProjectDirs::from("dev", "Strata", "strata")
                    .map(|d| d.config_dir().to_path_buf())
            })
            .context("cannot determine Strata home")?;
        Ok(Self { root })
    }
    pub fn source_path(&self) -> PathBuf {
        self.root.join("profiles.jsonc")
    }
    pub fn state_path(&self) -> PathBuf {
        self.root.join("state.json")
    }
    pub fn lock_path(&self) -> PathBuf {
        self.root.join("state.lock")
    }
    pub fn read_source(&self) -> Result<Source> {
        parse_source(
            &fs::read_to_string(self.source_path())
                .context("Strata source is missing; run build")?,
        )
        .map_err(Into::into)
    }
    pub fn write_source(&self, source: &Source) -> Result<()> {
        let text = if self.source_path().exists() {
            patch_source(&fs::read_to_string(self.source_path())?, source)?
        } else {
            serialized_source(source)?
        };
        atomic_write(&self.source_path(), text.as_bytes())
    }
    pub fn replace_source(&self, source: &Source) -> Result<()> {
        let text = serialized_source(source)?;
        atomic_write(&self.source_path(), text.as_bytes())
    }
    pub fn read_state(&self) -> Result<StateSnapshot> {
        if !self.state_path().exists() {
            return Ok(StateSnapshot::default());
        }
        Ok(serde_json::from_str(&fs::read_to_string(
            self.state_path(),
        )?)?)
    }
    pub fn write_state(&self, state: &StateSnapshot) -> Result<()> {
        let mut text = serde_json::to_string_pretty(state)?;
        text.push('\n');
        atomic_write(&self.state_path(), text.as_bytes())
    }
}

fn serialized_source(source: &Source) -> Result<String> {
    let mut text = serde_json::to_string_pretty(source)?;
    text.push('\n');
    Ok(text)
}

fn patch_source(text: &str, source: &Source) -> Result<String> {
    let previous = parse_source(text)?;
    let root = CstRootNode::parse(text, &Default::default())?;
    let object = root.object_value_or_set();
    patch_settings(
        &object.object_value_or_set("base"),
        &previous.base,
        &source.base,
    );
    let profiles = object.object_value_or_set("profiles");
    let names: std::collections::BTreeSet<_> = previous
        .profiles
        .keys()
        .chain(source.profiles.keys())
        .cloned()
        .collect();
    for name in names {
        match (previous.profiles.get(&name), source.profiles.get(&name)) {
            (Some(_), None) => {
                if let Some(prop) = profiles.get(&name) {
                    prop.remove();
                }
            }
            (None, Some(delta)) => {
                profiles.append(&name, delta_input(delta));
            }
            (Some(before), Some(after)) => {
                let profile = profiles.object_value_or_set(&name);
                patch_settings(&profile, &before.settings, &after.settings);
                if before.uninherit != after.uninherit {
                    if after.uninherit.is_empty() {
                        if let Some(prop) = profile.get("uninherit") {
                            prop.remove();
                        }
                    } else {
                        let value = CstInputValue::Array(
                            after
                                .uninherit
                                .iter()
                                .cloned()
                                .map(CstInputValue::String)
                                .collect(),
                        );
                        match profile.get("uninherit") {
                            Some(prop) => prop.set_value(value),
                            None => {
                                profile.append("uninherit", value);
                            }
                        }
                    }
                }
            }
            (None, None) => unreachable!(),
        }
    }
    Ok(root.to_string())
}

fn patch_settings(object: &CstObject, before: &Settings, after: &Settings) {
    let keys: std::collections::BTreeSet<_> = before.keys().chain(after.keys()).cloned().collect();
    for key in keys {
        if before.get(&key) == after.get(&key) {
            continue;
        }
        match after.get(&key) {
            Some(value) => match object.get(&key) {
                Some(prop) => prop.set_value(value_input(value)),
                None => {
                    object.append(&key, value_input(value));
                }
            },
            None => {
                if let Some(prop) = object.get(&key) {
                    prop.remove();
                }
            }
        }
    }
}

fn delta_input(delta: &strata_core::ProfileDelta) -> CstInputValue {
    let mut properties = Vec::new();
    if !delta.uninherit.is_empty() {
        properties.push((
            "uninherit".into(),
            CstInputValue::Array(
                delta
                    .uninherit
                    .iter()
                    .cloned()
                    .map(CstInputValue::String)
                    .collect(),
            ),
        ));
    }
    properties.extend(
        delta
            .settings
            .iter()
            .map(|(key, value)| (key.clone(), value_input(value))),
    );
    CstInputValue::Object(properties)
}

fn value_input(value: &serde_json::Value) -> CstInputValue {
    match value {
        serde_json::Value::Null => CstInputValue::Null,
        serde_json::Value::Bool(value) => CstInputValue::Bool(*value),
        serde_json::Value::Number(value) => CstInputValue::Number(value.to_string()),
        serde_json::Value::String(value) => CstInputValue::String(value.clone()),
        serde_json::Value::Array(values) => {
            CstInputValue::Array(values.iter().map(value_input).collect())
        }
        serde_json::Value::Object(values) => CstInputValue::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), value_input(value)))
                .collect(),
        ),
    }
}

pub fn ensure_under(path: &Path, root: &Path) -> Result<()> {
    let parent = path.parent().context("target has no parent")?;
    fs::create_dir_all(parent)?;
    let parent = parent.canonicalize()?;
    let root = root.canonicalize()?;
    if !parent.starts_with(&root) {
        return Err(BackendError::UnsafePath(path.to_path_buf()).into());
    }
    Ok(())
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("target has no parent")?;
    fs::create_dir_all(parent)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".strata-")
        .tempfile_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    if let Ok(dir) = fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    fn temp() -> PathBuf {
        let p = env::temp_dir().join(format!(
            "strata-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(p.join("User/profiles/p1")).unwrap();
        p
    }
    #[test]
    fn fixture_discovery_and_snapshot() {
        let root = temp();
        fs::write(root.join("User/settings.json"), "{\"x\":1}").unwrap();
        fs::write(
            root.join("User/profiles/profiles.json"),
            r#"{"profiles":[{"id":"p1","name":"cpp"},{"id":"builtin/agents","name":"Agents","useDefaultSettings":true}]}"#,
        )
        .unwrap();
        fs::write(
            root.join("User/profiles/p1/settings.json"),
            "{ // c\n \"y\": 2, }",
        )
        .unwrap();
        let backend = FileBackend::at(root.clone(), "test", "test").unwrap();
        let snapshot = backend.snapshot().unwrap();
        assert_eq!(snapshot["Default"]["x"], 1);
        assert_eq!(snapshot["cpp"]["y"], 2);
        assert!(!snapshot.contains_key("Agents"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exports_and_imports_profile_archive_with_profile_layout() {
        let source = temp();
        fs::write(source.join("User/settings.json"), "{\"x\":1}").unwrap();
        fs::write(
            source.join("User/profiles/profiles.json"),
            r#"{"profiles":[{"id":"p1","name":"cpp"}]}"#,
        )
        .unwrap();
        fs::write(
            source.join("User/profiles/p1/settings.json"),
            "{\"editor.fontSize\":16}",
        )
        .unwrap();
        let source_backend = FileBackend::at(source.clone(), "test", "test").unwrap();
        let export_parent = temp();
        let archive = source_backend.export_profiles(&export_parent).unwrap();
        assert!(archive.join("User/profiles/p1/settings.json").is_file());
        assert!(archive.join("strata-profiles.json").is_file());

        let target = temp();
        let target_backend = FileBackend::at(target.clone(), "test", "test").unwrap();
        assert_eq!(target_backend.import_profiles(&archive).unwrap(), 2);
        assert_eq!(
            target_backend.snapshot().unwrap(),
            source_backend.snapshot().unwrap()
        );

        fs::remove_dir_all(source).unwrap();
        fs::remove_dir_all(export_parent).unwrap();
        fs::remove_dir_all(target).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn hides_windows_verbatim_prefix_in_reported_paths() {
        assert_eq!(
            user_facing_path(Path::new(r"\\?\C:\Users\strata\AppData\Roaming\Code")),
            PathBuf::from(r"C:\Users\strata\AppData\Roaming\Code"),
        );
        assert_eq!(
            user_facing_path(Path::new(r"\\?\UNC\server\share\Code")),
            PathBuf::from(r"\\server\share\Code"),
        );
    }

    #[test]
    fn source_mutation_preserves_unrelated_comments() {
        let text = "{\n  // shared settings\n  \"version\": 1,\n  \"base\": {\n    // keep this comment\n    \"x\": 1,\n  },\n  \"profiles\": { \"Default\": {} },\n}\n";
        let mut source = parse_source(text).unwrap();
        source.base.insert("x".into(), serde_json::json!(2));
        source.base.insert("y".into(), serde_json::json!(3));
        let patched = patch_source(text, &source).unwrap();
        assert!(patched.contains("// shared settings"));
        assert!(patched.contains("// keep this comment"));
        assert_eq!(parse_source(&patched).unwrap(), source);
    }
}
