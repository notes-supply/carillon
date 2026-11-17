import { svelte } from "@sveltejs/vite-plugin-svelte";
import { defineConfig } from "vite";

export default defineConfig({
  base: "./",
  plugins: [svelte()],
  build: {
    outDir: "dist",
    emptyOutDir: true,
    assetsDir: "",
    assetsInlineLimit: 0,
    modulePreload: false,
    cssCodeSplit: false,
    target: "es2024",
  },
});
