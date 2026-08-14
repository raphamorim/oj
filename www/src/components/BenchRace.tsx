import { useEffect, useRef, useState } from "react";

// A "spawn -> paint" race, visualized. Each lane is a pixelated wavefront that
// sweeps to PAINT at a speed proportional to the REAL measured latency
// (bench/run.mjs: 10k-component dev server, p50, oj --bundle vs Vite 8.2's
// default dev, M-series Mac). oj reaches paint first; the ratio is the story.
// Numbers are the project's committed benchmark, reproducible in bench/.

type Metric = { key: string; label: string; oj: number; vite: number };

const METRICS: Metric[] = [
  { key: "cold", label: "Cold start", oj: 1569, vite: 5468 },
  { key: "warm", label: "Warm start", oj: 1408, vite: 4957 },
  { key: "reload", label: "Full reload", oj: 231, vite: 1604 },
];

const OJ_MS = 900; // oj lane always takes this long on screen; vite scales by ratio
const HOLD = 1100; // pause after the slower lane finishes, before the next metric

export function BenchRace() {
  const ref = useRef<HTMLCanvasElement | null>(null);
  const [idx, setIdx] = useState(0);

  useEffect(() => {
    const canvas = ref.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    const styles = getComputedStyle(document.documentElement);
    const col = (name: string, fb: string) => styles.getPropertyValue(name).trim() || fb;
    const ink = col("--color-ink", "rgba(0,0,0,0.85)");
    const bg = col("--color-bg", "#ffffff");
    const line = col("--color-line", "#e5e5e5");
    const accent = col("--color-accent", "#2a33d4"); // oj lane: cobalt
    const vite = "#b6b4ae"; // vite lane: muted grey, so oj visibly wins on colour too
    const faint = col("--color-faint", "rgba(0,0,0,0.35)");
    const reduce = window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;

    let dpr = 1, w = 0, h = 0, raf = 0;
    const cell = 12;
    let metric = 0;
    let runStart = performance.now();

    function resize() {
      const r = canvas.getBoundingClientRect();
      dpr = Math.min(window.devicePixelRatio || 1, 2);
      w = r.width; h = r.height;
      canvas.width = Math.floor(w * dpr);
      canvas.height = Math.floor(h * dpr);
    }

    // draw one lane's pixelated wavefront filling to `p` (0..1)
    function lane(x: number, y: number, width: number, rows: number, p: number, color: string, done: boolean) {
      const cols = Math.floor(width / cell);
      const front = p * cols;
      for (let r = 0; r < rows; r++) {
        for (let c = 0; c < cols; c++) {
          const lit = c <= front;
          // dithered leading edge for texture
          const edge = front - c;
          if (lit && edge < 2.2 && ((c * 7 + r * 13 + (front | 0) * 5) % 5 === 0)) continue;
          const px = Math.floor((x + c * cell) * dpr);
          const py = Math.floor((y + r * cell) * dpr);
          const s = Math.ceil(cell * dpr) - dpr;
          if (lit) {
            ctx.fillStyle = done ? color : (edge < 3 ? color : color);
            ctx.globalAlpha = edge < 1.5 ? 1 : 0.92;
          } else {
            ctx.fillStyle = line;
            ctx.globalAlpha = 0.5;
          }
          ctx.fillRect(px, py, s, s);
        }
      }
      ctx.globalAlpha = 1;
    }

    function draw(now: number) {
      const m = METRICS[metric];
      const viteMs = OJ_MS * (m.vite / m.oj);
      const t = now - runStart;
      const ojP = reduce ? 1 : Math.min(1, t / OJ_MS);
      const viteP = reduce ? 1 : Math.min(1, t / viteMs);

      ctx.fillStyle = bg;
      ctx.fillRect(0, 0, canvas.width, canvas.height);

      const padX = 14, labelW = 70, paintW = 8;
      const trackX = padX + labelW;
      const trackW = w - trackX - padX - paintW;
      const rows = 3;
      const laneH = rows * cell;
      const gap = 34;
      const topY = (h - (laneH * 2 + gap)) / 2;

      // lane labels
      ctx.fillStyle = ink;
      ctx.font = `600 ${13 * dpr}px ${styles.getPropertyValue("--font-mono") || "monospace"}`;
      ctx.textBaseline = "middle";
      ctx.fillText("OJ", padX * dpr, (topY + laneH / 2) * dpr);
      ctx.fillStyle = faint;
      ctx.fillText("VITE", padX * dpr, (topY + laneH + gap + laneH / 2) * dpr);

      lane(trackX, topY, trackW, rows, ojP, accent, ojP >= 1);
      lane(trackX, topY + laneH + gap, trackW, rows, viteP, vite, viteP >= 1);

      // finish line
      ctx.fillStyle = line;
      ctx.fillRect(Math.floor((trackX + trackW + 2) * dpr), Math.floor(topY * dpr), dpr, Math.ceil((laneH * 2 + gap) * dpr));

      // advance to next metric after the slower lane finishes + hold
      if (!reduce && t > viteMs + HOLD) {
        metric = (metric + 1) % METRICS.length;
        runStart = now;
        setIdx(metric);
      }
    }

    function frame(now: number) { draw(now); raf = requestAnimationFrame(frame); }
    resize();
    if (reduce) { draw(performance.now()); } else { raf = requestAnimationFrame(frame); }
    const onResize = () => resize();
    window.addEventListener("resize", onResize);
    return () => { cancelAnimationFrame(raf); window.removeEventListener("resize", onResize); };
  }, []);

  const m = METRICS[idx];
  const ratio = (m.vite / m.oj).toFixed(1);

  return (
    <div className="bench">
      <div className="bench__meta">
        <span className="bench__metric">{m.label}</span>
        <span className="bench__ratio">{ratio}× faster</span>
        <span className="bench__ms">
          oj <b>{m.oj}ms</b> · Vite <b>{m.vite}ms</b>
        </span>
      </div>
      <canvas ref={ref} className="bench__canvas" aria-hidden="true" />
    </div>
  );
}
