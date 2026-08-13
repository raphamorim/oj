import { useEffect, useRef } from "react";

// A generative pixel field: a grid of colored cells driven by animated value
// noise, quantized to a small palette and dithered so it thins out along a
// diagonal (dense colour top-left, scattered cells bottom-right). Renders
// client-side into a <canvas>; the server just ships an empty canvas.

// Palette flows warm -> cool -> ink. The orange is the "oj" nod.
const PALETTE = ["#e5382b", "#e2571f", "#f4b41a", "#f5d90a", "#2f56e6", "#1b2a6b", "#0f0e0c"];

// --- compact 2D value noise + fbm (seeded, deterministic per mount) ---
function makeNoise() {
  const perm = new Uint8Array(512);
  const base = Array.from({ length: 256 }, (_, i) => i);
  for (let i = 255; i > 0; i--) {
    const j = (Math.random() * (i + 1)) | 0;
    const tmp = base[i]; base[i] = base[j]; base[j] = tmp;
  }
  for (let i = 0; i < 512; i++) perm[i] = base[i & 255];
  const fade = (t: number) => t * t * t * (t * (t * 6 - 15) + 10);
  const lerp = (a: number, b: number, t: number) => a + (b - a) * t;
  const grad = (h: number, x: number, y: number) => ((h & 1 ? -x : x) + (h & 2 ? -y : y));
  return (x: number, y: number) => {
    const xi = Math.floor(x) & 255, yi = Math.floor(y) & 255;
    const xf = x - Math.floor(x), yf = y - Math.floor(y);
    const u = fade(xf), v = fade(yf);
    const aa = perm[perm[xi] + yi], ab = perm[perm[xi] + yi + 1];
    const ba = perm[perm[xi + 1] + yi], bb = perm[perm[xi + 1] + yi + 1];
    const x1 = lerp(grad(aa, xf, yf), grad(ba, xf - 1, yf), u);
    const x2 = lerp(grad(ab, xf, yf - 1), grad(bb, xf - 1, yf - 1), u);
    return (lerp(x1, x2, v) + 1) * 0.5; // 0..1
  };
}

export function PixelField({ height = 420 }: { height?: number }) {
  const ref = useRef<HTMLCanvasElement | null>(null);

  useEffect(() => {
    const canvas = ref.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    const noise = makeNoise();
    const noise2 = makeNoise();
    const dark = window.matchMedia?.("(prefers-color-scheme: dark)").matches;
    const bg = dark ? "#0f0e0c" : "#ffffff";
    const cell = 13; // px per cell (css pixels)
    let cols = 0, rows = 0, dpr = 1, raf = 0, last = 0;

    function resize() {
      const rect = canvas.getBoundingClientRect();
      dpr = Math.min(window.devicePixelRatio || 1, 2);
      canvas.width = Math.floor(rect.width * dpr);
      canvas.height = Math.floor(rect.height * dpr);
      cols = Math.ceil(rect.width / cell);
      rows = Math.ceil(rect.height / cell);
    }

    const reduce = window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;

    function draw(t: number) {
      const c = cell * dpr;
      ctx.fillStyle = bg;
      ctx.fillRect(0, 0, canvas.width, canvas.height);
      for (let y = 0; y < rows; y++) {
        for (let x = 0; x < cols; x++) {
          // diagonal coordinate: 0 top-left -> 1 bottom-right
          const d = (x / cols + y / rows) * 0.5;
          // flowing field
          const f = noise(x * 0.05 + t * 0.04, y * 0.05 - t * 0.03);
          // density mask: dense near top-left, scattered toward bottom-right
          const m = noise2(x * 0.08 - t * 0.02, y * 0.08 + t * 0.02);
          if (m < d * 1.15 - 0.15) continue; // skip -> background shows through
          // colour index: warm (top-left) sweeping to ink (bottom-right)
          const v = d * 0.62 + f * 0.55 - 0.08;
          const idx = Math.max(0, Math.min(PALETTE.length - 1, Math.floor(v * PALETTE.length)));
          ctx.fillStyle = PALETTE[idx];
          ctx.fillRect(Math.floor(x * c), Math.floor(y * c), Math.ceil(c) - dpr, Math.ceil(c) - dpr);
        }
      }
    }

    function frame(now: number) {
      // ~24fps evolution keeps it calm and cheap
      if (now - last > 42) {
        last = now;
        draw(now / 1000);
      }
      raf = requestAnimationFrame(frame);
    }

    resize();
    if (reduce) {
      draw(12); // a single static frame
    } else {
      raf = requestAnimationFrame(frame);
    }
    const onResize = () => resize();
    window.addEventListener("resize", onResize);
    return () => {
      cancelAnimationFrame(raf);
      window.removeEventListener("resize", onResize);
    };
  }, []);

  return <canvas ref={ref} className="pixelfield" style={{ height }} aria-hidden="true" />;
}
