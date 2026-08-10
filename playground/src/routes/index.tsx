import { SsrApp } from "@/ssr-app";
import { addLike, getLikes } from "@/store";
import { Likes, PendingBar, type RouteData } from "@/ui";
import type { LoaderArgs } from "@/router";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

export function loader(_args: LoaderArgs) {
  return { likes: getLikes() };
}

export async function action(_args: LoaderArgs) {
  await sleep(100); // slow enough to observe the "saving…" state
  addLike();
}

export default function Home({ data }: { data: RouteData }) {
  return (
    <main data-page="home">
      <SsrApp />
      <Likes data={data} />
      <PendingBar />
      <nav>
        {/* /about opts out so navigating to it stays observable (pending
            state); /boom opts out so its failing loader isn't prefetched; the
            rest viewport-prefetch. */}
        <a href="/about" data-no-prefetch>
          about
        </a>{" "}
        <a href="/users/42">user 42</a> <a href="/crash">crash</a>{" "}
        <a href="/boom" data-no-prefetch>
          boom
        </a>
      </nav>
      {/* A tall spacer pushes this link below the fold so it is only prefetched
          on hover, not on viewport entry. */}
      <div style={{ height: "150vh" }} />
      <a href="/deep" data-deep-link>
        deep
      </a>
    </main>
  );
}
