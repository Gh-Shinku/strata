import * as esbuild from 'esbuild';
import { copyFile, mkdir } from 'node:fs/promises';

const watch = process.argv.includes('--watch');
const options = {
  entryPoints: ['src/extension.ts'],
  bundle: true,
  platform: 'node',
  format: 'cjs',
  target: 'node20',
  // jsonc-parser publishes both a UMD entry that retains relative requires and
  // a fully bundleable ESM entry. A VSIX only ships this bundle, so prefer ESM.
  mainFields: ['module', 'main'],
  outfile: 'dist/extension.js',
  external: ['vscode'],
  sourcemap: true,
};

if (watch) {
  const context = await esbuild.context(options);
  await context.watch();
} else {
  await esbuild.build(options);
}

await mkdir('dist', { recursive: true });
await copyFile('../schemas/strata.schema.json', 'dist/strata.schema.json');
