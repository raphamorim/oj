import { getLikes } from "@/store";
import { Likes, PendingBar, type RouteData } from "@/ui";
import type { LoaderArgs } from "@/router";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

export async function loader(_args: LoaderArgs) {
  await sleep(200); // slow enough to observe the "loading" state
  return { likes: getLikes() };
}

export function meta() {
  return [
    { title: "About - oj" },
    { name: "description", content: "the about page" },
    { property: "og:title", content: "About oj" },
  ];
}

export default function About({ data }: { data: RouteData }) {
  return (
    <main data-page="about">
      <h1>About</h1>
      <Likes data={data} />
      <PendingBar />
      <a href="/">home</a>
    </main>
  );
}
