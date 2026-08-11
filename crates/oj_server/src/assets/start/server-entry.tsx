import { createStartHandler, defaultStreamHandler } from "@tanstack/react-start/server";
const handler = createStartHandler(defaultStreamHandler);
export default {
  async fetch(request: Request): Promise<Response> {
    return await handler(request);
  },
};
