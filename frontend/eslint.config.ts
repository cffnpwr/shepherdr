import cffnpwrConfig from "@cffnpwr/eslint-config";
import tsEslintParser from "@typescript-eslint/parser";
import { defineConfig } from "eslint/config";
import globals from "globals";

const files = ["**/*.{js,jsx,ts,tsx}"];
const srcFiles = ["src/**/*.{js,jsx,ts,tsx}"];
const bunFiles = ["vite.config.ts", "eslint.config.ts"];

export default defineConfig([
  {
    ignores: ["dist/**"],
  },
  {
    files: srcFiles,
    languageOptions: {
      globals: {
        ...globals.browser,
      },
      parser: tsEslintParser,
      parserOptions: {
        project: ["./tsconfig.json"],
        tsconfigRootDir: import.meta.dirname,
      },
    },
  },
  {
    files: bunFiles,
    languageOptions: {
      globals: {
        ...globals.bunBuiltin,
      },
      parser: tsEslintParser,
      parserOptions: {
        project: ["./tsconfig.bun.json"],
        tsconfigRootDir: import.meta.dirname,
      },
    },
  },
  {
    files,
    extends: cffnpwrConfig({ react: true, tailwind: true }),
    settings: {
      // Points eslint-plugin-tailwindcss and eslint-plugin-better-tailwindcss at this project's
      // Tailwind v4 CSS entry point.
      tailwindcss: {
        cssConfigPath: "./src/index.css",
      },
      "better-tailwindcss": {
        entryPoint: "./src/index.css",
      },
    },
  },
]);
