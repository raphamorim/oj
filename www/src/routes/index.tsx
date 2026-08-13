import { createRoute, Link } from "@tanstack/react-router";

import { rootRoute } from "./__root";
import { Cmd } from "../components/site";

const GITHUB = "https://github.com/raphamorim/oj";

const FEATURES = [
  {
    idx: "01",
    title: "Rust at the core",
    body: "TS and JSX are transformed by oxc, the same Rust toolchain that powers modern bundlers. Compiled once, cached content-addressed, served warm.",
  },
  {
    idx: "02",
    title: "Server rendering, to the edge",
    body: "Streaming SSR and server functions through the TanStack Start adapter, with a Cloudflare Worker and static assets emitted by a single build.",
  },
  {
    idx: "03",
    title: "Faithful to the ecosystem",
    body: "It runs your vite.config plugins and honors the conventions you already write: aliases, tsconfig paths, import.meta.glob, ?url and ?raw, svgr, MDX.",
  },
];

const STACK = [
  "TanStack Start",
  "React 19",
  "Tailwind v4",
  "Cloudflare Workers",
  "TypeScript",
  "Server functions",
  "import.meta.glob",
  "CSS Modules",
  "svgr · MDX",
  "CommonJS interop",
];

function Home() {
  return (
    <>
      {/* hero */}
      <section className="hero">
        <div className="wrap">
          <p className="eyebrow eyebrow--accent rise rise-1">
            A Rust-native build tool for React
          </p>
          <h1 className="hero__title rise rise-2">Craft, compiled.</h1>
          <p className="hero__lede rise rise-3">
            oj builds React apps with a Rust core: a fast dev server, server
            rendering with TanStack Start, Tailwind, and one-command Cloudflare
            deploys. Compatible with the tools you already use.
          </p>
          <div
            className="rise rise-4"
            style={{ marginTop: "2.25rem", display: "flex", flexWrap: "wrap", gap: "0.9rem", alignItems: "center" }}
          >
            <Link to="/getting-started" className="btn btn--solid">
              Get started
            </Link>
            <a className="btn btn--ghost" href={GITHUB} target="_blank" rel="noreferrer">
              View on GitHub
            </a>
            <Cmd>cargo install oj</Cmd>
          </div>
        </div>
      </section>

      {/* features */}
      <section className="section">
        <div className="wrap">
          <div className="section__head">
            <p className="eyebrow">What you get</p>
            <h2 className="section__title">A build tool that respects the craft.</h2>
            <p className="section__sub">
              Speed where it counts, without asking you to relearn your project.
            </p>
          </div>
          <div className="cards cards--3" style={{ marginTop: "2.5rem" }}>
            {FEATURES.map((f) => (
              <div key={f.idx} className="card">
                <div className="card__idx">{f.idx}</div>
                <div className="card__title">{f.title}</div>
                <p className="card__body">{f.body}</p>
              </div>
            ))}
          </div>
        </div>
      </section>

      {/* supported stack */}
      <section className="section" style={{ paddingTop: 0 }}>
        <div className="wrap">
          <hr className="rule" style={{ marginBottom: "clamp(2.5rem, 6vw, 4rem)" }} />
          <div
            style={{ display: "grid", gap: "clamp(1.5rem,4vw,3rem)", gridTemplateColumns: "1fr", alignItems: "start" }}
          >
            <div className="section__head">
              <p className="eyebrow">Built for the modern stack</p>
              <h2 className="section__title">Everything a real app needs.</h2>
            </div>
            <ul
              style={{
                display: "flex",
                flexWrap: "wrap",
                gap: "0.6rem 0.7rem",
                listStyle: "none",
                padding: 0,
                margin: 0,
              }}
            >
              {STACK.map((item) => (
                <li
                  key={item}
                  style={{
                    fontFamily: "var(--font-mono)",
                    fontSize: "0.82rem",
                    color: "var(--color-muted)",
                    border: "1px solid var(--color-line)",
                    borderRadius: "0.5rem",
                    padding: "0.45rem 0.8rem",
                    background: "var(--color-surface)",
                  }}
                >
                  {item}
                </li>
              ))}
            </ul>
          </div>
        </div>
      </section>

      {/* closing CTA */}
      <section className="section" style={{ paddingTop: 0 }}>
        <div className="wrap">
          <div
            style={{
              border: "1px solid var(--color-line)",
              borderRadius: "1rem",
              padding: "clamp(2rem, 6vw, 4rem)",
              background: "var(--color-surface)",
            }}
          >
            <p className="eyebrow eyebrow--accent">Start in a minute</p>
            <h2 className="section__title" style={{ maxWidth: "18ch" }}>
              Point oj at your app and run.
            </h2>
            <div style={{ marginTop: "1.75rem", display: "flex", flexWrap: "wrap", gap: "0.9rem" }}>
              <Cmd>oj dev</Cmd>
              <Cmd>oj build</Cmd>
            </div>
            <div style={{ marginTop: "1.75rem" }}>
              <Link to="/getting-started" className="btn btn--solid">
                Read the guide
              </Link>
            </div>
          </div>
        </div>
      </section>
    </>
  );
}

export const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  component: Home,
});
