import * as vscode from 'vscode';
import { startHelper } from './client/process';
import { RpcClient } from './client/rpc';
import { SettingsIntelligence, UserSettingsSchema } from './settingsIntelligence';

type Profile = {
  name: string;
  settingsPath: string;
};
type SettingAction = {
  label: string;
  description: string;
  method?: string;
  needsProfile: boolean;
  needsValue: boolean;
};

let client: RpcClient | undefined;
let output: vscode.OutputChannel;
let sourcePath: string | undefined;
let previews: PreviewDocumentProvider;
let settingsSchema: UserSettingsSchema;

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  output = vscode.window.createOutputChannel('Strata', { log: true });
  context.subscriptions.push(output);
  previews = new PreviewDocumentProvider();
  settingsSchema = new UserSettingsSchema();
  context.subscriptions.push(previews);
  context.subscriptions.push(vscode.workspace.registerTextDocumentContentProvider('strata-preview', previews));
  context.subscriptions.push(vscode.workspace.onDidCloseTextDocument(document => previews.forget(document.uri)));
  context.subscriptions.push(new SettingsIntelligence(() => sourcePath, settingsSchema));
  try {
    const process = startHelper(context);
    client = new RpcClient(process);
    process.stderr.on('data', chunk => output.append(chunk.toString()));
    process.once('exit', (code, signal) => { if (code && code !== 0) void vscode.window.showErrorMessage(`Strata helper exited (${code ?? signal}). See output for details.`); });
    const initialized = await client.request<{ helperVersion: string }>('initialize', { protocolVersion: 1, clientVersion: context.extension.packageJSON.version });
    output.appendLine(`Connected to helper ${initialized.helperVersion}`);
  } catch (error) {
    output.appendLine(String(error));
    void vscode.window.showErrorMessage('Strata helper could not start. Run “Strata: Doctor” after configuring helperPath.');
  }

  const command = (name: string, handler: () => Promise<void>) => context.subscriptions.push(vscode.commands.registerCommand(`strata.${name}`, () => guarded(handler)));
  command('build', buildSource);
  command('preview', buildPreview);
  command('open', openConfiguration);
  command('doctor', async () => showDocument('doctor', await rpc().request('doctor')));
  command('crud', () => editSetting(selectedSettingId()));
  command('export', exportProfiles);
  command('import', importProfiles);
  context.subscriptions.push(vscode.workspace.onDidChangeConfiguration(() => client?.notify('configuration/changed')));
  const statusBar = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 20);
  statusBar.name = 'Strata';
  statusBar.text = '$(layers) Strata';
  statusBar.tooltip = 'Open Strata Configuration';
  statusBar.command = 'strata.open';
  statusBar.show();
  context.subscriptions.push(statusBar);
  if (client) void initialSync();
}

async function guarded(handler: () => Promise<void>): Promise<void> {
  try { await handler(); } catch (error) { output.appendLine(String(error)); void vscode.window.showErrorMessage(`Strata: ${error instanceof Error ? error.message : String(error)}`); }
}

function rpc(): RpcClient { if (!client) throw new Error('helper is not connected'); return client; }

async function initialSync(): Promise<void> {
  try {
    const status = await rpc().request<{ initialized: boolean; sourcePath: string }>('status');
    if (status.initialized) {
      sourcePath = status.sourcePath;
      const result = await rpc().request('reconcile');
      output.appendLine(`Initial reconcile: ${JSON.stringify(result)}`);
      return;
    }
    output.appendLine('Strata source is missing. Waiting for build confirmation.');
    const action = await vscode.window.showInformationMessage(
      'Strata needs to build a source from your existing VS Code Profiles before it can reconcile settings.',
      'Build', 'Doctor',
    );
    if (action === 'Build') await guarded(buildSource);
    if (action === 'Doctor') await guarded(async () => showDocument('doctor', await rpc().request('doctor')));
  } catch (error) {
    output.appendLine(`Initial status check failed: ${String(error)}`);
  }
}

