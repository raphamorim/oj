import type { ReactNode } from "react";

// Root layout: wraps every route. Its header persists across all navigations.
export default function RootLayout({ children }: { children: ReactNode }) {
  return (
    <div data-layout="root">
      <header data-app-header>oj app</header>
      {children}
    </div>
  );
}
