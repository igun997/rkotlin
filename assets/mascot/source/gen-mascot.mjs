// Generates the rkotlin mascot ("Kodi", a little build robot on a remote cloud) as Lottie JSON.
//   player variant: public/projects/main-project/scene-1/{lottie,controls}.json (with Skottie slots)
//   app variant:    <out>/mascot-lottie.json (no slots, plain values; for lottie-android, which ignores `sid`)
// Usage: node script/gen-mascot.mjs [app-out-dir]
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

const FR = 30;
const OP = 120;

const hex = (h, a = 1) => {
  const n = parseInt(h.slice(1), 16);
  return [((n >> 16) & 255) / 255, ((n >> 8) & 255) / 255, (n & 255) / 255, a];
};

const EASE = { i: { x: [0.45], y: [1] }, o: { x: [0.55], y: [0] } };
const LINEAR = { i: { x: [0.833], y: [0.833] }, o: { x: [0.167], y: [0.167] } };

/** keyframed property: frames = [[t, value|[values]], ...]; the last frame holds. */
const kf = (frames, ease = EASE) => ({
  a: 1,
  k: frames.map(([t, v], idx) => {
    const s = Array.isArray(v) ? v : [v];
    return idx === frames.length - 1 ? { t, s } : { t, s, ...ease };
  }),
});
const st = (v) => ({ a: 0, k: v });

