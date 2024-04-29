import { vercel } from "@t3-oss/env-core/presets";
import { createEnv } from "@t3-oss/env-nextjs";
import { z } from "zod";

const env = createEnv({
  extends: [vercel()],

  /**
   * Specify server-side environment variables schema here.
   */
  server: {
    GITHUB_PAT: z.string(),
    NODE_ENV: z.enum(["development", "test", "production"]).default("development"),
  },

  /**
   * Specify client-side environment variables schema here.
   * To expose them to the client, prefix them with `NEXT_PUBLIC_`.
   */
  client: {},

  /**
   * You can't destruct `process.env` as a regular object in the Next.js edge runtimes
   * (e.g. middlewares) or client-side so we need to destruct manually.
   */
  runtimeEnv: {
    NODE_ENV: process.env.NODE_ENV,
    GITHUB_PAT: process.env.GITHUB_PAT,
  },
  /**
   * Makes it so that empty strings are treated as undefined.
   * `SOME_VAR: z.string()` and `SOME_VAR=''` will throw an error.
   */
  emptyStringAsUndefined: true,
});

export default env;
