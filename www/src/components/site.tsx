import { useEffect, useState, type ReactNode } from "react";
import { Link, useRouterState } from "@tanstack/react-router";

const GITHUB = "https://github.com/raphamorim/oj";

export function Wordmark() {
  return (
    <Link to="/" className="mark" aria-label="oj home">
      oj<span className="mark__dot">.</span>
    </Link>
  );
}

const NAV = [
  { to: "/getting-started", label: "Get started" },
  { to: "/architecture", label: "Architecture" },
  { to: "/features", label: "Features" },
] as const;

export function Nav() {
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const [scrolled, setScrolled] = useState(false);
  useEffect(() => {
    const onScroll = () => setScrolled(window.scrollY > 8);
    onScroll();
    window.addEventListener("scroll", onScroll, { passive: true });
    return () => window.removeEventListener("scroll", onScroll);
  }, []);

  return (
    <header className="nav" data-scrolled={scrolled}>
      <div className="wrap nav__inner">
        <Wordmark />
        <nav className="nav__links">
          {NAV.map((item) => (
            <Link
              key={item.to}
              to={item.to}
              className="nav__link"
              data-active={pathname === item.to}
            >
              {item.label}
            </Link>
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
          oj is an open-source project. Built with oj, deployed on Cloudflare.
        </span>
        <div className="foot__links">
          <Link to="/getting-started" className="foot__link">Docs</Link>
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

const DOC_NAV = [
  {
    label: "Start",
    links: [{ to: "/getting-started", label: "Getting started" }],
  },
  {
    label: "Reference",
    links: [
      { to: "/architecture", label: "Architecture" },
      { to: "/features", label: "Features" },
    ],
  },
] as const;

export function DocsLayout({ children }: { children: ReactNode }) {
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  return (
    <div className="wrap docs">
      <aside className="sidebar">
        {DOC_NAV.map((group) => (
          <div key={group.label} className="sidebar__group">
            <div className="sidebar__label">{group.label}</div>
            {group.links.map((link) => (
              <Link
                key={link.to}
                to={link.to}
                className="sidebar__link"
                data-active={pathname === link.to}
              >
                {link.label}
              </Link>
            ))}
          </div>
        ))}
      </aside>
      <main className="prose">{children}</main>
    </div>
  );
}
