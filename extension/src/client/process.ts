import * as path from 'node:path';
import { spawn, ChildProcessWithoutNullStreams } from 'node:child_process';
import * as vscode from 'vscode';

export function startHelper(context: vscode.ExtensionContext): ChildProcessWithoutNullStreams {
  const config = vscode.workspace.getConfiguration('strata');
  const configured = config.get<string>('helperPath')?.trim();
  const executable = configured || path.join(context.extensionPath, 'bin', `${process.platform}-${process.arch}`, process.platform === 'win32' ? 'strata.exe' : 'strata');
  const args = ['serve', '--stdio'];
  const home = config.get<string>('home')?.trim();
  const userData = config.get<string>('userDataDir')?.trim();
  if (home) args.unshift('--home', home);
  if (userData) args.unshift('--user-data-dir', userData);
  return spawn(executable, args, { windowsHide: true, stdio: ['pipe', 'pipe', 'pipe'] });
}

