import { useState, type ReactNode } from "react";
import type { RouteData } from "@/ui";

// The users-section layout has its own loader (a section-level datum, distinct
// from the page's data), and local state that persists across navigations
// within /users/*.
export function loader() {
  return { userCount: 3 };
}

export default function UsersLayout({ children, data }: { children: ReactNode; data: RouteData }) {
  const [n, setN] = useState(0);
  return (
    <section data-layout="users">
      <p>
        users section (<span data-users-count={String(data?.userCount ?? "?")}>{String(data?.userCount ?? "?")}</span>{" "}
        total)
      </p>
      <button data-layout-inc onClick={() => setN((v) => v + 1)}>
        layout clicks: <span data-layout-count>{n}</span>
      </button>
      {children}
    </section>
  );
}
