import React from "react";
import { Routes, Route, Link, useLocation } from "react-router-dom";
import { format, addDays, differenceInCalendarDays } from "date-fns";
import { debounce, groupBy, chunk, capitalize } from "lodash-es";
import clsx from "clsx";
import { twMerge } from "tailwind-merge";

// Touch each dependency so its module graph is actually loaded. In unbundled
// dev, date-fns and lodash-es each fan out to hundreds of tiny files; partial
// bundling collapses each package to a single /@oj-pkg request.
const today = new Date(2026, 0, 1);
const rows = chunk(
  groupBy(
    [1, 2, 3, 4, 5, 6].map((n) => ({
      n,
      day: format(addDays(today, n), "yyyy-MM-dd"),
      label: capitalize(`row ${n}`),
    })),
    (r) => (r.n % 2 === 0 ? "even" : "odd"),
  ).odd,
  2,
);

function Home() {
  const loc = useLocation();
  const cls = twMerge(clsx("p-4", "p-2", "text-sm"));
  const log = debounce(() => {}, 50);
  React.useEffect(() => log(), [log]);
  const spanDays = differenceInCalendarDays(addDays(today, 6), today);
  return (
    <div className={cls} data-path={loc.pathname}>
      <h1>pbench</h1>
      <p>span: {spanDays} days</p>
      <ul>
        {rows.flat().map((r) => (
          <li key={r.n}>
            {r.label} — {r.day}
          </li>
        ))}
      </ul>
      <Link to="/about">about</Link>
    </div>
  );
}

export default function App() {
  return (
    <Routes>
      <Route path="/" element={<Home />} />
      <Route path="/about" element={<div>about</div>} />
    </Routes>
  );
}
