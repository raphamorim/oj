import { useState, type ReactNode } from "react";

// Section layout: wraps only /users/* routes. Its local state persists while
// navigating between user pages (the layout isn't remounted), demonstrating
// nested-layout persistence.
export default function UsersLayout({ children }: { children: ReactNode }) {
  const [n, setN] = useState(0);
  return (
    <section data-layout="users">
      <p>users section</p>
      <button data-layout-inc onClick={() => setN((v) => v + 1)}>
        layout clicks: <span data-layout-count>{n}</span>
      </button>
      {children}
    </section>
  );
}
