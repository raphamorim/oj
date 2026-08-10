import { getLikes } from "@/store";
import { Likes, PendingBar, type RouteData } from "@/ui";
import type { LoaderArgs } from "@/router";

// Dynamic segment: `$id` in the filename becomes params.id.
export function loader({ params }: LoaderArgs) {
  return { id: params.id, likes: getLikes() };
}

// Title from the route param.
export function meta({ params }: LoaderArgs) {
  return [{ title: `User ${params.id} - oj` }];
}

export default function User({ data, params }: { data: RouteData; params: Record<string, string> }) {
  const next = Number(params.id) + 1;
  return (
    <main data-page="user">
      <h1>
        user <span data-user-id={params.id}>{String(data?.id ?? params.id)}</span>
      </h1>
      <Likes data={data} />
      <PendingBar />
      <a href={`/users/${next}`}>next user</a> <a href="/">home</a>
    </main>
  );
}
