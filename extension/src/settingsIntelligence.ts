import * as vscode from 'vscode';
import { findNodeAtOffset, parseTree, type Node } from 'jsonc-parser';

type JsonSchema = {
  additionalProperties?: boolean | JsonSchema;
  default?: unknown;
  description?: string;
  enum?: unknown[];
  markdownDescription?: string;
  patternProperties?: Record<string, JsonSchema>;
  properties?: Record<string, JsonSchema>;
  type?: string | string[];
};

type Scope = 'base' | 'profile' | 'uninherit' | undefined;

type Context = {
  scope: Scope;
  setting?: string;
  profile?: string;
};

const USER_SETTINGS_SCHEMA = vscode.Uri.parse('vscode://schemas/settings/user');

/**
 * One shared, lazily loaded view of VS Code's generated settings schema.
 * VS Code already incorporates currently available setting contributors here.
 */
export class UserSettingsSchema {
  private schemaPromise: Promise<JsonSchema | undefined> | undefined;

  async get(): Promise<JsonSchema | undefined> {
    this.schemaPromise ??= this.load();
    return this.schemaPromise;
  }

  async knownSettingPrefixes(): Promise<string[]> {
    const schema = await this.get();
    if (!schema) throw new Error('VS Code settings schema is unavailable; cannot classify Base settings.');
    return [...new Set(Object.keys(schema.properties ?? {}).map(settingPrefix))].sort();
  }

  private async load(): Promise<JsonSchema | undefined> {
    try {
      const document = await vscode.workspace.openTextDocument(USER_SETTINGS_SCHEMA);
      return JSON.parse(document.getText()) as JsonSchema;
    } catch (error) {
      console.warn(`Strata could not load the VS Code settings schema: ${String(error)}`);
      return undefined;
    }
  }
}

/**
 * Bridges Strata's nested source format to VS Code's dynamically generated
 * User Settings schema. The schema includes installed extensions' settings.
 */
export class SettingsIntelligence implements vscode.HoverProvider, vscode.CompletionItemProvider, vscode.Disposable {
  private readonly disposables: vscode.Disposable[];

  constructor(
    private readonly sourcePath: () => string | undefined,
    private readonly schema: UserSettingsSchema,
  ) {
    const selector: vscode.DocumentSelector = { language: 'jsonc', scheme: 'file' };
    this.disposables = [
      vscode.languages.registerHoverProvider(selector, this),
      vscode.languages.registerCompletionItemProvider(selector, this, '"', '.', '['),
    ];
  }

  dispose(): void {
    for (const disposable of this.disposables) disposable.dispose();
  }

  async provideHover(document: vscode.TextDocument, position: vscode.Position): Promise<vscode.Hover | undefined> {
    if (!this.isSource(document)) return undefined;
    const root = parseTree(document.getText());
    if (!root) return undefined;
    const node = findNodeAtOffset(root, document.offsetAt(position), true);
    if (!node) return undefined;
    const context = contextAt(node);
    if (!context.scope) return undefined;

    if (!context.setting || context.setting.startsWith('$')) return undefined;
    const contents = new vscode.MarkdownString();
    appendSettingHoverDescription(contents, await this.settingSchema(context.setting));
    return new vscode.Hover(contents);
  }

  async provideCompletionItems(document: vscode.TextDocument, position: vscode.Position): Promise<vscode.CompletionItem[] | undefined> {
    if (!this.isSource(document)) return undefined;
    const root = parseTree(document.getText());
    if (!root) return undefined;
    const node = findNodeAtOffset(root, document.offsetAt(position), true) ?? root;
    const context = contextForCompletion(node);
    if (!context.scope) return undefined;

    if (context.scope === 'uninherit') return this.uninheritCompletions(root);
    const schema = await this.userSettingsSchema();
    if (!schema) return undefined;
    const items = Object.entries(schema.properties ?? {}).map(([setting, settingSchema]) => settingCompletion(setting, settingSchema));
    if (context.scope === 'profile') items.unshift(uninheritCompletion());
    return items;
  }

  private isSource(document: vscode.TextDocument): boolean {
    const source = this.sourcePath();
    return source !== undefined && normalizePath(document.uri.fsPath) === normalizePath(source);
  }

  private async userSettingsSchema(): Promise<JsonSchema | undefined> {
    return this.schema.get();
  }

  private async settingSchema(setting: string): Promise<JsonSchema | undefined> {
    const schema = await this.userSettingsSchema();
    if (!schema) return undefined;
    const exact = schema.properties?.[setting];
    if (exact) return exact;
    for (const [pattern, candidate] of Object.entries(schema.patternProperties ?? {})) {
      try {
        if (new RegExp(pattern).test(setting)) return candidate;
      } catch {
        // Ignore an invalid third-party schema pattern and continue with the rest.
      }
    }
    return undefined;
  }

  private uninheritCompletions(root: Node): vscode.CompletionItem[] {
    return baseSettingNames(root).map(setting => {
      const item = new vscode.CompletionItem(setting, vscode.CompletionItemKind.Value);
      item.insertText = JSON.stringify(setting);
      item.detail = 'Strata Base setting';
      item.documentation = 'Stop inheriting this Base setting for the current Profile.';
      return item;
    });
  }
}

