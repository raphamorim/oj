import { useEffect, useState, type ReactNode } from "react";

const GITHUB = "https://github.com/raphamorim/oj";

const SECTIONS = [
  { id: "process", label: "How it works" },
  { id: "features", label: "Features" },
  { id: "architecture", label: "Architecture" },
  { id: "start", label: "Get started" },
] as const;

export function Nav() {
  const [active, setActive] = useState("");
  const [scrolled, setScrolled] = useState(false);

  useEffect(() => {
    const onScroll = () => setScrolled(window.scrollY > 8);
    onScroll();
    window.addEventListener("scroll", onScroll, { passive: true });

    // Scroll-spy: highlight the section nearest the middle of the viewport.
    const obs = new IntersectionObserver(
      (entries) => {
        for (const e of entries) if (e.isIntersecting) setActive(e.target.id);
      },
      { rootMargin: "-45% 0px -50% 0px" },
    );
    for (const s of SECTIONS) {
      const el = document.getElementById(s.id);
      if (el) obs.observe(el);
    }
    return () => {
      window.removeEventListener("scroll", onScroll);
      obs.disconnect();
    };
  }, []);

  return (
    <header className="nav" data-scrolled={scrolled}>
      <div className="wrap nav__inner">
        <a href="#top" className="mark" aria-label="oj">
          oj<span className="mark__dot">.</span>
        </a>
        <nav className="nav__links">
          {SECTIONS.map((s) => (
            <a key={s.id} href={`#${s.id}`} className="nav__link" data-active={active === s.id}>
              {s.label}
            </a>
          ))}
          <a className="nav__link" href={GITHUB} target="_blank" rel="noreferrer">
            GitHub
          </a>
        </nav>
      </div>
    </header>
  );
}

export function Footer() {
  return (
    <footer className="foot">
      <div className="wrap foot__row">
        <span className="foot__note">
          oj is an open-source project. This site is built with oj and deployed on Cloudflare.
        </span>
        <div className="foot__links">
          <a className="foot__link" href="#start">Get started</a>
          <a className="foot__link" href={GITHUB} target="_blank" rel="noreferrer">GitHub</a>
          <a className="foot__link" href={`${GITHUB}/issues`} target="_blank" rel="noreferrer">Issues</a>
        </div>
      </div>
    </footer>
  );
}

/** A command line with a copyable feel: `$ oj dev`. */
export function Cmd({ children }: { children: ReactNode }) {
  return (
    <span className="cmd">
      <span className="cmd__prompt">$</span>
      <span>{children}</span>
    </span>
  );
}
