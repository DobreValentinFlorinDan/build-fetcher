// Renders build/icon.png (512x512): a red blade on a black rounded square, in the app's palette.
// desktop/tauri/icons/ are made from it (the .ico of the .exe, the installer and the shortcuts).
// No image dependencies: shapes are signed-distance tests, 4x4 supersampled, written as a PNG by hand.
//   node scripts/make-icon.mjs
import fs from 'node:fs';
import zlib from 'node:zlib';

const S = 512, SS = 4;
const hex = (h) => [1, 3, 5].map((i) => parseInt(h.slice(i, i + 2), 16));
const mix = (a, b, t) => a.map((v, i) => v + (b[i] - v) * t);
const BG = hex('#0b0b0d'), RING = hex('#3a070b'), DARK = hex('#8f0d17'), LIGHT = hex('#ff4d58'), EDGE = hex('#ffc2c5');

// Rounded square (background + ring)
const RADIUS = 104, HALF = S / 2 - 8;
function roundRect(x, y, inset = 0) {
  const h = HALF - inset, r = RADIUS - inset;
  const qx = Math.abs(x) - (h - r), qy = Math.abs(y) - (h - r);
  return Math.hypot(Math.max(qx, 0), Math.max(qy, 0)) + Math.min(Math.max(qx, qy), 0) - r;
}

// Leaf blade (intersection of two circles) along the u axis, rotated 45 degrees.
const L = 200, W = 54;
const R = (L * L + W * W) / (2 * W), D = R - W;
function blade(x, y, scale = 1, ox = 0, oy = 0) {
  const c = Math.SQRT1_2;
  const u = ((x - ox) * c - (y - oy) * c) / scale, v = ((x - ox) * c + (y - oy) * c) / scale;
  const inside = Math.hypot(u, v - D) <= R && Math.hypot(u, v + D) <= R;
  return inside ? { u, v } : null;
}

function shade(x, y) {
  if (roundRect(x, y) > 0) return [0, 0, 0, 0];
  let col = roundRect(x, y, 14) > 0 ? RING : mix(BG, DARK, Math.max(0, 0.22 - Math.hypot(x, y) / 1400));
  const small = blade(x, y, 0.42, -118, -118) ?? blade(x, y, 0.42, 118, 118);
  if (small) col = mix(DARK, LIGHT, 0.35);
  const b = blade(x, y);
  if (b) {
    col = mix(DARK, LIGHT, (b.u + L) / (2 * L));
    if (Math.abs(b.v) < W * 0.1 && Math.abs(b.u) < L * 0.8) col = mix(col, EDGE, 0.55);  // fuller (spine highlight)
  }
  return [...col, 255];
}

const px = Buffer.alloc(S * (S * 4 + 1));
for (let j = 0; j < S; j++) {
  px[j * (S * 4 + 1)] = 0;  // filter: none
  for (let i = 0; i < S; i++) {
    const acc = [0, 0, 0, 0];
    for (let sy = 0; sy < SS; sy++) for (let sx = 0; sx < SS; sx++) {
      const c = shade(i + (sx + 0.5) / SS - S / 2, j + (sy + 0.5) / SS - S / 2);
      acc[0] += c[0] * c[3]; acc[1] += c[1] * c[3]; acc[2] += c[2] * c[3]; acc[3] += c[3];
    }
    const o = j * (S * 4 + 1) + 1 + i * 4;
    const a = acc[3];
    px[o] = a ? acc[0] / a : 0; px[o + 1] = a ? acc[1] / a : 0; px[o + 2] = a ? acc[2] / a : 0; px[o + 3] = a / (SS * SS);
  }
}

const chunk = (type, data) => {
  const len = Buffer.alloc(4); len.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type), data]);
  const crc = Buffer.alloc(4); crc.writeUInt32BE(zlib.crc32(body));
  return Buffer.concat([len, body, crc]);
};
const ihdr = Buffer.alloc(13);
ihdr.writeUInt32BE(S, 0); ihdr.writeUInt32BE(S, 4); ihdr[8] = 8; ihdr[9] = 6;  // 8-bit RGBA
const png = Buffer.concat([
  Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
  chunk('IHDR', ihdr), chunk('IDAT', zlib.deflateSync(px, { level: 9 })), chunk('IEND', Buffer.alloc(0)),
]);
fs.mkdirSync('build', { recursive: true });
fs.writeFileSync('build/icon.png', png);
console.log(`build/icon.png ${S}x${S} (${png.length} bytes)`);
