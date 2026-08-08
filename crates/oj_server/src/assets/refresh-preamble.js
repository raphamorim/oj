// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// oj Fast Refresh preamble. Injected as the FIRST module script in the HTML
// so injectIntoGlobalHook runs before React is ever imported — the runtime
// hooks the renderer through the DevTools global hook at React load time.
import { injectIntoGlobalHook } from "/@oj/refresh-runtime.js";
injectIntoGlobalHook(window);
// Fallback no-ops for modules without refresh glue (non-component files).
window.$RefreshReg$ = () => {};
window.$RefreshSig$ = () => (type) => type;
window.__oj_refresh_installed__ = true;
// Node-isms that survive define-replacement in CJS deps. NODE_ENV itself is
// AST-replaced server-side; these are the safety net for stray references.
window.process ??= { env: { NODE_ENV: "development" } };
window.global ??= window;
