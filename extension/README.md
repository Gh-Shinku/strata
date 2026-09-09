# Strata

Declarative settings inheritance for VS Code Profiles.

## Usage

1. Run `Strata: Doctor` and resolve any reported Profile-storage issues.
2. Run `Strata: Build Preview` to inspect the source that Strata would generate from your existing Profiles. The preview is a read-only virtual document; closing it releases it immediately.
3. Run `Strata: Build` to generate `profiles.jsonc` and materialize the adopted Profiles.
4. Run `Strata: Open Configuration`, or click the Strata status-bar item, to open the source file.

`Build` replaces the current Strata source with one derived from the existing VS Code settings. It does not make a backup or request a second confirmation, so use `Build Preview` before rebuilding an existing source.

After the first Build, edit `profiles.jsonc` as the source of truth. Strata monitors it and the adopted VS Code Profile settings, then reconciles changes automatically. If the same setting changes on both sides, Strata reports a conflict instead of silently choosing a value. The source editor provides setting-name completion and hover descriptions from VS Code's current Settings schema.

`Strata: CRUD` groups visibility-oriented actions for one setting:

| Action | Result |
| --- | --- |
| Query | Shows whether a setting is visible from Base, a Profile override, `uninherit`, or VS Code defaults. |
| Base | Stores a setting and its JSON value in `base`; no Profile is selected. |
| Inherit | Removes the selected Profile's override or `uninherit` entry, restoring Base inheritance. |
| Uninherit | Removes the Profile override and prevents the Base setting from being materialized for that Profile. |

In a text `settings.json`, select a complete setting ID (for example, `"editor.fontSize"`) and choose **Strata: CRUD** from the editor context menu. For a Profile-specific file, Strata infers the Profile from its path; Query, Inherit, and Uninherit skip the Profile picker.

## Commands

| Command | Description |
| --- | --- |
| `Strata: Build Preview` | Preview the source that Build would create; it changes no files. |
| `Strata: Build` | Recreate `profiles.jsonc` from current Profile settings, then materialize it. |
| `Strata: Open Configuration` | Open the source file; Builds it first when it does not exist. |
| `Strata: CRUD` | Query, add to Base, inherit, or uninherit one setting. |
| `Strata: Export` | Create a timestamped archive containing VS Code Profile settings and registry data. |
| `Strata: Import` | Restore a Strata archive after an overwrite confirmation; optionally Build afterward. |
| `Strata: Doctor` | Diagnose helper startup, VS Code storage, Profile adoption, and source health. |
