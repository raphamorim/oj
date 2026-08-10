// Imports a plugin-provided virtual module (resolveId + load). Kept out of the
// app graph so it's only compiled when fetched directly (unbundled dev).
import { info } from "virtual:plugin-greeting";

export default info;
