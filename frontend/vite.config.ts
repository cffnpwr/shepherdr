import { copyFileSync, mkdirSync } from "node:fs";
import { dirname, resolve } from "node:path";
import process from "node:process";

import type { Plugin } from "vite";

import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

const host = process.env.TAURI_DEV_HOST;

// @fontsource/m-plus-1p is npm-managed and updated by Renovate, so its bundled
// LICENSE is copied verbatim at build time instead of a manual copy in public/
// that could silently drift from the font files it covers.
const copyMPlus1pLicense = (): Plugin => {
  let outDir: string;
  return {
    name: "copy-m-plus-1p-license",
    apply: "build",
    configResolved(config) {
      outDir = resolve(config.root, config.build.outDir);
    },
    closeBundle() {
      const dest = resolve(outDir, "licenses/m-plus-1p/LICENSE");
      mkdirSync(dirname(dest), { recursive: true });
      copyFileSync(
        resolve(
          import.meta.dirname,
          "node_modules/@fontsource/m-plus-1p/LICENSE",
        ),
        dest,
      );
    },
  };
};

// https://v2.tauri.app/start/frontend/vite/
export default defineConfig({
  plugins: [react(), tailwindcss(), copyMPlus1pLicense()],
  resolve: {
    alias: {
      "@": resolve(import.meta.dirname, "./src"),
    },
  },
  css: {
    postcss: {
      plugins: [
        {
          postcssPlugin: "drop-woff-fallback",
          // @fontsource/m-plus-1p's index.css lists a .woff fallback next to
          // every .woff2 src for legacy browsers. The app only ships in
          // Tauri's WebKit WebView, which supports woff2, so drop the
          // fallback here. This must run as a postcss `Once` visitor: Vite's
          // own url-to-asset resolution also runs in `Once` (which always
          // executes before per-node visitors like `Declaration` regardless
          // of plugin array position), so a `Declaration` visitor here would
          // edit the CSS text too late to stop the .woff files from already
          // having been resolved and emitted as build assets.
          Once(root) {
            root.walkDecls("src", (decl) => {
              decl.value = decl.value.replace(
                /,\s*url\([^)]*\)\s*format\((['"])woff\1\)/g,
                "",
              );
            });
          },
        },
      ],
    },
  },

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
        protocol: "ws",
        host,
        port: 1421,
      }
      : undefined,
  },
  envPrefix: ["VITE_", "TAURI_ENV_*"],
  build: {
    target:
      process.env.TAURI_ENV_PLATFORM === "windows" ? "chrome105" : "safari13",
    minify: !process.env.TAURI_ENV_DEBUG,
    sourcemap: !!process.env.TAURI_ENV_DEBUG,
  },
});