async function buildSource(): Promise<void> {
  const knownSettingPrefixes = await settingsSchema.knownSettingPrefixes();
  const applied = await rpc().request<{ sourcePath: string }>('build/apply', { knownSettingPrefixes });
  sourcePath = applied.sourcePath;
  const result = await rpc().request('compile');
  output.appendLine(`Build complete: ${JSON.stringify(result)}`);
  await vscode.window.showTextDocument(
    await vscode.workspace.openTextDocument(vscode.Uri.file(applied.sourcePath)),
    { preview: false },
  );
  void vscode.window.showInformationMessage('Strata source built.');
}

async function buildPreview(): Promise<void> {
  const knownSettingPrefixes = await settingsSchema.knownSettingPrefixes();
  const preview = await rpc().request('build/preview', { knownSettingPrefixes });
  await showBuildPreview(preview);
}

async function openConfiguration(): Promise<void> {
  const status = await rpc().request<{ initialized: boolean; sourcePath: string }>('status');
  if (!status.initialized) {
    await buildSource();
    const updated = await rpc().request<{ initialized: boolean }>('status');
    if (!updated.initialized) return;
  }
  const result = await rpc().request<{ path: string }>('source/path');
  sourcePath = result.path;
  await vscode.window.showTextDocument(await vscode.workspace.openTextDocument(vscode.Uri.file(result.path)));
}

async function selectProfile(): Promise<string | undefined> {
  const profiles = await rpc().request<Profile[]>('profiles/list');
  return (await vscode.window.showQuickPick(profiles.map(p => p.name), { title: 'Select VS Code Profile' })) ?? undefined;
}

async function profileForActiveSettingsFile(): Promise<string | undefined> {
  const editor = vscode.window.activeTextEditor;
  if (!editor || editor.document.fileName.toLowerCase().split(/[\\/]/).pop() !== 'settings.json') return undefined;
  const activePath = normalizePath(editor.document.fileName);
  const matches = (await rpc().request<Profile[]>('profiles/list'))
    .filter(profile => normalizePath(profile.settingsPath) === activePath);
  if (matches.length === 1) return matches[0]?.name;
  if (matches.length > 1) output.appendLine(`Cannot infer one Profile for shared settings file: ${editor.document.fileName}`);
  return undefined;
}

async function settingId(value?: string): Promise<string | undefined> {
  return vscode.window.showInputBox({
    title: 'Setting ID',
    prompt: 'For example: editor.fontSize',
    value,
    validateInput: candidate => isSettingId(candidate.trim()) ? undefined : 'Select or enter one complete setting ID.',
  });
}

async function editSetting(selectedSetting?: string): Promise<void> {
  const action = await vscode.window.showQuickPick<SettingAction>([
    { label: 'Query', description: 'Show where the setting is visible from for a Profile.', needsProfile: true, needsValue: false },
    { label: 'Base', description: 'Put this setting and its value in Base.', method: 'settings/setBase', needsProfile: false, needsValue: true },
    { label: 'Inherit', description: 'Remove the override and inherit the Base value.', method: 'settings/inherit', needsProfile: true, needsValue: false },
    { label: 'Uninherit', description: 'Do not materialize the Base setting for this Profile.', method: 'settings/uninherit', needsProfile: true, needsValue: false },
  ], { title: 'Strata: CRUD' });
  if (!action) return;
  const profile = action.needsProfile ? await profileForActiveSettingsFile() ?? await selectProfile() : undefined;
  if (action.needsProfile && !profile) return;
  const setting = selectedSetting ?? await settingId();
  if (!setting) return;
  if (!action.method) {
    await showDocument('query', await rpc().request('settings/origin', { profile, setting }));
    return;
  }
  await applyCrudMutation(action.method, setting, profile, action.needsValue);
}

async function applyCrudMutation(method: string, setting: string, profile: string | undefined, needsValue: boolean): Promise<void> {
  let value: unknown;
  if (needsValue) {
    const text = await vscode.window.showInputBox({ title: 'JSON setting value', prompt: 'Enter a JSON value' });
    if (text === undefined) return;
    try { value = JSON.parse(text); } catch { throw new Error('value is not valid JSON'); }
  }
  await rpc().request(method, { ...(profile ? { profile } : {}), setting, ...(needsValue ? { value } : {}) });
  await rpc().request('compile');
  void vscode.window.showInformationMessage('Strata source and materialized profiles updated.');
}

