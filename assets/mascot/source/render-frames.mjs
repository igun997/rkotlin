// Renders a Lottie file to PNG frames with the same Skottie build the player uses (canvaskit-wasm).
// Usage: node script/render-frames.mjs <lottie.json> <out-dir> [size=512] [step=1]
import CanvasKitInit from "canvaskit-wasm/bin/full/canvaskit.js";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const [, , file, outDir, sizeArg = "512", stepArg = "1"] = process.argv;
const size = Number(sizeArg);
const step = Number(stepArg);
const ck = await CanvasKitInit({ locateFile: (f) => join("node_modules/canvaskit-wasm/bin/full", f) });
const anim = ck.MakeAnimation(readFileSync(file, "utf8"));
if (!anim) throw new Error("Skottie could not parse " + file);
const frames = Math.round(anim.duration() * anim.fps());
const surface = ck.MakeSurface(size, size);
const canvas = surface.getCanvas();
mkdirSync(outDir, { recursive: true });
let n = 0;
for (let f = 0; f < frames; f += step) {
  anim.seekFrame(f);
  canvas.clear(ck.TRANSPARENT);
  anim.render(canvas, ck.LTRBRect(0, 0, size, size));
  const png = surface.makeImageSnapshot().encodeToBytes();
  writeFileSync(join(outDir, `f${String(n).padStart(3, "0")}.png`), png);
  n++;
}
console.log(`rendered ${n} frames (${frames} total @ ${anim.fps()} fps) to ${outDir}`);
