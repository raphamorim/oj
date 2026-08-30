// SPDX-License-Identifier: MIT

// The scripts TanStack Start asks a dev server to put in the document head
// before the client entry. Vite fills this by running transformIndexHtml over
// an empty document and collecting the inline scripts it gets back -- the
// @vitejs/plugin-react refresh preamble, plus whatever a user's
// transformIndexHtml added.
//
// Neither applies here: the adapter applies Fast Refresh itself through the
// preamble its own client entry carries, and the Start path has no
// transformIndexHtml. So there is nothing to inject, which is what
// start-server-core does with a falsy value anyway. What it does not tolerate
// is the module failing to resolve, and it imports this one unconditionally
// whenever TSS_DEV_SERVER is set -- which runner.mjs sets.
export const injectedHeadScripts: string | undefined = undefined;