function build(withSlots) {
  const slots = {};
  const colorProp = (sid, h) => {
    const c = hex(h);
    if (!withSlots) return st(c);
    slots[sid] = { p: { a: 0, k: c } };
    return { sid };
  };

  const tr = (o = {}) => ({
    ty: "tr",
    p: o.p ?? st([0, 0]),
    a: o.a ?? st([0, 0]),
    s: o.s ?? st([100, 100]),
    r: o.r ?? st(0),
    o: o.o ?? st(100),
  });
  const group = (nm, items, t = {}) => ({ ty: "gr", nm, it: [...items, tr(t)] });
  const fill = (c, o = 100) => ({ ty: "fl", c: Array.isArray(c) ? st(c) : c, o: st(o), r: 1 });
  const stroke = (c, w) => ({ ty: "st", c: Array.isArray(c) ? st(c) : c, o: st(100), w: st(w), lc: 2, lj: 2 });
  const rect = (x, y, w, h, r = 0) => ({ ty: "rc", p: st([x, y]), s: st([w, h]), r: st(r) });
  const ell = (x, y, w, h) => ({ ty: "el", p: st([x, y]), s: st([w, h]) });
  const grad = (stops, s, e) => {
    const flat = stops.flatMap(([off, h]) => [off, ...hex(h).slice(0, 3)]);
    return {
      ty: "gf",
      o: st(100),
      r: 1,
      t: 1,
      s: st(s),
      e: st(e),
      g: { p: stops.length, k: st(flat) },
    };
  };
  const bodyGrad = (s, e) =>
    grad([[0, "#7F52FF"], [0.55, "#C711E1"], [1, "#FF8A2B"]], s, e);

  const layer = (nm, shapes, ks = {}) => ({
    ty: 4,
    nm,
    ip: 0,
    op: OP,
    st: 0,
    ks: {
      o: ks.o ?? st(100),
      r: st(0),
      a: st([0, 0, 0]),
      s: st([100, 100, 100]),
      p: ks.p ?? st([0, 0, 0]),
    },
    // shapes are written bottom-first; Lottie draws the first item on top
    shapes: [...shapes].reverse(),
  });

  // ---- palette (slots: the properties panel can recolor these)
  const bg = () => colorProp("bgColor", "#140F2B");
  const cloud = () => colorProp("cloudColor", "#CFC4FF");
  const head = () => colorProp("headColor", "#F4EEFF");
  const eye = () => colorProp("eyeColor", "#35F2C8");
  const gearC = () => colorProp("gearColor", "#FFB020");

  const RX = 256;
  const RY = 168; // robot origin (head center)

  // ---- robot (everything relative to the robot origin, placed by one group transform)
  const bob = kf([[0, [0, 0, 0]], [30, [0, -9, 0]], [60, [0, 0, 0]], [90, [0, -9, 0]], [120, [0, 0, 0]]]);

  const blink = kf([[0, [100, 100]], [96, [100, 100]], [100, [100, 8]], [105, [100, 100]], [120, [100, 100]]]);
  const wave = kf([[0, 0], [40, 0], [50, -32], [58, 12], [66, -32], [74, 12], [84, 0], [120, 0]]);
  const bulbPulse = kf([[0, [100, 100]], [15, [135, 135]], [30, [100, 100]], [45, [135, 135]], [60, [100, 100]], [75, [135, 135]], [90, [100, 100]], [105, [135, 135]], [120, [100, 100]]]);
  const glowPulse = kf([[0, 25], [15, 70], [30, 25], [45, 70], [60, 25], [75, 70], [90, 25], [105, 70], [120, 25]]);

  const robot = layer(
    "robot",
    [
      // antenna
      group("antenna-glow", [ell(0, 0, 44, 44), fill(hex("#FFA21F"))], { p: st([0, -116]), s: bulbPulse, o: glowPulse }),
      group("antenna", [rect(0, -92, 6, 36, 3), fill(hex("#B9A8F5"))]),
      group("antenna-bulb", [ell(0, 0, 22, 22), fill(hex("#FFA21F"))], { p: st([0, -116]) }),
      // arms (behind the body)
      group("arm-left", [rect(0, 26, 20, 56, 10), fill(hex("#9B6BFF"))], { p: st([-72, 90]), r: st(14) }),
      group("arm-right", [rect(0, 26, 20, 56, 10), fill(hex("#9B6BFF"))], { p: st([72, 90]), r: wave }),
      // body
      group("body", [rect(0, 112, 128, 80, 30), bodyGrad([-60, 80], [60, 150])]),
      // terminal prompt on the chest
      group("prompt", [
        {
          ty: "sh",
          ks: st({ c: false, v: [[-16, -10], [-3, 0], [-16, 10]], i: [[0, 0], [0, 0], [0, 0]], o: [[0, 0], [0, 0], [0, 0]] }),
        },
        stroke(hex("#FFFFFF"), 6),
      ], { p: st([-6, 112]) }),
      group("prompt-cursor", [
        { ty: "sh", ks: st({ c: false, v: [[0, 0], [16, 0]], i: [[0, 0], [0, 0]], o: [[0, 0], [0, 0]] }) },
        stroke(hex("#FFFFFF"), 6),
      ], { p: st([10, 122]) }),
      // ears
      group("ear-left", [rect(-104, 0, 20, 46, 10), bodyGrad([-104, -23], [-104, 23])]),
      group("ear-right", [rect(104, 0, 20, 46, 10), bodyGrad([104, -23], [104, 23])]),
      // head + visor
      group("head", [rect(0, 0, 192, 152, 46), fill(head())]),
      group("visor", [rect(0, 0, 150, 98, 32), fill(hex("#1E1640"))]),
      // eyes (blink = scale Y about the eye center)
      group("eye-left", [rect(0, 0, 22, 36, 11), fill(eye())], { p: st([-30, -6]), s: blink }),
      group("eye-right", [rect(0, 0, 22, 36, 11), fill(eye())], { p: st([30, -6]), s: blink }),
      group("cheek-left", [ell(0, 0, 18, 10), fill(hex("#FF7AAE"), 55)], { p: st([-52, 24]) }),
      group("cheek-right", [ell(0, 0, 18, 10), fill(hex("#FF7AAE"), 55)], { p: st([52, 24]) }),
      group("mouth", [
        { ty: "sh", ks: st({ c: false, v: [[-14, 0], [14, 0]], i: [[0, 0], [-6, 12]], o: [[6, 12], [0, 0]] }) },
        stroke(eye(), 5),
      ], { p: st([0, 20]) }),
    ],
    { p: bob }
  );
  // move the whole robot to its origin
  robot.ks.a = st([0, 0, 0]);
  robot.shapes = [group("robot-root", robot.shapes, { p: st([RX, RY]) })];

  // ---- shadow on the cloud (shrinks when the robot floats up)
  const shadow = layer("shadow", [
    group("shadow-ellipse", [ell(0, 0, 110, 16), fill(hex("#5B3FA8"), 45)], {
      p: st([RX, 346]),
      s: kf([[0, [100, 100]], [30, [82, 82]], [60, [100, 100]], [90, [82, 82]], [120, [100, 100]]]),
    }),
  ]);

  // ---- cloud (the "remote machine")
  const cloudBob = kf([[0, [0, -3, 0]], [30, [0, 3, 0]], [60, [0, -3, 0]], [90, [0, 3, 0]], [120, [0, -3, 0]]]);
  const cloudLayer = layer(
    "cloud",
    [
      group("cloud-shape", [
        ell(190, 410, 84, 84),
        ell(262, 392, 116, 116),
        ell(334, 410, 80, 80),
        rect(262, 428, 232, 58, 29),
        fill(cloud()),
      ]),
      // status lights on the cloud
      group("led-1", [ell(0, 0, 9, 9), fill(hex("#35F2C8"))], { p: st([218, 440]) }),
      group("led-2", [ell(0, 0, 9, 9), fill(hex("#FFA21F"))], { p: st([240, 440]) }),
      group("led-3", [ell(0, 0, 9, 9), fill(hex("#FF7AAE"))], { p: st([262, 440]) }),
    ],
    { p: cloudBob }
  );

  // ---- gear (the build)
  const gear = layer("gear", [
    group(
      "gear-group",
      [
        { ty: "sr", sy: 1, pt: st(10), p: st([0, 0]), r: st(0), ir: st(24), or: st(33), is: st(0), os: st(0) },
        ell(0, 0, 54, 54),
        fill(gearC()),
      ],
      { p: st([408, 312]), r: kf([[0, 0], [120, 360]], LINEAR) }
    ),
    group("gear-hole", [ell(0, 0, 18, 18), fill(bg())], { p: st([408, 312]) }),
  ]);

  // ---- sparkles
  const star = (nm, x, y, size, frames) =>
    group(
      nm,
      [
        { ty: "sr", sy: 1, pt: st(4), p: st([0, 0]), r: st(0), ir: st(size * 0.28), or: st(size), is: st(0), os: st(0) },
        fill(hex("#FFE9A8")),
      ],
      { p: st([x, y]), s: kf(frames.map(([t, v]) => [t, [v, v]])), r: kf([[0, 0], [120, 90]], LINEAR) }
    );
  const sparkles = layer("sparkles", [
    star("sparkle-a", 86, 132, 16, [[0, 0], [16, 100], [32, 0], [60, 0], [78, 100], [96, 0], [120, 0]]),
    star("sparkle-b", 430, 108, 20, [[0, 0], [10, 0], [28, 100], [46, 0], [70, 0], [88, 100], [106, 0], [120, 0]]),
    star("sparkle-c", 70, 318, 12, [[0, 100], [18, 0], [50, 0], [68, 100], [86, 0], [110, 0], [120, 100]]),
    star("sparkle-d", 448, 226, 11, [[0, 0], [34, 0], [50, 100], [66, 0], [100, 0], [112, 100], [120, 0]]),
  ]);

  // ---- soft glow behind the robot + background
  const glow = layer("glow", [
    group("glow-circle", [ell(RX, 210, 330, 330), fill(hex("#7F52FF"))], {
      o: kf([[0, 16], [60, 28], [120, 16]]),
    }),
  ]);
  const background = layer("background", [group("background-group", [rect(256, 256, 512, 512, 0), fill(bg())])]);

  const doc = {
    v: "5.7.0",
    fr: FR,
    ip: 0,
    op: OP,
    w: 512,
    h: 512,
    nm: "rkotlin mascot: Kodi the build robot",
    assets: [],
    ...(withSlots ? { slots } : {}),
    layers: [sparkles, robot, shadow, gear, cloudLayer, glow, background],
  };
  return doc;
}

const controls = {
  controls: [
    { sid: "bgColor", label: "Background color" },
    { sid: "cloudColor", label: "Cloud color" },
    { sid: "headColor", label: "Head color" },
    { sid: "eyeColor", label: "Eye color" },
    { sid: "gearColor", label: "Gear color" },
  ],
};

const sceneDir = join("public", "projects", "main-project", "scene-1");
mkdirSync(sceneDir, { recursive: true });
writeFileSync(join(sceneDir, "lottie.json"), JSON.stringify(build(true)));
writeFileSync(join(sceneDir, "controls.json"), JSON.stringify(controls, null, 2));
console.log("wrote", sceneDir);

const appOut = process.argv[2];
if (appOut) {
  mkdirSync(appOut, { recursive: true });
  const f = join(appOut, "mascot-lottie.json");
  mkdirSync(dirname(f), { recursive: true });
  writeFileSync(f, JSON.stringify(build(false)));
  console.log("wrote", f);
}
