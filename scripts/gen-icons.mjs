#!/usr/bin/env node
/**
 * Generate the desktop shell icons.
 *
 * A dependency-free PNG encoder: the desktop bundle needs at least one icon, and pulling a
 * graphics toolchain into the repo for a placeholder is not worth it. Replace the output with
 * real artwork via "tauri icon" whenever you like:
 *
 *   pnpm --filter @agentos/desktop exec tauri icon path/to/logo.png
 */

import { deflateSync } from "node:zlib";
import { mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";

const outDir = path.resolve("apps/desktop/src-tauri/icons");
mkdirSync(outDir, { recursive: true });

function crc32(buffer) {
  let crc = 0xffffffff;
  for (const byte of buffer) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit += 1) {
      crc = crc & 1 ? (crc >>> 1) ^ 0xedb88320 : crc >>> 1;
    }
  }
  return (crc ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length, 0);
  const typeAndData = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(typeAndData), 0);
  return Buffer.concat([length, typeAndData, crc]);
}

function png(size) {
  const raw = Buffer.alloc(size * (size * 4 + 1));
  const center = size / 2;
  for (let y = 0; y < size; y += 1) {
    const rowStart = y * (size * 4 + 1);
    raw[rowStart] = 0; // filter: none
    for (let x = 0; x < size; x += 1) {
      const offset = rowStart + 1 + x * 4;
      const dx = x - center + 0.5;
      const dy = y - center + 0.5;
      const distance = Math.sqrt(dx * dx + dy * dy) / center;
      // A dark disc with a brighter ring: recognisable at 32px, no assets required.
      const inside = distance < 0.92;
      const ring = distance > 0.72 && distance < 0.86;
      const glow = Math.max(0, 1 - distance);
      const r = inside ? (ring ? 90 : 24) + glow * 20 : 0;
      const g = inside ? (ring ? 200 : 40) + glow * 60 : 0;
      const b = inside ? (ring ? 240 : 90) + glow * 40 : 0;
      raw[offset] = Math.min(255, Math.round(r));
      raw[offset + 1] = Math.min(255, Math.round(g));
      raw[offset + 2] = Math.min(255, Math.round(b));
      raw[offset + 3] = inside ? 255 : 0;
    }
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  ihdr[10] = 0;
  ihdr[11] = 0;
  ihdr[12] = 0;
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

/**
 * ICO container with PNG-compressed images (supported since Windows Vista). tauri-build needs
 * icon.ico on Windows to generate the executable resource.
 */
function ico(images) {
  const header = Buffer.alloc(6);
  header.writeUInt16LE(0, 0);
  header.writeUInt16LE(1, 2);
  header.writeUInt16LE(images.length, 4);
  const entries = [];
  let offset = 6 + images.length * 16;
  const payloads = [];
  for (const { size, data } of images) {
    const entry = Buffer.alloc(16);
    entry[0] = size >= 256 ? 0 : size;
    entry[1] = size >= 256 ? 0 : size;
    entry[2] = 0;
    entry[3] = 0;
    entry.writeUInt16LE(1, 4);
    entry.writeUInt16LE(32, 6);
    entry.writeUInt32LE(data.length, 8);
    entry.writeUInt32LE(offset, 12);
    entries.push(entry);
    payloads.push(data);
    offset += data.length;
  }
  return Buffer.concat([header, ...entries, ...payloads]);
}

const sizes = [
  [32, "32x32.png"],
  [128, "128x128.png"],
  [256, "128x128@2x.png"],
  [512, "icon.png"],
];

const rendered = new Map();
for (const [size, name] of sizes) {
  const data = png(size);
  rendered.set(size, data);
  const file = path.join(outDir, name);
  writeFileSync(file, data);
  console.log("wrote " + file);
}

const icoFile = path.join(outDir, "icon.ico");
writeFileSync(
  icoFile,
  ico([
    { size: 32, data: rendered.get(32) },
    { size: 128, data: rendered.get(128) },
    { size: 256, data: rendered.get(256) },
  ]),
);
console.log("wrote " + icoFile);

// Square logos are referenced by the default Tauri bundle configuration.
for (const [size, name] of [[30, "Square30x30Logo.png"], [44, "Square44x44Logo.png"], [71, "Square71x71Logo.png"], [89, "Square89x89Logo.png"], [107, "Square107x107Logo.png"], [142, "Square142x142Logo.png"], [150, "Square150x150Logo.png"], [284, "Square284x284Logo.png"], [310, "Square310x310Logo.png"], [50, "StoreLogo.png"]]) {
  writeFileSync(path.join(outDir, name), png(size));
}
console.log("wrote Square* and Store logos");