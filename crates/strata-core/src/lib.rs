use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

pub type Settings = BTreeMap<String, Value>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(default = "schema_version")]
    pub version: u32,
    #[serde(default)]
    pub base: Settings,
    #[serde(default)]
    pub profiles: BTreeMap<String, ProfileDelta>,
}

impl Default for Source {
    fn default() -> Self {
        Self {
            schema: None,
            version: 1,
            base: Settings::new(),
            profiles: BTreeMap::new(),
        }
    }
}

const fn schema_version() -> u32 {
    1
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProfileDelta {
    #[serde(
        default,
        rename = "uninherit",
        skip_serializing_if = "BTreeSet::is_empty"
    )]
    pub uninherit: BTreeSet<String>,
    #[serde(flatten)]
    pub settings: Settings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Origin {
    Base,
    ProfileOverride,
    Uninherit,
    VscodeDefault,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedProfile {
    pub settings: Settings,
    pub provenance: BTreeMap<String, Origin>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateSnapshot {
    pub revision: u64,
    #[serde(default)]
    pub source_hash: String,
    #[serde(default)]
    pub materialized: BTreeMap<String, Settings>,
    #[serde(default)]
    pub materialized_hashes: BTreeMap<String, String>,
    /// Cached from VS Code's generated settings schema at migration time.
    #[serde(default)]
    pub known_setting_prefixes: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Conflict {
    pub profile: String,
    pub setting: String,
    pub previous: Option<Value>,
    pub source: Option<Value>,
    pub actual: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileResult {
    pub source: Source,
    pub outputs: BTreeMap<String, Settings>,
    pub conflicts: Vec<Conflict>,
    pub changed_source: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationChange {
    pub profile: String,
    pub setting: String,
    pub inherited_value: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationPreview {
    pub source: Source,
    pub newly_inherited: Vec<MigrationChange>,
}

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("invalid JSONC: {0}")]
    Jsonc(String),
    #[error("unsupported source version {0}; supported version is 1")]
    Version(u32),
    #[error("profile name must not be empty")]
    EmptyProfile,
    #[error("profile '{profile}' contains both a value and uninherit for '{setting}'")]
    ValueAndUninherit { profile: String, setting: String },
    #[error("profile '{profile}' uses obsolete '$unset'; run Build to regenerate the source")]
    ObsoleteUnset { profile: String },
}

pub fn parse_source(text: &str) -> Result<Source, CoreError> {
    let source: Source = jsonc_parser::parse_to_serde_value(text, &Default::default())
        .map_err(|e| CoreError::Jsonc(e.to_string()))?;
    validate_source(&source)?;
    Ok(source)
}

pub fn validate_source(source: &Source) -> Result<(), CoreError> {
    if source.version != 1 {
        return Err(CoreError::Version(source.version));
    }
    for (profile, delta) in &source.profiles {
        if profile.trim().is_empty() {
            return Err(CoreError::EmptyProfile);
        }
        if delta.settings.contains_key("$unset") {
            return Err(CoreError::ObsoleteUnset {
                profile: profile.clone(),
            });
        }
        if let Some(setting) = delta
            .uninherit
            .iter()
            .find(|k| delta.settings.contains_key(*k))
        {
            return Err(CoreError::ValueAndUninherit {
                profile: profile.clone(),
                setting: setting.clone(),
            });
        }
    }
    Ok(())
}

pub fn resolve(base: &Settings, delta: &ProfileDelta) -> ResolvedProfile {
    let mut settings = base.clone();
    let mut provenance: BTreeMap<String, Origin> =
        base.keys().map(|k| (k.clone(), Origin::Base)).collect();
    for key in &delta.uninherit {
        settings.remove(key);
        provenance.insert(key.clone(), Origin::Uninherit);
    }
    for (key, value) in &delta.settings {
        let merged = match settings.get(key) {
            Some(parent) => overlay_value(parent, value),
            None => value.clone(),
        };
        settings.insert(key.clone(), merged);
        provenance.insert(key.clone(), Origin::ProfileOverride);
    }
    ResolvedProfile {
        settings,
        provenance,
    }
}

fn overlay_value(base: &Value, overlay: &Value) -> Value {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            let mut out: Map<String, Value> = base.clone();
            for (key, value) in overlay {
                let next = out
                    .get(key)
                    .map(|old| overlay_value(old, value))
                    .unwrap_or_else(|| value.clone());
                out.insert(key.clone(), next);
            }
            Value::Object(out)
        }
        _ => overlay.clone(),
    }
}

pub fn compile(source: &Source) -> BTreeMap<String, Settings> {
    source
        .profiles
        .iter()
        .map(|(name, delta)| (name.clone(), resolve(&source.base, delta).settings))
        .collect()
}

pub fn content_hash(settings: &Settings) -> String {
    blake3::hash(&serde_json::to_vec(settings).expect("settings serialize"))
        .to_hex()
        .to_string()
}

pub fn source_hash(source: &Source) -> String {
    blake3::hash(&serde_json::to_vec(source).expect("source serialize"))
        .to_hex()
        .to_string()
}

pub fn canonical_json(settings: &Settings) -> String {
    let mut text = serde_json::to_string_pretty(settings).expect("settings serialize");
    text.push('\n');
    text
}

pub fn reconcile(
    source: &Source,
    previous: &StateSnapshot,
    actual: &BTreeMap<String, Settings>,
    is_base_setting: impl Fn(&str) -> bool,
) -> ReconcileResult {
    let expected = compile(source);
    let mut next = source.clone();
    let mut conflicts = Vec::new();
    let mut changed_source = false;
    let profiles: BTreeSet<_> = expected
        .keys()
        .chain(actual.keys())
        .chain(previous.materialized.keys())
        .cloned()
        .collect();

    for profile in profiles {
        let old = previous
            .materialized
            .get(&profile)
            .cloned()
            .unwrap_or_default();
        let wanted = expected.get(&profile).cloned().unwrap_or_default();
        let observed = actual.get(&profile).cloned().unwrap_or_default();
        let keys: BTreeSet<_> = old
            .keys()
            .chain(wanted.keys())
            .chain(observed.keys())
            .cloned()
            .collect();
        for key in keys {
            let before = old.get(&key);
            let source_value = wanted.get(&key);
            let actual_value = observed.get(&key);
            let source_changed = source_value != before;
            let actual_changed = actual_value != before;
            if source_changed && actual_changed && source_value != actual_value {
                conflicts.push(Conflict {
                    profile: profile.clone(),
                    setting: key,
                    previous: before.cloned(),
                    source: source_value.cloned(),
                    actual: actual_value.cloned(),
                });
            } else if actual_changed && !source_changed {
                apply_actual_mutation(
                    &mut next,
                    &profile,
                    &key,
                    actual_value.cloned(),
                    is_base_setting(&key),
                );
                changed_source = true;
            }
        }
    }
    let outputs = compile(&next);
    ReconcileResult {
        source: next,
        outputs,
        conflicts,
        changed_source,
    }
}

pub fn apply_actual_mutation(
    source: &mut Source,
    profile: &str,
    key: &str,
    actual: Option<Value>,
    is_base_setting: bool,
) {
    if profile == "Default" && is_base_setting {
        match actual {
            Some(value) => {
                source.base.insert(key.into(), value);
            }
            None => {
                source.base.remove(key);
            }
        }
        let default = source.profiles.entry("Default".into()).or_default();
        default.settings.remove(key);
        default.uninherit.remove(key);
        return;
    }
    let inherited = source.base.get(key).cloned();
    let delta = source.profiles.entry(profile.into()).or_default();
    match actual {
        Some(value) if inherited.as_ref() == Some(&value) => {
            delta.settings.remove(key);
            delta.uninherit.remove(key);
        }
        Some(value) => {
            delta.settings.insert(key.into(), value);
            delta.uninherit.remove(key);
        }
        None if delta.settings.remove(key).is_some() => {
            delta.uninherit.remove(key);
        }
        None if inherited.is_some() => {
            delta.uninherit.insert(key.into());
        }
        None => {
            delta.settings.remove(key);
            delta.uninherit.remove(key);
        }
    }
}

pub fn migration_preview(
    actual: &BTreeMap<String, Settings>,
    is_base_setting: impl Fn(&str) -> bool,
) -> MigrationPreview {
    let base: Settings = actual
        .get("Default")
        .into_iter()
        .flat_map(|settings| settings.iter())
        .filter(|(key, _)| is_base_setting(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let mut profiles = BTreeMap::new();
    let mut newly_inherited = Vec::new();
    for (name, settings) in actual {
        let mut delta = ProfileDelta::default();
        if name == "Default" {
            for (key, value) in settings {
                if !base.contains_key(key) {
                    delta.settings.insert(key.clone(), value.clone());
                }
            }
        } else {
            for (key, base_value) in &base {
                match settings.get(key) {
                    None => {
                        delta.uninherit.insert(key.clone());
                        newly_inherited.push(MigrationChange {
                            profile: name.clone(),
                            setting: key.clone(),
                            inherited_value: base_value.clone(),
                        });
                    }
                    Some(value) if value != base_value => {
                        delta.settings.insert(key.clone(), value.clone());
                    }
                    Some(_) => {}
                }
            }
            for (key, value) in settings {
                if !base.contains_key(key) {
                    delta.settings.insert(key.clone(), value.clone());
                }
            }
        }
        profiles.insert(name.clone(), delta);
    }
    profiles.entry("Default".into()).or_default();
    MigrationPreview {
        source: Source {
            schema: None,
            version: 1,
            base,
            profiles,
        },
        newly_inherited,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn settings(items: &[(&str, Value)]) -> Settings {
        items
            .iter()
            .map(|(k, v)| ((*k).into(), v.clone()))
            .collect()
    }

    #[test]
    fn resolves_inheritance_object_merge_array_replace_and_uninherit() {
        let base = settings(&[
            ("editor.fontSize", json!(15)),
            (
                "[typescript]",
                json!({"editor.defaultFormatter":"a", "editor.tabSize":2}),
            ),
            ("list", json!([1, 2])),
            ("gone", json!(true)),
        ]);
        let delta = ProfileDelta {
            uninherit: BTreeSet::from(["gone".into()]),
            settings: settings(&[
                ("[typescript]", json!({"editor.tabSize":4})),
                ("list", json!([3])),
            ]),
        };
        let got = resolve(&base, &delta);
        assert_eq!(
            got.settings["[typescript]"],
            json!({"editor.defaultFormatter":"a", "editor.tabSize":4})
        );
        assert_eq!(got.settings["list"], json!([3]));
        assert!(!got.settings.contains_key("gone"));
        assert_eq!(got.provenance["gone"], Origin::Uninherit);
    }

    #[test]
    fn parses_comments_and_trailing_commas() {
        let source = parse_source(
            "{ // hi\n \"version\": 1, \"base\": {\"x\": 1,}, \"profiles\": {\"Default\": {},}, }",
        )
        .unwrap();
        assert_eq!(source.base["x"], json!(1));
    }

    #[test]
    fn rejects_obsolete_unset_field() {
        let error =
            parse_source("{\"version\":1,\"base\":{},\"profiles\":{\"Default\":{\"$unset\":[]}}}")
                .unwrap_err();
        assert!(matches!(error, CoreError::ObsoleteUnset { .. }));
    }

    #[test]
    fn reconcile_merges_independent_changes_and_detects_same_key_conflict() {
        let mut source = Source {
            base: settings(&[("x", json!(2)), ("y", json!(1))]),
            ..Source::default()
        };
        source
            .profiles
            .insert("Default".into(), ProfileDelta::default());
        let previous = StateSnapshot {
            revision: 1,
            source_hash: String::new(),
            materialized: BTreeMap::from([(
                "Default".into(),
                settings(&[("x", json!(1)), ("y", json!(1))]),
            )]),
            materialized_hashes: BTreeMap::new(),
            known_setting_prefixes: BTreeSet::new(),
        };
        let actual = BTreeMap::from([(
            "Default".into(),
            settings(&[("x", json!(1)), ("y", json!(3))]),
        )]);
        let got = reconcile(&source, &previous, &actual, |_| true);
        assert!(got.conflicts.is_empty());
        assert_eq!(got.source.base["x"], json!(2));
        assert_eq!(got.source.base["y"], json!(3));
        let actual_conflict = BTreeMap::from([(
            "Default".into(),
            settings(&[("x", json!(4)), ("y", json!(1))]),
        )]);
        assert_eq!(
            reconcile(&source, &previous, &actual_conflict, |_| true)
                .conflicts
                .len(),
            1
        );
    }

    #[test]
    fn migration_preserves_absence_with_uninherit() {
        let actual = BTreeMap::from([
            ("Default".into(), settings(&[("x", json!(1))])),
            ("cpp".into(), Settings::new()),
        ]);
        let preview = migration_preview(&actual, |_| true);
        assert!(preview.source.profiles["cpp"].uninherit.contains("x"));
        assert!(compile(&preview.source)["cpp"].is_empty());
        assert_eq!(preview.newly_inherited.len(), 1);
    }

    #[test]
    fn migration_keeps_non_base_default_settings_in_default_profile() {
        let actual = BTreeMap::from([
            (
                "Default".into(),
                settings(&[
                    ("editor.fontSize", json!(16)),
                    ("cmake.configureOnOpen", json!(true)),
                ]),
            ),
            ("cpp".into(), Settings::new()),
        ]);
        let preview = migration_preview(&actual, |key| key.starts_with("editor."));
        assert_eq!(
            preview.source.base,
            settings(&[("editor.fontSize", json!(16))])
        );
        assert_eq!(
            preview.source.profiles["Default"].settings,
            settings(&[("cmake.configureOnOpen", json!(true))])
        );
        assert!(
            preview.source.profiles["cpp"]
                .uninherit
                .contains("editor.fontSize")
        );
        assert!(
            !preview.source.profiles["cpp"]
                .uninherit
                .contains("cmake.configureOnOpen")
        );
        assert_eq!(compile(&preview.source)["Default"], actual["Default"]);
        assert!(compile(&preview.source)["cpp"].is_empty());
    }

    #[test]
    fn reconcile_keeps_non_base_default_changes_local() {
        let mut source = Source::default();
        source
            .profiles
            .insert("Default".into(), ProfileDelta::default());
        let previous = StateSnapshot {
            materialized: BTreeMap::from([("Default".into(), Settings::new())]),
            ..StateSnapshot::default()
        };
        let actual = BTreeMap::from([(
            "Default".into(),
            settings(&[("cmake.configureOnOpen", json!(true))]),
        )]);
        let result = reconcile(&source, &previous, &actual, |key| {
            key.starts_with("editor.")
        });
        assert!(!result.source.base.contains_key("cmake.configureOnOpen"));
        assert_eq!(
            result.source.profiles["Default"].settings["cmake.configureOnOpen"],
            json!(true)
        );
    }
}