function contextAt(node: Node): Context {
  const path = propertyPath(node);
  if (path[0] === 'base' && path.length >= 2) {
    return { scope: 'base', setting: settingFromPath(path, 1) };
  }
  if (path[0] === 'profiles' && path.length >= 3) {
    const profile = path[1];
    if (path[2] === 'uninherit') {
      return { scope: 'uninherit', profile, setting: node.type === 'string' ? String(node.value) : undefined };
    }
    return { scope: 'profile', profile, setting: settingFromPath(path, 2) };
  }
  return { scope: undefined };
}

function contextForCompletion(node: Node): Context {
  const array = nearestArray(node);
  if (array) {
    const arrayPath = propertyPath(array);
    if (arrayPath[0] === 'profiles' && arrayPath[2] === 'uninherit') {
      return { scope: 'uninherit', profile: arrayPath[1] };
    }
  }
  const object = nearestObject(node);
  if (!object) return { scope: undefined };
  const path = propertyPath(object);
  if (path.length === 1 && path[0] === 'base') return { scope: 'base' };
  if (path.length === 2 && path[0] === 'profiles') return { scope: 'profile', profile: path[1] };
  const second = path[1];
  const third = path[2];
  if (path.length === 2 && path[0] === 'base' && second?.startsWith('[')) return { scope: 'base' };
  if (path.length === 3 && path[0] === 'profiles' && third?.startsWith('[')) return { scope: 'profile', profile: second };
  if (path.length === 3 && path[0] === 'profiles' && path[2] === 'uninherit') return { scope: 'uninherit', profile: path[1] };
  return { scope: undefined };
}

function settingFromPath(path: string[], index: number): string | undefined {
  const first = path[index];
  return first?.startsWith('[') ? path[index + 1] : first;
}

function propertyPath(node: Node): string[] {
  const path: string[] = [];
  let current: Node | undefined = node;
  while (current) {
    if (current.type === 'property') {
      const name = current.children?.[0]?.value;
      if (typeof name === 'string') path.unshift(name);
    }
    current = current.parent;
  }
  return path;
}

function nearestObject(node: Node): Node | undefined {
  let current: Node | undefined = node;
  while (current) {
    if (current.type === 'object') return current;
    current = current.parent;
  }
  return undefined;
}

function nearestArray(node: Node): Node | undefined {
  let current: Node | undefined = node;
  while (current) {
    if (current.type === 'array') return current;
    current = current.parent;
  }
  return undefined;
}

function baseSettingNames(root: Node): string[] {
  const base = root.children?.find(child => propertyName(child) === 'base');
  const object = base?.children?.[1];
  if (object?.type !== 'object') return [];
  return object.children?.map(propertyName).filter((name): name is string => name !== undefined) ?? [];
}

function propertyName(node: Node): string | undefined {
  if (node.type !== 'property') return undefined;
  const value = node.children?.[0]?.value;
  return typeof value === 'string' ? value : undefined;
}

function settingCompletion(setting: string, schema: JsonSchema): vscode.CompletionItem {
  const item = new vscode.CompletionItem(setting, vscode.CompletionItemKind.Property);
  item.insertText = JSON.stringify(setting);
  item.detail = schemaType(schema);
  const documentation = new vscode.MarkdownString();
  appendSettingDocumentation(documentation, setting, schema);
  item.documentation = documentation;
  return item;
}

function uninheritCompletion(): vscode.CompletionItem {
  const item = new vscode.CompletionItem('uninherit', vscode.CompletionItemKind.Property);
  item.insertText = new vscode.SnippetString('"uninherit": [\n\t"$1"\n]');
  item.detail = 'Strata inheritance control';
  item.documentation = 'Stops inheriting selected Base settings for this Profile.';
  return item;
}

function appendSettingDocumentation(markdown: vscode.MarkdownString, setting: string, schema: JsonSchema | undefined): void {
  markdown.appendMarkdown(`**${setting}**`);
  if (!schema) {
    markdown.appendMarkdown('\n\nVS Code has no schema entry for this setting. It may be contributed by an unavailable extension.');
    return;
  }
  const description = schema.markdownDescription ?? schema.description;
  if (description) markdown.appendMarkdown(`\n\n${description}`);
  const type = schemaType(schema);
  if (type) markdown.appendMarkdown(`\n\nType: \`${type}\``);
  if (schema.enum?.length) markdown.appendMarkdown(`\n\nAllowed: ${schema.enum.map(value => `\`${JSON.stringify(value)}\``).join(', ')}`);
  if (schema.default !== undefined) markdown.appendMarkdown(`\n\nDefault: \`${JSON.stringify(schema.default)}\``);
}

/** Hover stays deliberately minimal: show only the upstream schema description. */
function appendSettingHoverDescription(markdown: vscode.MarkdownString, schema: JsonSchema | undefined): void {
  if (!schema) {
    markdown.appendMarkdown('VS Code has no schema entry for this setting. It may be contributed by an unavailable extension.');
    return;
  }
  const description = schema.description ?? schema.markdownDescription;
  if (description) markdown.appendMarkdown(description);
}

function schemaType(schema: JsonSchema): string {
  return Array.isArray(schema.type) ? schema.type.join(' | ') : schema.type ?? 'setting';
}

function settingPrefix(setting: string): string {
  const separator = setting.indexOf('.');
  return separator === -1 ? setting : setting.slice(0, separator);
}

function normalizePath(path: string): string {
  return process.platform === 'win32' ? path.toLowerCase() : path;
}