function selectedSettingId(): string | undefined {
  const editor = vscode.window.activeTextEditor;
  if (!editor || editor.selection.isEmpty || editor.document.fileName.toLowerCase().split(/[\\/]/).pop() !== 'settings.json') return undefined;
  const text = editor.document.getText(editor.selection).trim();
  if (!text) return undefined;
  try {
    const parsed = JSON.parse(text);
    return typeof parsed === 'string' && isSettingId(parsed) ? parsed : undefined;
  } catch {
    return isSettingId(text) ? text : undefined;
  }
}

function isSettingId(value: string): boolean {
  return /^(?:[A-Za-z0-9][A-Za-z0-9._-]*|\[[^\]\r\n]+\])$/.test(value);
}

function normalizePath(path: string): string {
  const normalized = path.replace(/\\/g, '/').toLowerCase();
  return normalized.startsWith('//?/') ? normalized.slice(4) : normalized;
}

async function exportProfiles(): Promise<void> {
  const selected = await vscode.window.showOpenDialog({
    canSelectFiles: false,
    canSelectFolders: true,
    canSelectMany: false,
    openLabel: 'Export Profiles Here',
    title: 'Choose a directory for the Strata profile archive',
  });
  const destination = selected?.[0];
  if (!destination) return;
  const result = await rpc().request<{ path: string }>('profiles/export', { destination: destination.fsPath });
  void vscode.window.showInformationMessage(`Strata profiles exported to ${result.path}.`);
}

async function importProfiles(): Promise<void> {
  const selected = await vscode.window.showOpenDialog({
    canSelectFiles: false,
    canSelectFolders: true,
    canSelectMany: false,
    openLabel: 'Import Profile Archive',
    title: 'Select a Strata profile archive directory',
  });
  const archive = selected?.[0];
  if (!archive) return;
  const choice = await vscode.window.showWarningMessage(
    'Import Strata profiles? This overwrites matching VS Code Profile files and replaces the registered Profile list from the archive.',
    { modal: true, detail: 'The archive must have been created by Strata: Export.' },
    'Import',
  );
  if (choice !== 'Import') return;
  const result = await rpc().request<{ profiles: number }>('profiles/import', { archive: archive.fsPath });
  const next = await vscode.window.showInformationMessage(
    `Imported ${result.profiles} profiles. Build the Strata source from the imported settings?`,
    'Build',
  );
  if (next === 'Build') await buildSource();
}

async function showDocument(command: string, value: unknown): Promise<void> {
  const title = `strata ${command}`;
  await previews.show(title, JSON.stringify(value, null, 2), 'json');
  output.appendLine(title);
}

async function showBuildPreview(value: unknown): Promise<void> {
  const content = `// STRATA BUILD PREVIEW ONLY\n// This is a read-only virtual document, not the active Strata source.\n${JSON.stringify(value, null, 2)}\n`;
  await previews.show('strata build preview', content, 'jsonc');
  output.appendLine('Strata Build Preview opened (read-only virtual document).');
}

export async function deactivate(): Promise<void> { await client?.dispose(); client = undefined; }

/** Supplies read-only, in-memory documents so preview tabs never become dirty. */
class PreviewDocumentProvider implements vscode.TextDocumentContentProvider, vscode.Disposable {
  private readonly contents = new Map<string, string>();
  private sequence = 0;

  provideTextDocumentContent(uri: vscode.Uri): string {
    return this.contents.get(uri.toString()) ?? 'This Strata preview is no longer available.';
  }

  forget(uri: vscode.Uri): void {
    if (uri.scheme === 'strata-preview') this.contents.delete(uri.toString());
  }

  dispose(): void {
    this.contents.clear();
  }

  async show(title: string, content: string, language: string): Promise<void> {
    const uri = vscode.Uri.from({
      scheme: 'strata-preview',
      authority: 'view',
      path: `/${title}`,
      query: String(++this.sequence),
    });
    this.contents.set(uri.toString(), content);
    const document = await vscode.workspace.openTextDocument(uri);
    await vscode.languages.setTextDocumentLanguage(document, language);
    await vscode.window.showTextDocument(document, { preview: true });
  }
}
