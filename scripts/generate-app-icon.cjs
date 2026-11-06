// Usage: node scripts/generate-app-icon.cjs path/to/master.png
// Asset preparation only: requires sharp and the Windows SDK resource compiler.
// Generated resources are checked in; cargo builds need neither dependency.
const fs = require('node:fs');
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const sharp = require('sharp');

const output = path.resolve(__dirname, '../assets/icon');
const source = process.argv[2];
if (!source) {
  throw new Error('Provide the path to the master PNG.');
}

function bitmap(rgba, size) {
  const maskStride = Math.ceil(size / 32) * 4;
  const result = Buffer.alloc(40 + size * size * 4 + size * maskStride);
  result.writeUInt32LE(40, 0);
  result.writeInt32LE(size, 4);
  result.writeInt32LE(size * 2, 8);
  result.writeUInt16LE(1, 12);
  result.writeUInt16LE(32, 14);
  result.writeUInt32LE(result.length - 40, 20);
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      const input = (y * size + x) * 4;
      const destination = 40 + ((size - 1 - y) * size + x) * 4;
      result[destination] = rgba[input + 2];
      result[destination + 1] = rgba[input + 1];
      result[destination + 2] = rgba[input];
      result[destination + 3] = rgba[input + 3];
      if (rgba[input + 3] === 0) {
        const mask = 40 + size * size * 4 + (size - 1 - y) * maskStride;
        result[mask + (x >> 3)] |= 0x80 >> (x & 7);
      }
    }
  }
  return result;
}

function resourceCompiler() {
  const probe = spawnSync('rc.exe', ['/?'], { windowsHide: true });
  if (!probe.error) return 'rc.exe';
  const sdk = path.join(process.env['ProgramFiles(x86)'] || 'C:/Program Files (x86)',
    'Windows Kits/10/bin');
  const architecture = process.arch === 'arm64' ? 'arm64' : 'x64';
  const versions = fs.readdirSync(sdk).filter(name => /^\d+\.\d+/.test(name))
    .sort((a, b) => b.localeCompare(a, undefined, { numeric: true }));
  for (const version of versions) {
    const compiler = path.join(sdk, version, architecture, 'rc.exe');
    if (fs.existsSync(compiler)) return compiler;
  }
  throw new Error('Windows SDK rc.exe was not found.');
}

async function main() {
  const compiler = resourceCompiler();
  fs.mkdirSync(output, { recursive: true });
  const sizes = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256];
  const frames = [];
  for (const size of sizes) {
    const resized = sharp(source).resize(size, size).toColourspace('srgb').ensureAlpha();
    const rgba = await resized.clone().raw().toBuffer();
    frames.push(size === 256 ? await resized.png().toBuffer() : bitmap(rgba, size));
    if (size === 256) {
      fs.writeFileSync(path.join(output, 'flowsurface.rgba'), rgba);
      fs.writeFileSync(path.join(output, 'flowsurface.png'), frames.at(-1));
    }
  }
  const header = Buffer.alloc(6 + frames.length * 16);
  header.writeUInt16LE(1, 2);
  header.writeUInt16LE(frames.length, 4);
  let offset = header.length;
  frames.forEach((frame, index) => {
    const entry = 6 + index * 16;
    header[entry] = sizes[index] === 256 ? 0 : sizes[index];
    header[entry + 1] = header[entry];
    header.writeUInt16LE(1, entry + 4);
    header.writeUInt16LE(32, entry + 6);
    header.writeUInt32LE(frame.length, entry + 8);
    header.writeUInt32LE(offset, entry + 12);
    offset += frame.length;
  });
  fs.writeFileSync(path.join(output, 'flowsurface.ico'), Buffer.concat([header, ...frames]));
  const compiled = spawnSync(compiler,
    ['/nologo', '/fo', 'flowsurface.res', 'flowsurface.rc'],
    { cwd: output, encoding: 'utf8', windowsHide: true });
  if (compiled.error || compiled.status !== 0) {
    throw compiled.error || new Error(compiled.stdout + compiled.stderr);
  }
  console.log(`Generated PNG, RGBA, ICO (${sizes.join(', ')} px), and Windows resource in ${output}`);
}

main().catch(error => { console.error(error); process.exitCode = 1; });
