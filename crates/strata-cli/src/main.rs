use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use fs2::FileExt;
use notify::{RecursiveMode, Watcher};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::PathBuf,
};
use strata_core::{
    Origin, ProfileDelta, Settings, Source, StateSnapshot, apply_actual_mutation, compile,
    content_hash, migration_preview, reconcile, source_hash,
};
use strata_protocol::{InitializeResult, PROTOCOL_VERSION, Request, Response};
use strata_vscode::{FileBackend, Home, Profile, VSCodeBackend};

#[derive(Parser)]
#[command(
    name = "strata",
    version,
    about = "Base + Delta settings for VS Code Profiles"
)]
struct Args {
    #[arg(long, global = true)]
    user_data_dir: Option<PathBuf>,
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    #[arg(long = "known-setting-prefix", global = true)]
    known_setting_prefixes: Vec<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Probe,
    Profiles,
    Snapshot,
    Doctor,
    Build {
        #[arg(long)]
        accept: bool,
    },
    Compile,
    Reconcile,
    Diff,
    Origin {
        profile: String,
        setting: String,
    },
    Serve {
        #[arg(long)]
        stdio: bool,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = Args::parse();
    if matches!(args.command, Command::Serve { stdio: true }) {
        return serve(args.user_data_dir, args.home);
    }
    let known_setting_prefixes = sanitize_prefixes(args.known_setting_prefixes);
    let backend = FileBackend::discover(args.user_data_dir)?;
    let home = Home::discover(args.home)?;
    match args.command {
        Command::Probe => print_json(&backend.probe()?)?,
        Command::Profiles => print_json(&backend.list_profiles()?)?,
        Command::Snapshot => print_json(&backend.snapshot()?)?,
        Command::Doctor => print_json(&doctor(&backend, &home))?,
        Command::Build { accept } => {
            let preview = migration_preview(&backend.snapshot()?, |setting| {
                has_known_prefix(&known_setting_prefixes, setting)
            });
            print_json(&preview)?;
            if accept {
                with_lock(&home, || {
                    home.replace_source(&preview.source)?;
                    home.write_state(&StateSnapshot {
                        known_setting_prefixes: known_setting_prefixes.clone(),
                        ..StateSnapshot::default()
                    })
                })?;
                eprintln!("built {}", home.source_path().display());
            } else {
                eprintln!("preview only; rerun with --accept to build");
            }
        }
        Command::Compile => print_json(&compile_all(&backend, &home)?)?,
        Command::Reconcile => print_json(&reconcile_all(&backend, &home)?)?,
        Command::Diff => print_json(&diff(&backend, &home)?)?,
        Command::Origin { profile, setting } => {
            print_json(&origin(&home.read_source()?, &profile, &setting))?
        }
        Command::Serve { .. } => bail!("serve requires --stdio"),
    }
    Ok(())
}

fn print_json(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn sanitize_prefixes(values: impl IntoIterator<Item = String>) -> BTreeSet<String> {
    values
        .into_iter()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty() && !value.contains('.'))
        .collect()
}

fn known_setting_prefixes(params: &Value) -> Result<BTreeSet<String>> {
    let values = params
        .get("knownSettingPrefixes")
        .and_then(Value::as_array)
        .context("knownSettingPrefixes is required")?;
    let prefixes = values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .context("knownSettingPrefixes must contain only strings")
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(sanitize_prefixes(prefixes))
}

fn has_known_prefix(prefixes: &BTreeSet<String>, setting: &str) -> bool {
    prefixes.contains(setting.split('.').next().unwrap_or_default())
}

fn archive_path(params: &Value, field: &str) -> Result<PathBuf> {
    params
        .get(field)
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .with_context(|| format!("{field} is required"))
}

fn with_lock<T>(home: &Home, action: impl FnOnce() -> Result<T>) -> Result<T> {
    fs::create_dir_all(&home.root)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.lock_path())?;
    lock.lock_exclusive()?;
    let result = action();
    FileExt::unlock(&lock)?;
    result
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Doctor {
    ok: bool,
    backend: Option<Value>,
    source: Option<String>,
    issues: Vec<String>,
}

fn doctor(backend: &FileBackend, home: &Home) -> Doctor {
    let mut issues = Vec::new();
    let backend_info = match backend.probe() {
        Ok(v) => serde_json::to_value(v).ok(),
        Err(e) => {
            issues.push(e.to_string());
            None
        }
    };
    match backend.list_profiles() {
        Ok(profiles) => {
            for profile in profiles {
                if profile.uses_default_settings {
                    issues.push(format!(
                        "profile '{}' reuses Default settings and must be adopted explicitly",
                        profile.name
                    ));
                }
            }
        }
        Err(e) => issues.push(e.to_string()),
    }
    let source = if home.source_path().exists() {
        match home.read_source() {
            Ok(_) => Some(home.source_path().display().to_string()),
            Err(e) => {
                issues.push(e.to_string());
                None
            }
        }
    } else {
        issues.push("Strata is not initialized".into());
        None
    };
    Doctor {
        ok: issues.is_empty(),
        backend: backend_info,
        source,
        issues,
    }
}

fn mapped_profiles(backend: &FileBackend, source: &Source) -> Result<BTreeMap<String, Profile>> {
    let available: BTreeMap<_, _> = backend
        .list_profiles()?
        .into_iter()
        .map(|p| (p.name.clone(), p))
        .collect();
    for name in source.profiles.keys() {
        if !available.contains_key(name) {
            bail!("source profile '{name}' does not exist in VS Code");
        }
    }
    for name in available.keys() {
        if !source.profiles.contains_key(name) {
            bail!("VS Code profile '{name}' is not adopted in the source; run build");
        }
    }
    Ok(available)
}

fn state_for(
    source: &Source,
    revision: u64,
    outputs: &BTreeMap<String, Settings>,
    known_setting_prefixes: BTreeSet<String>,
) -> StateSnapshot {
    StateSnapshot {
        revision,
        source_hash: source_hash(source),
        materialized: outputs.clone(),
        materialized_hashes: outputs
            .iter()
            .map(|(n, s)| (n.clone(), content_hash(s)))
            .collect(),
        known_setting_prefixes,
    }
}

fn compile_all(backend: &FileBackend, home: &Home) -> Result<StateSnapshot> {
    with_lock(home, || {
        let source = home.read_source()?;
        let previous = home.read_state()?;
        let profiles = mapped_profiles(backend, &source)?;
        let actual = backend.snapshot()?;
        let outputs = compile(&source);
        for (name, settings) in &outputs {
            if actual.get(name).map(content_hash).as_ref() != Some(&content_hash(settings)) {
                backend.write_settings(&profiles[name], settings)?;
            }
        }
        let state = state_for(
            &source,
            previous.revision + 1,
            &outputs,
            previous.known_setting_prefixes,
        );
        home.write_state(&state)?;
        Ok(state)
    })
}

fn reconcile_all(backend: &FileBackend, home: &Home) -> Result<Value> {
    with_lock(home, || {
        let source = home.read_source()?;
        let previous = home.read_state()?;
        let actual = backend.snapshot()?;
        if previous.materialized.is_empty() {
            bail!("no prior materialized state; run compile first");
        }
        let actual_hashes: BTreeMap<_, _> = actual
            .iter()
            .map(|(name, settings)| (name.clone(), content_hash(settings)))
            .collect();
        if previous.source_hash == source_hash(&source)
            && previous.materialized_hashes == actual_hashes
        {
            return Ok(json!({"status":"unchanged", "revision":previous.revision}));
        }
        let known_setting_prefixes = previous.known_setting_prefixes.clone();
        let result = reconcile(&source, &previous, &actual, |setting| {
            has_known_prefix(&known_setting_prefixes, setting)
        });
        if !result.conflicts.is_empty() {
            return Ok(json!({"status":"conflict", "conflicts":result.conflicts}));
        }
        let profiles = mapped_profiles(backend, &result.source)?;
        if result.changed_source {
            home.write_source(&result.source)?;
        }
        for (name, settings) in &result.outputs {
            if actual.get(name).map(content_hash).as_ref() != Some(&content_hash(settings)) {
                backend.write_settings(&profiles[name], settings)?;
            }
        }
        let state = state_for(
            &result.source,
            previous.revision + 1,
            &result.outputs,
            known_setting_prefixes,
        );
        home.write_state(&state)?;
        Ok(json!({"status":"ok", "revision":state.revision, "sourceChanged":result.changed_source}))
    })
}

fn diff(backend: &FileBackend, home: &Home) -> Result<Value> {
    let expected = compile(&home.read_source()?);
    let actual = backend.snapshot()?;
    let mut changes = Vec::new();
    let profiles: BTreeSet<_> = expected.keys().chain(actual.keys()).cloned().collect();
    for profile in profiles {
        let left = expected.get(&profile).cloned().unwrap_or_default();
        let right = actual.get(&profile).cloned().unwrap_or_default();
        let keys: BTreeSet<_> = left.keys().chain(right.keys()).cloned().collect();
        for setting in keys {
            if left.get(&setting) != right.get(&setting) {
                changes.push(json!({"profile":profile,"setting":setting,"expected":left.get(&setting),"actual":right.get(&setting)}));
            }
        }
    }
    Ok(json!({"clean":changes.is_empty(),"changes":changes}))
}

fn origin(source: &Source, profile: &str, setting: &str) -> Value {
    let Some(delta) = source.profiles.get(profile) else {
        return json!({"origin":"unknownProfile"});
    };
    let origin = if delta.uninherit.contains(setting) {
        Origin::Uninherit
    } else if delta.settings.contains_key(setting) {
        Origin::ProfileOverride
    } else if source.base.contains_key(setting) {
        Origin::Base
    } else {
        Origin::VscodeDefault
    };
    json!({"profile":profile,"setting":setting,"origin":origin})
}

fn mutate(home: &Home, method: &str, params: &Value) -> Result<Value> {
    with_lock(home, || {
        let mut source = home.read_source()?;
        let setting = params
            .get("setting")
            .and_then(Value::as_str)
            .context("setting is required")?;
        let value = params.get("value").cloned();
        if method == "settings/setBase" {
            source
                .base
                .insert(setting.into(), value.context("value is required")?);
            home.write_source(&source)?;
            return Ok(json!({"ok":true}));
        }
        let profile = params
            .get("profile")
            .and_then(Value::as_str)
            .context("profile is required")?;
        let delta = source
            .profiles
            .entry(profile.into())
            .or_insert_with(ProfileDelta::default);
        match method {
            "settings/inherit" => {
                delta.settings.remove(setting);
                delta.uninherit.remove(setting);
            }
            "settings/uninherit" => {
                delta.settings.remove(setting);
                delta.uninherit.insert(setting.into());
            }
            _ => bail!("unknown mutation method"),
        }
        home.write_source(&source)?;
        Ok(json!({"ok":true}))
    })
}

fn build_source(
    backend: &FileBackend,
    home: &Home,
    known_setting_prefixes: BTreeSet<String>,
) -> Result<Value> {
    with_lock(home, || {
        let preview = migration_preview(&backend.snapshot()?, |setting| {
            has_known_prefix(&known_setting_prefixes, setting)
        });
        home.replace_source(&preview.source)?;
        home.write_state(&StateSnapshot {
            known_setting_prefixes,
            ..StateSnapshot::default()
        })?;
        Ok(json!({
            "status": "built",
            "sourcePath": home.source_path(),
            "newlyInherited": preview.newly_inherited.len()
        }))
    })
}

fn resolve_conflicts(backend: &FileBackend, home: &Home, params: &Value) -> Result<Value> {
    with_lock(home, || {
        let mut source = home.read_source()?;
        let previous = home.read_state()?;
        let actual = backend.snapshot()?;
        let known_setting_prefixes = previous.known_setting_prefixes.clone();
        let conflicts = reconcile(&source, &previous, &actual, |setting| {
            has_known_prefix(&known_setting_prefixes, setting)
        })
        .conflicts;
        let resolutions = params
            .get("resolutions")
            .and_then(Value::as_array)
            .context("resolutions are required")?;
        if resolutions.len() != conflicts.len() {
            bail!("every current conflict must have exactly one resolution");
        }
        let mut changed_source = false;
        for resolution in resolutions {
            let profile = resolution
                .get("profile")
                .and_then(Value::as_str)
                .context("profile is required")?;
            let setting = resolution
                .get("setting")
                .and_then(Value::as_str)
                .context("setting is required")?;
            let action = resolution
                .get("action")
                .and_then(Value::as_str)
                .context("action is required")?;
            let conflict = conflicts
                .iter()
                .find(|item| item.profile == profile && item.setting == setting)
                .context("conflict is no longer current")?;
            match action {
                "useSource" => {}
                "useVscode" => {
                    apply_actual_mutation(
                        &mut source,
                        profile,
                        setting,
                        conflict.actual.clone(),
                        has_known_prefix(&known_setting_prefixes, setting),
                    );
                    changed_source = true;
                }
                "keepProfileOverride" => {
                    let delta = source.profiles.entry(profile.into()).or_default();
                    match conflict.actual.clone() {
                        Some(value) => {
                            delta.settings.insert(setting.into(), value);
                            delta.uninherit.remove(setting);
                        }
                        None => {
                            delta.settings.remove(setting);
                            delta.uninherit.insert(setting.into());
                        }
                    }
                    changed_source = true;
                }
                _ => bail!("unknown conflict action '{action}'"),
            }
        }
        if changed_source {
            home.write_source(&source)?;
        }
        let profiles = mapped_profiles(backend, &source)?;
        let outputs = compile(&source);
        for (name, settings) in &outputs {
            if actual.get(name).map(content_hash).as_ref() != Some(&content_hash(settings)) {
                backend.write_settings(&profiles[name], settings)?;
            }
        }
        let state = state_for(
            &source,
            previous.revision + 1,
            &outputs,
            known_setting_prefixes,
        );
        home.write_state(&state)?;
        Ok(json!({"status":"resolved", "revision":state.revision}))
    })
}

fn serve(user_data_dir: Option<PathBuf>, home_dir: Option<PathBuf>) -> Result<()> {
    let backend = FileBackend::discover(user_data_dir)?;
    let home = Home::discover(home_dir)?;
    let _watcher = start_watcher(backend.clone(), home.clone())?;
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    while let Some(body) = read_frame(&mut reader)? {
        let request: Request = serde_json::from_slice(&body)?;
        if request.jsonrpc != "2.0" {
            continue;
        }
        let Some(id) = request.id.clone() else {
            if request.method == "exit" {
                break;
            } else {
                continue;
            }
        };
        let response = match handle_rpc(&backend, &home, &request) {
            Ok(value) => Response::ok(id, value),
            Err(e) => Response::err(id, -32000, format!("{e:#}")),
        };
        write_frame(&mut writer, &serde_json::to_vec(&response)?)?;
        if request.method == "shutdown" {
            break;
        }
    }
    Ok(())
}

fn start_watcher(backend: FileBackend, home: Home) -> Result<notify::RecommendedWatcher> {
    let (sender, receiver) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = sender.send(event);
    })?;
    if let Some(parent) = home.source_path().parent() {
        fs::create_dir_all(parent)?;
        watcher.watch(parent, RecursiveMode::NonRecursive)?;
    }
    let user_dir = backend.probe()?.user_data_dir.join("User");
    watcher.watch(&user_dir, RecursiveMode::Recursive)?;
    std::thread::spawn(move || {
        while receiver.recv().is_ok() {
            while receiver
                .recv_timeout(std::time::Duration::from_millis(200))
                .is_ok()
            {}
            if !home.source_path().is_file() {
                continue;
            }
            if let Err(error) = reconcile_all(&backend, &home) {
                tracing::warn!(%error, "automatic reconcile skipped");
            }
        }
    });
    Ok(watcher)
}

