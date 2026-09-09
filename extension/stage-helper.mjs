import { spawnSync } from 'node:child_process';
import { copyFileSync, mkdirSync } from 'node:fs';
import path from 'node:path';

const cargo = process.env.CARGO || 'cargo';
const build = spawnSync(cargo, ['build', '--release', '-p', 'strata-cli'], {
  cwd: '..', stdio: 'inherit',
});
if (build.status !== 0) process.exit(build.status ?? 1);

const executable = process.platform === 'win32' ? 'strata.exe' : 'strata';
const destination = path.join('bin', `${process.platform}-${process.arch}`);
mkdirSync(destination, { recursive: true });
copyFileSync(path.join('..', 'target', 'release', executable), path.join(destination, executable));