fn handle_rpc(backend: &FileBackend, home: &Home, request: &Request) -> Result<Value> {
    Ok(match request.method.as_str() {
        "initialize" => {
            let version = request
                .params
                .get("protocolVersion")
                .and_then(Value::as_u64)
                .context("protocolVersion is required")?;
            if version != PROTOCOL_VERSION as u64 {
                bail!("incompatible protocol version {version}");
            }
            serde_json::to_value(InitializeResult {
                protocol_version: PROTOCOL_VERSION,
                helper_version: env!("CARGO_PKG_VERSION"),
                capabilities: vec![
                    "build",
                    "compile",
                    "reconcile",
                    "diff",
                    "origin",
                    "settingsCrud",
                    "profileArchive",
                ],
                backend: serde_json::to_value(backend.probe()?)?,
            })?
        }
        "profiles/list" => serde_json::to_value(backend.list_profiles()?)?,
        "profiles/export" => {
            let destination = archive_path(&request.params, "destination")?;
            with_lock(home, || {
                Ok(json!({"path":backend.export_profiles(&destination)?}))
            })?
        }
        "profiles/import" => {
            let archive = archive_path(&request.params, "archive")?;
            with_lock(home, || {
                Ok(json!({"profiles":backend.import_profiles(&archive)?}))
            })?
        }
        "status" => json!({
            "initialized": home.source_path().is_file(),
            "hasState": home.state_path().is_file(),
            "sourcePath": home.source_path()
        }),
        "build/preview" => {
            let prefixes = known_setting_prefixes(&request.params)?;
            serde_json::to_value(migration_preview(&backend.snapshot()?, |setting| {
                has_known_prefix(&prefixes, setting)
            }))?
        }
        "build/apply" => build_source(backend, home, known_setting_prefixes(&request.params)?)?,
        "compile" => serde_json::to_value(compile_all(backend, home)?)?,
        "reconcile" => reconcile_all(backend, home)?,
        "diff" => diff(backend, home)?,
        "doctor" => serde_json::to_value(doctor(backend, home))?,
        "settings/origin" => origin(
            &home.read_source()?,
            request
                .params
                .get("profile")
                .and_then(Value::as_str)
                .context("profile required")?,
            request
                .params
                .get("setting")
                .and_then(Value::as_str)
                .context("setting required")?,
        ),
        "conflicts/resolveAll" => resolve_conflicts(backend, home, &request.params)?,
        method if method.starts_with("settings/") => mutate(home, method, &request.params)?,
        "source/path" => json!({"path":home.source_path()}),
        "shutdown" => Value::Null,
        method => bail!("method not found: {method}"),
    })
}

fn read_frame(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(value) = line
            .strip_prefix("Content-Length:")
            .or_else(|| line.strip_prefix("content-length:"))
        {
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    let mut body = vec![0; length.context("missing Content-Length")?];
    reader.read_exact(&mut body)?;
    Ok(Some(body))
}

fn write_frame(writer: &mut impl Write, body: &[u8]) -> Result<()> {
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(body)?;
    writer.flush()?;
    Ok(())
}
